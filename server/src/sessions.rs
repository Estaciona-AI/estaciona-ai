//! Local access-session registry. Identity-provider integration remains separate.
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
};
use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, Transaction};
use tokio::time::{Duration, timeout};
use uuid::Uuid;

use crate::{
    security::{self, AuthSession},
    state::SharedState,
};

type ApiError = (StatusCode, &'static str);
const OPERATION_TIMEOUT: Duration = Duration::from_secs(5);

fn unavailable(error: impl std::fmt::Display) -> ApiError {
    tracing::error!(%error, "Session storage unavailable");
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "Session service unavailable",
    )
}

pub(crate) fn expires_at(session: &AuthSession) -> Result<DateTime<Utc>, security::AuthError> {
    i64::try_from(session.expires_at())
        .ok()
        .and_then(|seconds| DateTime::from_timestamp(seconds, 0))
        .ok_or((StatusCode::UNAUTHORIZED, "Invalid session expiry".into()))
}

pub(crate) async fn issue(
    pool: &PgPool,
    user: Uuid,
    role: &str,
    secret: &str,
) -> Result<String, sqlx::Error> {
    let token = security::create_jwt(&user.to_string(), role, secret)
        .map_err(|_| sqlx::Error::Protocol("Session signing failed".into()))?;
    let claims = security::verify_jwt(&token, secret)
        .map_err(|_| sqlx::Error::Protocol("Session signing failed".into()))?;
    let session = AuthSession::from_claims(claims)
        .map_err(|_| sqlx::Error::Protocol("Session expiry invalid".into()))?;
    sqlx::query("INSERT INTO auth_sessions (id,user_id,role,expires_at) VALUES ($1,$2,$3,$4)")
        .bind(session.session_id)
        .bind(user)
        .bind(role)
        .bind(
            expires_at(&session)
                .map_err(|_| sqlx::Error::Protocol("Session expiry invalid".into()))?,
        )
        .execute(pool)
        .await?;
    Ok(token)
}

/// Serializes ticket issuance with logout; callers hold this until ticket commit.
pub(crate) async fn lock_active(
    tx: &mut Transaction<'_, Postgres>,
    session: &AuthSession,
) -> Result<bool, sqlx::Error> {
    let expiry =
        expires_at(session).map_err(|_| sqlx::Error::Protocol("Session expiry invalid".into()))?;
    let active: Option<Uuid> = sqlx::query_scalar("SELECT id FROM auth_sessions WHERE id=$1 AND user_id=$2 AND role=$3 AND expires_at=$4 AND expires_at>NOW() AND revoked_at IS NULL FOR UPDATE")
        .bind(session.session_id).bind(session.user_id).bind(&session.role).bind(expiry)
        .fetch_optional(&mut **tx).await?;
    Ok(active.is_some())
}

pub(crate) async fn logout(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    let raw = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|value| !value.is_empty())
        .ok_or((StatusCode::UNAUTHORIZED, "Bearer token required"))?;
    let claims = security::verify_jwt(raw, &state.jwt_secret)
        .map_err(|_| (StatusCode::UNAUTHORIZED, "Invalid or expired session"))?;
    let session = AuthSession::from_claims(claims)
        .map_err(|_| (StatusCode::UNAUTHORIZED, "Invalid or expired session"))?;
    let expiry =
        expires_at(&session).map_err(|_| (StatusCode::UNAUTHORIZED, "Invalid session expiry"))?;
    let operation = async {
        let mut tx = state.pool.begin().await.map_err(unavailable)?;
        sqlx::raw_sql("SET LOCAL lock_timeout='2s'; SET LOCAL statement_timeout='2s'")
            .execute(&mut *tx)
            .await
            .map_err(unavailable)?;
        let id: Option<Uuid> = sqlx::query_scalar("UPDATE auth_sessions SET revoked_at=COALESCE(revoked_at,NOW()) WHERE id=$1 AND user_id=$2 AND role=$3 AND expires_at=$4 RETURNING id")
            .bind(session.session_id).bind(session.user_id).bind(&session.role).bind(expiry)
            .fetch_optional(&mut *tx).await.map_err(unavailable)?;
        if id.is_none() {
            return Err((StatusCode::UNAUTHORIZED, "Invalid session"));
        }
        sqlx::query("DELETE FROM ws_tickets WHERE session_id=$1")
            .bind(session.session_id)
            .execute(&mut *tx)
            .await
            .map_err(unavailable)?;
        tx.commit().await.map_err(unavailable)?;
        Ok::<_, ApiError>(())
    };
    timeout(OPERATION_TIMEOUT, operation).await.map_err(|_| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "Session operation timed out",
        )
    })??;
    Ok((
        StatusCode::NO_CONTENT,
        [
            (
                header::SET_COOKIE,
                "estaciona_token=; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=0",
            ),
            (header::CACHE_CONTROL, "no-store"),
        ],
    ))
}

pub(crate) async fn cleanup_expired(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL statement_timeout='2s'")
        .execute(&mut *tx)
        .await?;
    let count = sqlx::query("DELETE FROM auth_sessions WHERE id IN (SELECT id FROM auth_sessions WHERE expires_at<=NOW() ORDER BY expires_at LIMIT 1000 FOR UPDATE SKIP LOCKED)")
        .execute(&mut *tx).await?.rows_affected();
    tx.commit().await?;
    Ok(count)
}
