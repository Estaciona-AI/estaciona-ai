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

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
    routing::post,
};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use std::{fmt::Write as _, time::Duration};
use tokio::time::timeout;
use tower_http::limit::RequestBodyLimitLayer;
use uuid::Uuid;

use crate::{
    security::{self, AuthSession, Claims},
    state::SharedState,
};

const TICKET_TTL_SECONDS: i64 = 30;
const MAX_PENDING_PER_USER: i64 = 5;
const OPERATION_TIMEOUT: Duration = Duration::from_secs(5);
const SQL_TIMEOUT: &str = "2s";
const CLEANUP_INTERVAL: Duration = Duration::from_secs(10);
const CLEANUP_BATCH: i64 = 1000;

type ApiError = (StatusCode, &'static str);

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/auth/ws-ticket", post(issue))
        .layer(DefaultBodyLimit::max(1024))
        .layer(RequestBodyLimitLayer::new(1024))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IssueRequest {
    destination: Destination,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Destination {
    App,
    Dashboard,
}

impl Destination {
    fn as_str(self) -> &'static str {
        match self {
            Self::App => "app",
            Self::Dashboard => "dashboard",
        }
    }
}

#[derive(Serialize)]
struct IssuedTicket {
    ticket: String,
    expires_at: DateTime<Utc>,
}

fn bearer(headers: &HeaderMap) -> Result<&str, ApiError> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|value| !value.is_empty())
        .ok_or((StatusCode::UNAUTHORIZED, "Bearer token required"))
}

fn service_unavailable(error: impl std::fmt::Display) -> ApiError {
    tracing::error!(%error, "WebSocket ticket operation unavailable");
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "WebSocket ticket service unavailable",
    )
}

fn new_ticket() -> (String, Vec<u8>) {
    let mut secret = [0_u8; 32];
    OsRng.fill_bytes(&mut secret);
    let mut ticket = String::with_capacity(secret.len() * 2);
    for byte in secret {
        write!(&mut ticket, "{byte:02x}").expect("writing to a String cannot fail");
    }
    let digest = Sha256::digest(ticket.as_bytes()).to_vec();
    (ticket, digest)
}

async fn issue(
    State(state): State<SharedState>,
    headers: HeaderMap,
    request: Result<Json<IssueRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<(StatusCode, HeaderMap, Json<IssuedTicket>), ApiError> {
    let Json(request) = request.map_err(|rejection| {
        let status = if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
            StatusCode::PAYLOAD_TOO_LARGE
        } else {
            StatusCode::BAD_REQUEST
        };
        (status, "Invalid request")
    })?;
    let raw_jwt = bearer(&headers)?;
    let claims = security::verify_jwt(raw_jwt, &state.jwt_secret)
        .map_err(|_| (StatusCode::UNAUTHORIZED, "Invalid or expired session"))?;
    let session = AuthSession::from_claims(claims)
        .map_err(|_| (StatusCode::UNAUTHORIZED, "Invalid or expired session"))?;
    let (ticket, digest) = new_ticket();
    let operation = async {
        session
            .revalidate(&state.pool)
            .await
            .map_err(|(status, _)| (status, "Session authorization failed"))?;
        if request.destination == Destination::Dashboard && session.role != "admin" {
            return Err((StatusCode::FORBIDDEN, "Admin required"));
        }

        let expiry = Utc::now()
            .checked_add_signed(ChronoDuration::seconds(TICKET_TTL_SECONDS))
            .ok_or((StatusCode::SERVICE_UNAVAILABLE, "Ticket expiry unavailable"))?;
        let expires_at = expiry.min(
            DateTime::from_timestamp(session.expires_at() as i64, 0)
                .ok_or((StatusCode::UNAUTHORIZED, "Invalid or expired session"))?,
        );
        let mut tx = state.pool.begin().await.map_err(service_unavailable)?;
        set_transaction_limits(&mut tx)
            .await
            .map_err(service_unavailable)?;

        let current_role: Option<String> =
            sqlx::query_scalar("SELECT role FROM users WHERE id=$1 FOR UPDATE")
                .bind(session.user_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(service_unavailable)?;
        session
            .check_expiration()
            .map_err(|_| (StatusCode::UNAUTHORIZED, "Invalid or expired session"))?;
        if current_role.as_deref() != Some(session.role.as_str()) {
            return Err((StatusCode::FORBIDDEN, "Session permissions changed"));
        }
        if request.destination == Destination::Dashboard && current_role.as_deref() != Some("admin")
        {
            return Err((StatusCode::FORBIDDEN, "Admin required"));
        }

        sqlx::query("DELETE FROM ws_tickets WHERE user_id=$1 AND expires_at <= NOW()")
            .bind(session.user_id)
            .execute(&mut *tx)
            .await
            .map_err(service_unavailable)?;
        let pending: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM ws_tickets WHERE user_id=$1 AND expires_at > NOW()",
        )
        .bind(session.user_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(service_unavailable)?;
        if pending >= MAX_PENDING_PER_USER {
            return Err((StatusCode::TOO_MANY_REQUESTS, "Too many pending tickets"));
        }

        sqlx::query("INSERT INTO ws_tickets (digest,user_id,role,destination,session_expires_at,expires_at) VALUES ($1,$2,$3,$4,$5,$6)")
            .bind(&digest)
            .bind(session.user_id)
            .bind(&session.role)
            .bind(request.destination.as_str())
            .bind(session.expires_at() as i64)
            .bind(expires_at)
            .execute(&mut *tx)
            .await
            .map_err(service_unavailable)?;
        session
            .check_expiration()
            .map_err(|_| (StatusCode::UNAUTHORIZED, "Invalid or expired session"))?;
        tx.commit().await.map_err(service_unavailable)?;
        Ok::<_, ApiError>(expires_at)
    };
    let expires_at = timeout(OPERATION_TIMEOUT, operation).await.map_err(|_| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "WebSocket ticket operation timed out",
        )
    })??;

    let mut response_headers = HeaderMap::new();
    response_headers.insert(
        header::CACHE_CONTROL,
        "no-store".parse().expect("static header"),
    );
    response_headers.insert(header::PRAGMA, "no-cache".parse().expect("static header"));
    Ok((
        StatusCode::CREATED,
        response_headers,
        Json(IssuedTicket { ticket, expires_at }),
    ))
}

