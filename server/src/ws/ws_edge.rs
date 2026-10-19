// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use axum::{
    extract::{State, WebSocketUpgrade},
    response::IntoResponse,
};
use futures_util::StreamExt;
use uuid::Uuid;

use crate::reservation_store::{self, FinishReason};
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
    let mut durable_events = vec![
        "SPOT_UPDATE",
        "PATH_UPDATE",
        "TREND_PREDICTION",
        "TREND_PREDICTION_V2",
    ];
    if state.lpr_mode == crate::lpr::LprMode::Disabled {
        durable_events.push("CAR_DETECTED");
    }
    let ready = ServerToEdgeMsg::Ready {
        delivery_version: 1,
        durable_events,
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
                    let delivery = match crate::edge_delivery::apply_with_state(
                        state, principal, event_id, payload,
                    )
                    .await
                    {
                        Ok(delivery) => delivery,
                        Err(error) => {
                            tracing::error!(%error, cause = ?std::error::Error::source(&error), %event_id, "Failed to persist edge event");
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
                        match crate::edge_operations::car_detected(
                            state, principal, &plate, &camera_id,
                        )
                        .await
                        {
                            Ok(true) => {}
                            Ok(false) => {
                                let _ = socket
                                    .send(Message::Close(Some(CloseFrame {
                                        code: 1008,
                                        reason: "Gateway arrival not granted".into(),
                                    })))
                                    .await;
                                return false;
                            }
                            Err(error) => {
                                tracing::error!(%error, cause = ?std::error::Error::source(&error), gateway_id = %principal.gateway_id, "Edge arrival failed");
                                return false;
                            }
                        }
                    }
                    EdgeToServerMsg::SpotUpdate {
                        spot_id,
                        status,
                        camera_id,
                        confidence,
                        timestamp,
                    } => {
                        let observation = match crate::edge_delivery::apply_legacy(
                            &state.pool,
                            principal,
                            &camera_id,
                            &spot_id,
                            &status,
                            confidence,
                            timestamp,
                        )
                        .await
                        {
                            Ok(observation) => observation,
                            Err(error) => {
                                tracing::error!(%error, cause = ?std::error::Error::source(&error), gateway_id = %principal.gateway_id, "Legacy edge observation failed");
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
                        match crate::edge_operations::path_update(
                            state, principal, &from_node, &to_node, is_active,
                        )
                        .await
                        {
                            Ok(true) => {}
                            Ok(false) => {
                                let _ = socket
                                    .send(Message::Close(Some(CloseFrame {
                                        code: 1008,
                                        reason: "Gateway path operation not granted".into(),
                                    })))
                                    .await;
                                return false;
                            }
                            Err(error) => {
                                tracing::error!(%error, cause = ?std::error::Error::source(&error), gateway_id = %principal.gateway_id, "Edge path update failed");
                                return false;
                            }
                        }
                    }
                    prediction @ (EdgeToServerMsg::TrendPrediction { .. }
                    | EdgeToServerMsg::TrendPredictionV2(_)) => {
                        match crate::edge_operations::forecast(state, principal, prediction).await {
                            Ok(true) => {}
                            Ok(false) => {
                                let _ = socket
                                    .send(Message::Close(Some(CloseFrame {
                                        code: 1008,
                                        reason: "Gateway forecast operation not granted".into(),
                                    })))
                                    .await;
                                return false;
                            }
                            Err(error) => {
                                tracing::error!(%error, cause = ?std::error::Error::source(&error), gateway_id = %principal.gateway_id, "Edge forecast update failed");
                                return false;
                            }
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
