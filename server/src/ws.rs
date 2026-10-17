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
pub(crate) mod private_session;
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
use uuid::Uuid;

use crate::reservation_store::{self, CreateError, FinishReason};
use crate::security::{AuthError, AuthSession};
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
    let session = match AuthSession::from_claims(claims) {
        Ok(session) => session,
        Err(error) => return error.into_response(),
    };
    if let Err(error) = session.revalidate(&state.pool).await {
        return error.into_response();
    }
    ws.on_upgrade(move |socket| handle_app_socket(socket, state, session))
        .into_response()
}

async fn handle_app_socket(socket: WebSocket, state: SharedState, session: AuthSession) {
    let user_id = session.user_id;
    let (mut sender, mut receiver) = socket.split();

    let (private_tx, mut private_rx, mut private_closed) = private_session::channel();
    state.user_sessions.insert(user_id, private_tx.clone());

    let mut broadcast_rx = state.tx.subscribe();

    // Keep the snapshot and both IO loops under the same session supervisor.
    // Dropping these futures also cancels pending reads/writes; no detached tasks.
    let send_task = async {
        session.revalidate(&state.pool).await?;
        let db_spots = sqlx::query!("SELECT id, status FROM spots WHERE is_active")
            .fetch_all(&state.pool)
            .await
            .unwrap_or_default();
        for row in db_spots {
            session.check_expiration()?;
            let msg = ServerToAppMsg::SpotUpdate {
                spot_id: row.id,
                status: row.status,
            };
            if let Ok(json_str) = serde_json::to_string(&msg)
                && sender.send(Message::Text(json_str.into())).await.is_err()
            {
                return Ok::<(), AuthError>(());
            }
        }

        loop {
            tokio::select! {
                received = broadcast_rx.recv() => match received {
                    Ok(broadcast_msg) => {
                        session.check_expiration()?;
                        if sender.send(Message::Text(broadcast_msg.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
                Some(private_msg) = private_rx.recv() => {
                    session.check_expiration()?;
                    if sender.send(Message::Text(private_msg.into())).await.is_err() {
                        break;
                    }
                }
            }
        }
        Ok::<(), AuthError>(())
    };

    let value = state.clone();
    let recv_task = async {
        while let Some(Ok(Message::Text(text))) = receiver.next().await {
            if let Ok(msg) = serde_json::from_str::<AppToServerMsg>(&text) {
                session.revalidate(&value.pool).await?;
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
                                    let _ = private_tx.try_send(json);
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
                                    let _ = private_tx.try_send(json);
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
        Ok::<(), AuthError>(())
    };

    let invalidation = tokio::select! {
        biased;
        _ = tokio::time::sleep_until(session.deadline) => Some((axum::http::StatusCode::UNAUTHORIZED, "Expired session".to_owned())),
        error = session.wait_for_invalidation(&state.pool) => Some(error),
        _ = private_closed.wait_for(|closed| *closed) => None,
        result = send_task => result.err(),
        result = recv_task => result.err(),
    };

    // The most recent connection owns the private queue. An older connection's
    // cleanup must not remove a newer queue for this user.
    state
        .user_sessions
        .remove_if(&user_id, |_, current| current.same_channel(&private_tx));
    if let Some((status, _)) = invalidation {
        tracing::debug!(%user_id, %status, "Closing invalid app WebSocket session");
    }
    // Drop the transport without another send/flush: a canceled send may still
    // have protected data in its buffer, which must not be flushed after expiry.
}

pub async fn ws_dashboard_handler(
    ws: WebSocketUpgrade,
    Query(query): Query<DashboardQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<SharedState>,
) -> axum::response::Response {
    let claims = query
        .token
        .as_deref()
        .and_then(|token| crate::security::verify_jwt(token, &state.jwt_secret).ok())
        .or_else(|| crate::security::authenticated_claims(&headers, &state.jwt_secret).ok());
    let Some(claims) = claims.filter(|claims| claims.role == "admin") else {
        return (axum::http::StatusCode::UNAUTHORIZED, "Admin required").into_response();
    };
    let session = match AuthSession::from_claims(claims) {
        Ok(session) => session,
        Err(error) => return error.into_response(),
    };
    if let Err(error) = session.revalidate(&state.pool).await {
        return error.into_response();
    }
    ws.on_upgrade(move |socket| handle_dashboard_socket(socket, state, session))
        .into_response()
}

#[derive(Deserialize)]
pub struct DashboardQuery {
    pub token: Option<String>,
}

async fn handle_dashboard_socket(socket: WebSocket, state: SharedState, session: AuthSession) {
    let (mut sender, mut receiver) = socket.split();
    let mut broadcast_rx = state.tx.subscribe();

    let send_task = async {
        session.revalidate(&state.pool).await?;
        let db_spots = sqlx::query!("SELECT id, status FROM spots WHERE is_active")
            .fetch_all(&state.pool)
            .await
            .unwrap_or_default();
        for row in db_spots {
            session.check_expiration()?;
            let msg = ServerToAppMsg::SpotUpdate {
                spot_id: row.id,
                status: row.status,
            };
            if let Ok(json_str) = serde_json::to_string(&msg)
                && sender.send(Message::Text(json_str.into())).await.is_err()
            {
                return Ok::<(), AuthError>(());
            }
        }

        let cached_trend = state
            .last_trend_prediction
            .lock()
            .ok()
            .and_then(|c| c.clone());
        if let Some(json_str) = cached_trend {
            session.check_expiration()?;
            if sender.send(Message::Text(json_str.into())).await.is_err() {
                return Ok::<(), AuthError>(());
            }
        }

        loop {
            match broadcast_rx.recv().await {
                Ok(broadcast_msg) => {
                    session.check_expiration()?;
                    if sender
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
        Ok::<(), AuthError>(())
    };
    // Poll incoming frames too, so a quiet dashboard observes client closure.
    let recv_task = async {
        while let Some(Ok(message)) = receiver.next().await {
            if matches!(message, Message::Close(_)) {
                break;
            }
        }
    };
    let invalidation = tokio::select! {
        biased;
        _ = tokio::time::sleep_until(session.deadline) => true,
        _ = session.wait_for_invalidation(&state.pool) => true,
        result = send_task => result.is_err(),
        _ = recv_task => false,
    };
    if invalidation {
        tracing::debug!(user_id = %session.user_id, "Closing invalid dashboard WebSocket session");
    }
    // Never flush an interrupted data send while invalidating authentication.
}
