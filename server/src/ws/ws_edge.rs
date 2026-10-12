// ==============================================================================
// Copyright (C) 2026 Guilherme Pedroza
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.
// ==============================================================================

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use axum::{
    extract::{State, WebSocketUpgrade},
    response::IntoResponse,
};
use chrono::{Duration, Utc};
use futures_util::StreamExt;
use uuid::Uuid;

use crate::reservation_store::{self, CreateError, FinishReason};
use crate::state::SharedState;
use crate::ws::messages::{EdgeToServerMsg, EdgeTransportMsg, ServerToAppMsg, ServerToEdgeMsg};

pub async fn ws_edge_handler(
    ws: WebSocketUpgrade,
    headers: axum::http::HeaderMap,
    State(state): State<SharedState>,
) -> axum::response::Response {
    let Some(token) = headers
        .get("Authorization")
        .and_then(|header| header.to_str().ok())
        .and_then(|header| header.strip_prefix("Bearer "))
    else {
        return (
            axum::http::StatusCode::UNAUTHORIZED,
            "Invalid gateway credential",
        )
            .into_response();
    };
    match crate::edge_auth::authenticate(&state.pool, token).await {
        Ok(Some(principal)) => ws.on_upgrade(|socket| handle_edge_socket(socket, state, principal)),
        Ok(None) => (
            axum::http::StatusCode::UNAUTHORIZED,
            "Invalid gateway credential",
        )
            .into_response(),
        Err(error) => {
            tracing::error!(%error, "Gateway authentication unavailable");
            (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                "Gateway authentication unavailable",
            )
                .into_response()
        }
    }
}

fn schedule_unconfirmed_occupancy(state: SharedState, reservation_id: Uuid) {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(15)).await;
        if let Err(error) = reservation_store::finish(
            &state.pool,
            reservation_id,
            FinishReason::UnconfirmedOccupancy,
        )
        .await
        {
            tracing::error!(%error, %reservation_id, "Failed to finish unconfirmed occupancy");
        }
    });
}

async fn revalidate_edge(state: &SharedState, principal: &crate::edge_auth::Principal) -> bool {
    matches!(
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            let tx = crate::edge_auth::begin_authorized(&state.pool, principal).await?;
            tx.commit()
                .await
                .map_err(crate::edge_auth::AuthError::Database)
        })
        .await,
        Ok(Ok(()))
    )
}

async fn watch_edge_session(state: &SharedState, principal: &crate::edge_auth::Principal) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval.tick().await;
    loop {
        interval.tick().await;
        if !revalidate_edge(state, principal).await {
            return;
        }
    }
}

async fn handle_edge_socket(
    socket: WebSocket,
    state: SharedState,
    principal: crate::edge_auth::Principal,
) {
    tokio::select! {
        biased;
        _ = watch_edge_session(&state, &principal) => {
            tracing::warn!(gateway_id = %principal.gateway_id, "Gateway session invalidated");
        }
        _ = run_edge_socket(socket, &state, &principal) => {}
    }
}

