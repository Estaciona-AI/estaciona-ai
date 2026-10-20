// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Extension, Query, State},
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
    ws_origin::WsOriginPolicy,
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

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct IssueRequest {
    destination: Destination,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
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

#[derive(Serialize, schemars::JsonSchema)]
pub(crate) struct IssuedTicket {
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

        if !crate::sessions::lock_active(&mut tx, &session)
            .await
            .map_err(service_unavailable)?
        {
            return Err((StatusCode::UNAUTHORIZED, "Invalid or revoked session"));
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

        sqlx::query("INSERT INTO ws_tickets (digest,user_id,role,destination,session_expires_at,expires_at,session_id) VALUES ($1,$2,$3,$4,$5,$6,$7)")
            .bind(&digest)
            .bind(session.user_id)
            .bind(&session.role)
            .bind(request.destination.as_str())
            .bind(session.expires_at() as i64)
            .bind(expires_at)
            .bind(session.session_id)
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
    session_id: Uuid,
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
        let row: Option<ConsumedTicket> = sqlx::query_as("DELETE FROM ws_tickets WHERE digest=$1 RETURNING session_id,user_id,role,destination,session_expires_at,expires_at")
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
            jti: row.session_id,
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
            match timeout(
                Duration::from_secs(2),
                crate::sessions::cleanup_expired(&pool),
            )
            .await
            {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => tracing::warn!(%error, "Expired session cleanup failed"),
                Err(_) => tracing::warn!("Expired session cleanup timed out"),
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
    headers: HeaderMap,
    Extension(origin_policy): Extension<WsOriginPolicy>,
    Query(query): Query<TicketQuery>,
    State(state): State<SharedState>,
) -> axum::response::Response {
    if let Err(error) = origin_policy.authorize(&headers) {
        return error.into_response();
    }
    let session = match consume(&state.pool, &query.ticket, Destination::App).await {
        Ok(session) => session,
        Err((status, message)) => return (status, message).into_response(),
    };
    ws.on_upgrade(move |socket| crate::ws::handle_app_socket(socket, state, session))
        .into_response()
}

pub async fn dashboard_handler(
    ws: axum::extract::ws::WebSocketUpgrade,
    headers: HeaderMap,
    Extension(origin_policy): Extension<WsOriginPolicy>,
    Query(query): Query<TicketQuery>,
    State(state): State<SharedState>,
) -> axum::response::Response {
    if let Err(error) = origin_policy.authorize(&headers) {
        return error.into_response();
    }
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
        let jwt = crate::sessions::issue(&pool, user_id, "user", "fixture")
            .await
            .unwrap();
        let session_id = crate::security::verify_jwt(&jwt, "fixture").unwrap().jti;
        sqlx::query("INSERT INTO ws_tickets (session_id,digest,user_id,role,destination,session_expires_at,expires_at) SELECT $2,decode(lpad(to_hex(value),64,'0'),'hex'),$1,'user','app',0,NOW()-INTERVAL '1 second' FROM generate_series(1,1001) value")
            .bind(user_id)
            .bind(session_id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO ws_tickets (session_id,digest,user_id,role,destination,session_expires_at,expires_at) VALUES ($2,decode(lpad(to_hex(1002),64,'0'),'hex'),$1,'user','app',0,NOW()+INTERVAL '1 minute')")
            .bind(user_id)
            .bind(session_id)
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

#[cfg(test)]
mod origin_handshake_tests {
    use super::{app_handler, dashboard_handler, new_ticket};
    use crate::{
        site_config::{ParkingLotConfig, PathNode, SiteConfig},
        state::{self, SharedState},
        ws_origin::WsOriginPolicy,
    };
    use axum::{Extension, Router, routing::get};
    use chrono::{Duration as ChronoDuration, Utc};
    use sqlx::PgPool;
    use std::net::SocketAddr;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        task::JoinHandle,
    };
    use uuid::Uuid;

    async fn test_state(pool: PgPool) -> SharedState {
        state::init_state(
            crate::installation::test_support::provision(&pool).await,
            "r14-test-jwt".into(),
            "r14-test-pepper".into(),
            SiteConfig {
                directory: std::env::temp_dir(),
                parkings: vec![ParkingLotConfig {
                    path: vec![PathNode { x: 0.0, z: 0.0 }, PathNode { x: 1.0, z: 0.0 }],
                }],
                spots: Vec::new(),
            },
            Default::default(),
        )
        .await
        .unwrap()
    }

    async fn start_server(state: SharedState) -> (SocketAddr, JoinHandle<()>) {
        let app = Router::new()
            .route("/ws/app", get(app_handler))
            .route("/ws/dashboard", get(dashboard_handler))
            .with_state(state)
            .layer(Extension(
                WsOriginPolicy::parse("https://parking.example.edu").unwrap(),
            ));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (address, server)
    }

    async fn insert_ticket(pool: &PgPool, destination: &str, role: &str) -> String {
        let user_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO users (id,name,email,password_hash,role) VALUES ($1,'R14 test',$2,'unused',$3)",
        )
        .bind(user_id)
        .bind(format!("{user_id}@example.invalid"))
        .bind(role)
        .execute(pool)
        .await
        .unwrap();

        let jwt = crate::sessions::issue(pool, user_id, role, "fixture")
            .await
            .unwrap();
        let claims = crate::security::verify_jwt(&jwt, "fixture").unwrap();
        let (ticket, digest) = new_ticket();
        sqlx::query("INSERT INTO ws_tickets (digest,user_id,role,destination,session_expires_at,expires_at,session_id) VALUES ($1,$2,$3,$4,$5,$6,$7)")
            .bind(digest)
            .bind(user_id)
            .bind(role)
            .bind(destination)
            .bind(claims.exp as i64)
            .bind(Utc::now() + ChronoDuration::seconds(30))
            .bind(claims.jti)
            .execute(pool)
            .await
            .unwrap();
        ticket
    }

    async fn ticket_exists(pool: &PgPool, ticket: &str) -> bool {
        let digest = super::ticket_digest(ticket).unwrap();
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM ws_tickets WHERE digest=$1)")
            .bind(digest)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn websocket_handshake(
        address: SocketAddr,
        path: &str,
        ticket: &str,
        origins: &[&str],
    ) -> (u16, TcpStream) {
        let mut stream = TcpStream::connect(address).await.unwrap();
        let origin_headers = origins
            .iter()
            .map(|origin| format!("Origin: {origin}\r\n"))
            .collect::<String>();
        let request = format!(
            "GET /ws/{path}?ticket={ticket} HTTP/1.1\r\n\
             Host: localhost\r\n\
             Connection: Upgrade\r\n\
             Upgrade: websocket\r\n\
             Sec-WebSocket-Version: 13\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             {origin_headers}\r\n"
        );
        stream.write_all(request.as_bytes()).await.unwrap();

        let mut response = Vec::new();
        while !response.ends_with(b"\r\n\r\n") {
            let mut byte = [0_u8; 1];
            if stream.read(&mut byte).await.unwrap() == 0 {
                break;
            }
            response.push(byte[0]);
        }
        let response = String::from_utf8(response).unwrap();
        let status = response
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|value| value.parse().ok())
            .unwrap_or_default();
        (status, stream)
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn only_exact_origins_can_upgrade_app_and_dashboard_sockets(pool: PgPool) {
        let state = test_state(pool.clone()).await;
        let (address, server) = start_server(state).await;
        let cases = [("app", "app", "user"), ("dashboard", "dashboard", "admin")];
        let mut outcomes = Vec::new();

        for (path, destination, role) in cases {
            let ticket = insert_ticket(&pool, destination, role).await;
            let (denied_status, denied_stream) =
                websocket_handshake(address, path, &ticket, &["https://attacker.example"]).await;
            drop(denied_stream);
            outcomes.push((path, denied_status, ticket_exists(&pool, &ticket).await));

            let (allowed_status, allowed_stream) =
                websocket_handshake(address, path, &ticket, &["https://parking.example.edu"]).await;
            drop(allowed_stream);
            outcomes.push((path, allowed_status, ticket_exists(&pool, &ticket).await));
        }

        server.abort();
        assert_eq!(
            outcomes,
            vec![
                ("app", 403, true),
                ("app", 101, false),
                ("dashboard", 403, true),
                ("dashboard", 101, false),
            ],
            "unlisted origins must be rejected without consuming the one-time ticket"
        );
    }
}
