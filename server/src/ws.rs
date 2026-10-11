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

pub mod messages;
pub mod ws_edge;

pub use ws_edge::ws_edge_handler;

use axum::{
    extract::{
        Query, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    response::IntoResponse,
};
use chrono::{Duration, Utc};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::reservation_store::{self, CreateError, FinishReason};
use crate::state::SharedState;
use crate::ws::messages::{AppToServerMsg, ServerToAppMsg};

#[derive(Deserialize)]
pub struct WsAppQuery {
    pub user_id: Uuid,
    pub token: String,
}

pub async fn ws_app_handler(
    ws: WebSocketUpgrade,
    Query(query): Query<WsAppQuery>,
    State(state): State<SharedState>,
) -> axum::response::Response {
    let claims = match crate::security::verify_jwt(&query.token, &state.jwt_secret) {
        Ok(c) => c,
        Err(_) => {
            return (
                axum::http::StatusCode::UNAUTHORIZED,
                "Invalid or expired JWT token",
            )
                .into_response();
        }
    };
    if claims.sub != query.user_id.to_string() {
        return (
            axum::http::StatusCode::FORBIDDEN,
            "Token does not match user_id",
        )
            .into_response();
    }
    ws.on_upgrade(move |socket| handle_app_socket(socket, state, query.user_id))
        .into_response()
}

async fn handle_app_socket(socket: WebSocket, state: SharedState, user_id: Uuid) {
    let (mut sender, mut receiver) = socket.split();

    let (private_tx, mut private_rx) = mpsc::unbounded_channel::<String>();
    state.user_sessions.insert(user_id, private_tx.clone());

    let mut broadcast_rx = state.tx.subscribe();

    let db_spots = sqlx::query!("SELECT id, status FROM spots")
        .fetch_all(&state.pool)
        .await
        .unwrap_or_default();
    for row in db_spots {
        let msg = ServerToAppMsg::SpotUpdate {
            spot_id: row.id,
            status: row.status,
        };
        if let Ok(json_str) = serde_json::to_string(&msg) {
            let _ = sender.send(Message::Text(json_str.into())).await;
        }
    }

    let mut send_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                received = broadcast_rx.recv() => match received {
                    Ok(broadcast_msg) => {
                        if sender.send(Message::Text(broadcast_msg.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
                Some(private_msg) = private_rx.recv() => {
                    if sender.send(Message::Text(private_msg.into())).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    let value = state.clone();
    let mut recv_task = tokio::spawn(async move {
        while let Some(Ok(Message::Text(text))) = receiver.next().await {
            if let Ok(msg) = serde_json::from_str::<AppToServerMsg>(&text) {
                match msg {
                    AppToServerMsg::ReserveSpot { spot_id, .. } => {
                        let expires = Utc::now() + Duration::minutes(15);
                        match reservation_store::create(&value.pool, &spot_id, user_id, expires)
                            .await
                        {
                            Ok(reservation) => {
                                let confirm = ServerToAppMsg::ReservationConfirmed {
                                    reservation_id: reservation.id,
                                    spot_id: spot_id.clone(),
                                    expires_at: expires,
                                };

                                let update = ServerToAppMsg::SpotUpdate {
                                    spot_id: spot_id.clone(),
                                    status: "reserved".to_string(),
                                };

                                if let Ok(json) = serde_json::to_string(&confirm) {
                                    let _ = private_tx.send(json);
                                }
                                if let Ok(json) = serde_json::to_string(&update) {
                                    let _ = value.tx.send(json);
                                }
                            }
                            Err(CreateError::Unavailable) => {
                                let reject = ServerToAppMsg::ReservationRejected {
                                    spot_id,
                                    reason: "already_taken".to_string(),
                                };
                                if let Ok(json) = serde_json::to_string(&reject) {
                                    let _ = private_tx.send(json);
                                }
                            }
                            Err(CreateError::Database(error)) => {
                                tracing::error!(%error, %spot_id, "Failed to create WebSocket reservation");
                            }
                        }
                    }
                    AppToServerMsg::CancelReservation { reservation_id } => {
                        match reservation_store::finish(
                            &value.pool,
                            reservation_id,
                            FinishReason::Cancel { user_id },
                        )
                        .await
                        {
                            Ok(Some(transition)) => {
                                if let Some(status) = transition.spot_status {
                                    let update = ServerToAppMsg::SpotUpdate {
                                        spot_id: transition.spot_id,
                                        status: status.to_string(),
                                    };
                                    if let Ok(json) = serde_json::to_string(&update) {
                                        let _ = value.tx.send(json);
                                    }
                                }
                            }
                            Ok(None) => {}
                            Err(error) => {
                                tracing::error!(%error, %reservation_id, "Failed to cancel WebSocket reservation");
                            }
                        }
                    }
                }
            }
        }
    });

    tokio::select! {
        _ = (&mut send_task) => recv_task.abort(),
        _ = (&mut recv_task) => send_task.abort(),
    };

    state.user_sessions.remove(&user_id);
}

pub async fn ws_dashboard_handler(
    ws: WebSocketUpgrade,
    Query(query): Query<DashboardQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<SharedState>,
) -> axum::response::Response {
    let authorized = query
        .token
        .as_deref()
        .and_then(|token| crate::security::verify_jwt(token, &state.jwt_secret).ok())
        .or_else(|| crate::security::authenticated_claims(&headers, &state.jwt_secret).ok())
        .is_some_and(|claims| claims.role == "admin");
    if !authorized {
        return (axum::http::StatusCode::UNAUTHORIZED, "Admin required").into_response();
    }
    ws.on_upgrade(move |socket| handle_dashboard_socket(socket, state))
        .into_response()
}

#[derive(Deserialize)]
pub struct DashboardQuery {
    pub token: Option<String>,
}

async fn handle_dashboard_socket(mut socket: WebSocket, state: SharedState) {
    let mut broadcast_rx = state.tx.subscribe();

    let db_spots = sqlx::query!("SELECT id, status FROM spots")
        .fetch_all(&state.pool)
        .await
        .unwrap_or_default();
    for row in db_spots {
        let msg = ServerToAppMsg::SpotUpdate {
            spot_id: row.id,
            status: row.status,
        };
        if let Ok(json_str) = serde_json::to_string(&msg) {
            let _ = socket.send(Message::Text(json_str.into())).await;
        }
    }

    let cached_trend = state
        .last_trend_prediction
        .lock()
        .ok()
        .and_then(|c| c.clone());
    if let Some(json_str) = cached_trend {
        let _ = socket.send(Message::Text(json_str.into())).await;
    }

    loop {
        match broadcast_rx.recv().await {
            Ok(broadcast_msg) => {
                if socket
                    .send(Message::Text(broadcast_msg.into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
}