async fn run_edge_socket(
    mut socket: WebSocket,
    state: &SharedState,
    principal: &crate::edge_auth::Principal,
) {
    let ready = ServerToEdgeMsg::Ready {
        delivery_version: 1,
    };
    if let Ok(json) = serde_json::to_string(&ready)
        && !matches!(
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                socket.send(Message::Text(json.into()))
            )
            .await,
            Ok(Ok(()))
        )
    {
        return;
    }
    while let Some(Ok(msg)) = socket.next().await {
        if let Message::Text(text) = msg {
            let operation = async {
                if !revalidate_edge(state, principal).await {
                    return false;
                }
                if let Ok(EdgeTransportMsg::Event {
                    version,
                    event_id,
                    payload,
                }) = serde_json::from_str::<EdgeTransportMsg>(&text)
                {
                    if version != 1 {
                        tracing::warn!(%event_id, version, "Unsupported edge delivery version");
                        return false;
                    }
                    let delivery = match crate::edge_delivery::apply(
                        &state.pool,
                        principal,
                        event_id,
                        payload,
                    )
                    .await
                    {
                        Ok(delivery) => delivery,
                        Err(error) => {
                            tracing::error!(%error, %event_id, "Failed to persist edge event");
                            return false;
                        }
                    };
                    if let Some((spot_id, status)) = delivery.spot_update {
                        let update = ServerToAppMsg::SpotUpdate { spot_id, status };
                        if let Ok(json) = serde_json::to_string(&update) {
                            let _ = state.tx.send(json);
                        }
                    }
                    if let Some(id) = delivery.unconfirmed_reservation {
                        schedule_unconfirmed_occupancy(state.clone(), id);
                    }
                    let ack = ServerToEdgeMsg::Ack {
                        version: delivery.ack.version,
                        event_id: delivery.ack.event_id,
                        outcome: delivery.ack.outcome.as_str().to_owned(),
                        reason: delivery.ack.reason,
                        duplicate: delivery.ack.duplicate,
                    };
                    if let Ok(json) = serde_json::to_string(&ack)
                        && !matches!(
                            tokio::time::timeout(
                                std::time::Duration::from_secs(5),
                                socket.send(Message::Text(json.into()))
                            )
                            .await,
                            Ok(Ok(()))
                        )
                    {
                        return false;
                    }
                    return true;
                }
                let Ok(edge_msg) = serde_json::from_str::<EdgeToServerMsg>(&text) else {
                    tracing::warn!("Invalid edge message ignored without ACK");
                    return true;
                };
                match edge_msg {
                    EdgeToServerMsg::CarDetected {
                        plate, camera_id, ..
                    } => {
                        handle_car_detected(state, plate, camera_id).await;
                    }
                    EdgeToServerMsg::SpotUpdate {
                        spot_id,
                        status,
                        camera_id,
                        confidence,
                        ..
                    } => {
                        let observation = match crate::edge_delivery::apply_legacy(
                            &state.pool,
                            principal,
                            &camera_id,
                            &spot_id,
                            &status,
                            confidence,
                        )
                        .await
                        {
                            Ok(observation) => observation,
                            Err(error) => {
                                tracing::error!(%error, gateway_id = %principal.gateway_id, "Legacy edge observation failed");
                                return false;
                            }
                        };
                        if observation.outcome == crate::edge_delivery::Outcome::Rejected {
                            tracing::warn!(gateway_id = %principal.gateway_id, reason = ?observation.reason, "Legacy edge observation rejected without ACK");
                            let _ = socket
                                .send(Message::Close(Some(CloseFrame {
                                    code: 1008,
                                    reason: "Gateway observation rejected".into(),
                                })))
                                .await;
                            return false;
                        }
                        if let Some((spot_id, status)) = observation.spot_update {
                            let update = ServerToAppMsg::SpotUpdate { spot_id, status };
                            if let Ok(json) = serde_json::to_string(&update) {
                                let _ = state.tx.send(json);
                            }
                        }
                        if let Some(id) = observation.unconfirmed_reservation {
                            schedule_unconfirmed_occupancy(state.clone(), id);
                        }
                    }
                    EdgeToServerMsg::PathUpdate {
                        from_node,
                        to_node,
                        is_active,
                    } => {
                        let mut graph_write = state.graph.write().await;
                        graph_write.set_edge_status(&from_node, &to_node, is_active);
                        tracing::info!(
                            "Path update: {} -> {} active: {}",
                            from_node,
                            to_node,
                            is_active
                        );
                    }
                    EdgeToServerMsg::TrendPrediction {
                        timestamp,
                        avg_stay_duration_mins,
                        stay_duration_distribution,
                        max_capacity,
                        model_health,
                        next_24h_occupancy,
                    } => {
                        tracing::info!(
                            "[WS EDGE] TrendPrediction received: {:.1} mins avg stay, {} hours forecast",
                            avg_stay_duration_mins,
                            next_24h_occupancy.len()
                        );
                        let broadcast_msg = ServerToAppMsg::TrendPrediction {
                            timestamp,
                            avg_stay_duration_mins,
                            stay_duration_distribution,
                            max_capacity,
                            model_health,
                            next_24h_occupancy,
                        };
                        if let Ok(json_str) = serde_json::to_string(&broadcast_msg) {
                            if let Ok(mut cache) = state.last_trend_prediction.lock() {
                                *cache = Some(json_str.clone());
                            }
                            let _ = state.tx.send(json_str);
                        }
                    }
                }
                true
            };
            if !matches!(
                tokio::time::timeout(std::time::Duration::from_secs(5), operation).await,
                Ok(true)
            ) {
                break;
            }
        }
    }

    tracing::info!("[WS EDGE] Camera disconnected.");
}

