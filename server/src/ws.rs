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
            .map_err(|error| {
                tracing::error!(%error, "WebSocket snapshot query failed");
                (
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    "Snapshot unavailable".to_owned(),
                )
            })?;
        for row in db_spots {
            session.check_expiration()?;
            let msg = ServerToAppMsg::SpotUpdate {
                spot_id: row.id,
                status: row.status,
            };
            if let Ok(json_str) = serde_json::to_string(&msg)
                && write_message(&mut sender, Message::Text(json_str.into()))
                    .await
                    .is_err()
            {
                return Ok::<(), AuthError>(());
            }
        }

        loop {
            tokio::select! {
                received = broadcast_rx.recv() => {
                    session.check_expiration()?;
                    if write_broadcast(&mut sender, received).await.is_err() {
                        break;
                    }
                },
                Some(private_msg) = private_rx.recv() => {
                    session.check_expiration()?;
                    if write_message(&mut sender, Message::Text(private_msg.into())).await.is_err() {
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
            .map_err(|error| {
                tracing::error!(%error, "WebSocket snapshot query failed");
                (
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    "Snapshot unavailable".to_owned(),
                )
            })?;
        for row in db_spots {
            session.check_expiration()?;
            let msg = ServerToAppMsg::SpotUpdate {
                spot_id: row.id,
                status: row.status,
            };
            if let Ok(json_str) = serde_json::to_string(&msg)
                && write_message(&mut sender, Message::Text(json_str.into()))
                    .await
                    .is_err()
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
            if write_message(&mut sender, Message::Text(json_str.into()))
                .await
                .is_err()
            {
                return Ok::<(), AuthError>(());
            }
        }

        loop {
            let received = broadcast_rx.recv().await;
            session.check_expiration()?;
            if write_broadcast(&mut sender, received).await.is_err() {
                break;
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

#[derive(Debug, PartialEq, Eq)]
enum WriteError {
    Timeout,
    Transport,
    Lagged,
    Closed,
}

async fn write_message<S>(sink: &mut S, message: Message) -> Result<(), WriteError>
where
    S: futures_util::Sink<Message> + Unpin,
{
    match tokio::time::timeout(std::time::Duration::from_secs(5), sink.send(message)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err(WriteError::Transport),
        Err(_) => {
            tracing::warn!("Closing WebSocket after write timeout");
            Err(WriteError::Timeout)
        }
    }
}

async fn write_broadcast<S>(
    sink: &mut S,
    received: Result<String, tokio::sync::broadcast::error::RecvError>,
) -> Result<(), WriteError>
where
    S: futures_util::Sink<Message> + Unpin,
{
    match received {
        Ok(message) => write_message(sink, Message::Text(message.into())).await,
        Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
            tracing::warn!(
                missed,
                "Closing lagged WebSocket; reconnect for a fresh snapshot"
            );
            Err(WriteError::Lagged)
        }
        Err(tokio::sync::broadcast::error::RecvError::Closed) => Err(WriteError::Closed),
    }
}

#[cfg(test)]
mod delivery_tests {
    use super::*;
    use futures_util::Sink;
    use std::{
        pin::Pin,
        task::{Context, Poll},
        time::{Duration, Instant},
    };

    struct TestSink {
        blocked_before_send: bool,
        blocked_on_flush: bool,
        messages: Vec<Message>,
    }

    impl Sink<Message> for TestSink {
        type Error = std::io::Error;

        fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            if self.blocked_before_send {
                Poll::Pending
            } else {
                Poll::Ready(Ok(()))
            }
        }
        fn start_send(mut self: Pin<&mut Self>, message: Message) -> Result<(), Self::Error> {
            self.messages.push(message);
            Ok(())
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            if self.blocked_on_flush {
                Poll::Pending
            } else {
                Poll::Ready(Ok(()))
            }
        }
        fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn r09_lagged_broadcast_requires_reconnection_instead_of_continuing() {
        let (updates, mut receiver) = tokio::sync::broadcast::channel(2);
        for message in ["old", "middle", "latest"] {
            updates.send(message.to_owned()).unwrap();
        }
        let mut sink = TestSink {
            blocked_before_send: false,
            blocked_on_flush: false,
            messages: vec![],
        };
        assert_eq!(
            write_broadcast(&mut sink, receiver.recv().await).await,
            Err(WriteError::Lagged)
        );
        assert!(
            sink.messages.is_empty(),
            "a partial stream is not a valid snapshot"
        );
    }

    #[tokio::test]
    async fn r09_closed_broadcast_ends_delivery_without_spinning() {
        let (updates, mut receiver) = tokio::sync::broadcast::channel::<String>(2);
        drop(updates);
        let mut sink = TestSink {
            blocked_before_send: false,
            blocked_on_flush: false,
            messages: vec![],
        };
        assert_eq!(
            write_broadcast(&mut sink, receiver.recv().await).await,
            Err(WriteError::Closed)
        );
        assert!(sink.messages.is_empty());
    }

    async fn assert_write_times_out(blocked_before_send: bool) {
        let mut sink = TestSink {
            blocked_before_send,
            blocked_on_flush: !blocked_before_send,
            messages: vec![],
        };
        let start = Instant::now();
        let result = tokio::time::timeout(
            Duration::from_secs(7),
            write_message(&mut sink, Message::Text("pending".into())),
        )
        .await;
        assert_eq!(result.unwrap(), Err(WriteError::Timeout));
        assert!(start.elapsed() >= Duration::from_millis(4900));
    }

    #[tokio::test]
    async fn r09_write_timeout_covers_readiness() {
        assert_write_times_out(true).await;
    }

    #[tokio::test]
    async fn r09_write_timeout_covers_flush() {
        assert_write_times_out(false).await;
    }

    #[tokio::test]
    async fn r09_transport_write_can_be_canceled_before_its_timeout() {
        let mut sink = TestSink {
            blocked_before_send: true,
            blocked_on_flush: false,
            messages: vec![],
        };
        // The authentication supervisor must remain able to cancel a blocked send.
        let result = tokio::time::timeout(
            Duration::from_millis(20),
            write_message(&mut sink, Message::Text("protected".into())),
        )
        .await;
        assert!(result.is_err());
        assert!(sink.messages.is_empty());
    }

    #[tokio::test]
    async fn r09_healthy_transport_delivers_normally() {
        let mut sink = TestSink {
            blocked_before_send: false,
            blocked_on_flush: false,
            messages: vec![],
        };
        write_message(&mut sink, Message::Text("delivered".into()))
            .await
            .unwrap();
        assert_eq!(sink.messages, [Message::Text("delivered".into())]);
    }
}