async fn set_transaction_limits(tx: &mut Transaction<'_, Postgres>) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT set_config('statement_timeout', $1, true)")
        .bind(SQL_TIMEOUT)
        .execute(&mut **tx)
        .await?;
    sqlx::query("SELECT set_config('lock_timeout', $1, true)")
        .bind(SQL_TIMEOUT)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

#[derive(FromRow)]
struct ConsumedTicket {
    user_id: Uuid,
    role: String,
    destination: String,
    session_expires_at: i64,
    expires_at: DateTime<Utc>,
}

fn ticket_digest(ticket: &str) -> Option<Vec<u8>> {
    if ticket.len() != 64 || !ticket.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(Sha256::digest(ticket.as_bytes()).to_vec())
}

async fn consume(
    pool: &PgPool,
    ticket: &str,
    destination: Destination,
) -> Result<AuthSession, ApiError> {
    let digest = ticket_digest(ticket).ok_or((StatusCode::UNAUTHORIZED, "Invalid ticket"))?;
    let operation = async {
        let mut tx = pool.begin().await.map_err(service_unavailable)?;
        set_transaction_limits(&mut tx)
            .await
            .map_err(service_unavailable)?;
        let row: Option<ConsumedTicket> = sqlx::query_as("DELETE FROM ws_tickets WHERE digest=$1 RETURNING user_id,role,destination,session_expires_at,expires_at")
            .bind(digest)
            .fetch_optional(&mut *tx)
            .await
            .map_err(service_unavailable)?;
        tx.commit().await.map_err(service_unavailable)?;
        let row = row.ok_or((StatusCode::UNAUTHORIZED, "Invalid ticket"))?;
        if row.expires_at <= Utc::now()
            || row.session_expires_at <= Utc::now().timestamp()
            || row.destination != destination.as_str()
        {
            return Err((StatusCode::UNAUTHORIZED, "Invalid ticket"));
        }
        let session = AuthSession::from_claims(Claims {
            sub: row.user_id.to_string(),
            role: row.role,
            exp: row.session_expires_at as usize,
        })
        .map_err(|_| (StatusCode::UNAUTHORIZED, "Invalid ticket"))?;
        session
            .revalidate(pool)
            .await
            .map_err(|(status, _)| (status, "Session authorization failed"))?;
        if destination == Destination::Dashboard && session.role != "admin" {
            return Err((StatusCode::FORBIDDEN, "Admin required"));
        }
        Ok::<_, ApiError>(session)
    };
    timeout(OPERATION_TIMEOUT, operation).await.map_err(|_| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "WebSocket ticket operation timed out",
        )
    })?
}