async fn handle_car_detected(state: &SharedState, plate: String, camera_id: String) {
    let hashed_plate = crate::security::hash_plate(&plate, &state.plate_pepper);

    // 1. Validação do usuário
    let user_record = sqlx::query!("SELECT id FROM users WHERE plate = $1", hashed_plate)
        .fetch_optional(&state.pool)
        .await;

    let user_id = match user_record {
        Ok(Some(record)) => record.id,
        _ => {
            tracing::warn!("Veículo não cadastrado detectado e ignorado.",);
            return;
        }
    };

    // 2. Verifica reserva ativa
    let reservation = sqlx::query!(
        "SELECT id, spot_id FROM reservations WHERE user_id = $1 AND status = 'active' LIMIT 1",
        user_id
    )
    .fetch_optional(&state.pool)
    .await;

    // 3. Define a vaga e a rota final
    let (spot_id, final_route) = if let Ok(Some(res)) = reservation {
        // Fluxo com reserva prévia
        let route = state
            .graph
            .read()
            .await
            .calculate_route(&camera_id, &res.spot_id)
            .unwrap_or_else(|| vec![camera_id.clone(), res.spot_id.clone()]);

        (res.spot_id, route)
    } else {
        // Fluxo sem reserva: Usa o Grafo para varrer todas as vagas livres e achar a rota mais curta
        let free_spots = sqlx::query!("SELECT id FROM spots WHERE status = 'free'")
            .fetch_all(&state.pool)
            .await
            .unwrap_or_default();

        let candidates = {
            let graph = state.graph.read().await;
            let mut candidates: Vec<_> = free_spots
                .into_iter()
                .filter_map(|row| {
                    let cost = graph.calculate_cost(&camera_id, &row.id)?;
                    let route = graph.calculate_route(&camera_id, &row.id)?;
                    Some((cost, row.id, route))
                })
                .collect();
            candidates.sort_by_key(|(cost, _, _)| *cost);
            candidates
        };

        let mut claimed = None;
        for (_, spot, route) in candidates {
            let expires_at = Utc::now() + Duration::minutes(15);
            match reservation_store::create(&state.pool, &spot, user_id, expires_at).await {
                Ok(_) => {
                    // Dispara broadcast de SPOT_UPDATE para os outros clientes saberem que a vaga foi ocupada
                    let update_msg = ServerToAppMsg::SpotUpdate {
                        spot_id: spot.clone(),
                        status: "reserved".to_string(),
                    };
                    if let Ok(json_str) = serde_json::to_string(&update_msg) {
                        let _ = state.tx.send(json_str);
                    }

                    claimed = Some((spot, route));
                    break;
                }
                Err(CreateError::Unavailable) => continue,
                Err(CreateError::Database(error)) => {
                    tracing::error!(%error, spot_id = %spot, "Failed to create automatic reservation");
                    return;
                }
            }
        }
        let Some((spot, route)) = claimed else { return };
        (spot, route)
    };

    // 4. Envia o comando de navegação para a sessão do usuário
    if let Some(session_tx) = state.user_sessions.get(&user_id) {
        let nav_msg = ServerToAppMsg::NavigationStart {
            spot_id,
            route: final_route,
        };

        if let Ok(json_str) = serde_json::to_string(&nav_msg) {
            let _ = session_tx.send(json_str);
        }
    }
}