async fn cleanup_expired_batch(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    set_transaction_limits(&mut tx).await?;
    let result = sqlx::query("DELETE FROM ws_tickets WHERE digest IN (SELECT digest FROM ws_tickets WHERE expires_at <= NOW() ORDER BY expires_at LIMIT $1 FOR UPDATE SKIP LOCKED)")
        .bind(CLEANUP_BATCH)
        .execute(&mut *tx)
        .await?;
    let count = result.rows_affected();
    tx.commit().await?;
    Ok(count)
}

pub fn spawn_cleanup(pool: PgPool) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(CLEANUP_INTERVAL).await;
            match timeout(Duration::from_secs(2), cleanup_expired_batch(&pool)).await {
                Ok(Ok(count)) if count > 0 => {
                    tracing::debug!(count, "Expired WebSocket tickets removed");
                }
                Ok(Ok(_)) => {}
                Ok(Err(error)) => tracing::warn!(%error, "WebSocket ticket cleanup failed"),
                Err(_) => tracing::warn!("WebSocket ticket cleanup timed out"),
            }
        }
    });
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TicketQuery {
    ticket: String,
}

pub async fn app_handler(
    ws: axum::extract::ws::WebSocketUpgrade,
    Query(query): Query<TicketQuery>,
    State(state): State<SharedState>,
) -> axum::response::Response {
    let session = match consume(&state.pool, &query.ticket, Destination::App).await {
        Ok(session) => session,
        Err((status, message)) => return (status, message).into_response(),
    };
    ws.on_upgrade(move |socket| crate::ws::handle_app_socket(socket, state, session))
        .into_response()
}

pub async fn dashboard_handler(
    ws: axum::extract::ws::WebSocketUpgrade,
    Query(query): Query<TicketQuery>,
    State(state): State<SharedState>,
) -> axum::response::Response {
    let session = match consume(&state.pool, &query.ticket, Destination::Dashboard).await {
        Ok(session) => session,
        Err((status, message)) => return (status, message).into_response(),
    };
    ws.on_upgrade(move |socket| crate::ws::handle_dashboard_socket(socket, state, session))
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticket_is_opaque_hex_and_only_digest_is_stored() {
        let (ticket, digest) = new_ticket();
        assert_eq!(ticket.len(), 64);
        assert!(ticket.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(ticket_digest(&ticket).unwrap(), digest);
        assert!(ticket_digest("not-a-ticket").is_none());
    }

    #[test]
    fn destinations_are_exact_protocol_values() {
        assert_eq!(
            serde_json::from_str::<Destination>("\"app\"").unwrap(),
            Destination::App
        );
        assert_eq!(
            serde_json::from_str::<Destination>("\"dashboard\"").unwrap(),
            Destination::Dashboard
        );
        assert!(serde_json::from_str::<Destination>("\"edge\"").is_err());
    }
}

#[cfg(test)]
mod postgres_tests {
    use super::cleanup_expired_batch;
    use sqlx::PgPool;
    use uuid::Uuid;

    #[sqlx::test(migrations = "./migrations")]
    async fn cleanup_removes_expired_tickets_in_bounded_batches(pool: PgPool) {
        let user_id = Uuid::new_v4();
        sqlx::query("INSERT INTO users (id,name,email,password_hash) VALUES ($1,'Ticket cleanup',$2,'unused')")
            .bind(user_id)
            .bind(format!("{user_id}@example.invalid"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO ws_tickets (digest,user_id,role,destination,session_expires_at,expires_at) SELECT decode(lpad(to_hex(value),64,'0'),'hex'),$1,'user','app',0,NOW()-INTERVAL '1 second' FROM generate_series(1,1001) value")
            .bind(user_id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO ws_tickets (digest,user_id,role,destination,session_expires_at,expires_at) VALUES (decode(lpad(to_hex(1002),64,'0'),'hex'),$1,'user','app',0,NOW()+INTERVAL '1 minute')")
            .bind(user_id)
            .execute(&pool)
            .await
            .unwrap();

        assert_eq!(cleanup_expired_batch(&pool).await.unwrap(), 1000);
        let remaining: (i64, i64) = sqlx::query_as("SELECT COUNT(*) FILTER (WHERE expires_at <= NOW()), COUNT(*) FILTER (WHERE expires_at > NOW()) FROM ws_tickets WHERE user_id=$1")
            .bind(user_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(remaining, (1, 1));
    }
}
