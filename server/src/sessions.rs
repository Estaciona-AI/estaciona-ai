// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

//! Local access-session registry. Sessions start after a sign-in at the identity
//! provider and end at logout, expiry or the provider's back-channel logout.
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

#[derive(serde::Serialize, schemars::JsonSchema)]
pub(crate) struct LogoutResponse {
    /// Visit to end the identity provider session as well.
    end_session_url: Option<String>,
}
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

#[cfg(test)]
pub(crate) async fn issue(
    pool: &PgPool,
    user: Uuid,
    role: &str,
    secret: &str,
) -> Result<String, sqlx::Error> {
    issue_for_provider_session(pool, user, role, secret, None).await
}

/// The provider session (`sid`) lets back-channel logout find this session.
pub(crate) async fn issue_for_provider_session(
    pool: &PgPool,
    user: Uuid,
    role: &str,
    secret: &str,
    provider_session: Option<&str>,
) -> Result<String, sqlx::Error> {
    let token = security::create_jwt(&user.to_string(), role, secret)
        .map_err(|_| sqlx::Error::Protocol("Session signing failed".into()))?;
    let claims = security::verify_jwt(&token, secret)
        .map_err(|_| sqlx::Error::Protocol("Session signing failed".into()))?;
    let session = AuthSession::from_claims(claims)
        .map_err(|_| sqlx::Error::Protocol("Session expiry invalid".into()))?;
    sqlx::query(
        "INSERT INTO auth_sessions (id,user_id,role,expires_at,idp_session_id) VALUES ($1,$2,$3,$4,$5)",
    )
    .bind(session.session_id)
    .bind(user)
    .bind(role)
    .bind(expires_at(&session).map_err(|_| sqlx::Error::Protocol("Session expiry invalid".into()))?)
    .bind(provider_session)
    .execute(pool)
    .await?;
    Ok(token)
}

async fn revocation(pool: &PgPool) -> Result<Transaction<'_, Postgres>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::raw_sql("SET LOCAL lock_timeout='2s'; SET LOCAL statement_timeout='2s'")
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

/// Their WebSocket tickets go in the same transaction; open sockets close at
/// their next revalidation.
async fn finish_revocation(
    mut tx: Transaction<'_, Postgres>,
    sessions: Vec<Uuid>,
) -> Result<u64, sqlx::Error> {
    sqlx::query("DELETE FROM ws_tickets WHERE session_id = ANY($1)")
        .bind(&sessions)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(sessions.len() as u64)
}

pub(crate) async fn revoke_provider_session(pool: &PgPool, sid: &str) -> Result<u64, sqlx::Error> {
    let mut tx = revocation(pool).await?;
    let sessions = sqlx::query_scalar(
        "UPDATE auth_sessions SET revoked_at=NOW() WHERE revoked_at IS NULL AND idp_session_id=$1 RETURNING id",
    )
    .bind(sid)
    .fetch_all(&mut *tx)
    .await?;
    finish_revocation(tx, sessions).await
}

pub(crate) async fn revoke_account(
    pool: &PgPool,
    issuer: &str,
    subject: &str,
) -> Result<u64, sqlx::Error> {
    let mut tx = revocation(pool).await?;
    let sessions = sqlx::query_scalar(
        "UPDATE auth_sessions SET revoked_at=NOW() WHERE revoked_at IS NULL AND user_id IN \
         (SELECT id FROM users WHERE oidc_issuer=$1 AND oidc_subject=$2) RETURNING id",
    )
    .bind(issuer)
    .bind(subject)
    .fetch_all(&mut *tx)
    .await?;
    finish_revocation(tx, sessions).await
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
    let raw =
        security::session_token(&headers).ok_or((StatusCode::UNAUTHORIZED, "Session required"))?;
    let claims = security::verify_jwt(raw, &state.jwt_secret)
        .map_err(|_| (StatusCode::UNAUTHORIZED, "Invalid or expired session"))?;
    let session = AuthSession::from_claims(claims)
        .map_err(|_| (StatusCode::UNAUTHORIZED, "Invalid or expired session"))?;
    let expiry =
        expires_at(&session).map_err(|_| (StatusCode::UNAUTHORIZED, "Invalid session expiry"))?;
    let operation = async {
        let mut tx = state
            .installation
            .pool()
            .begin()
            .await
            .map_err(unavailable)?;
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
    // The provider session continues until the browser visits this URL.
    let end_session_url = state.identity.end_session_url().await;
    Ok((
        StatusCode::OK,
        [
            (header::SET_COOKIE, security::clear_session_cookie()),
            (header::CACHE_CONTROL, "no-store"),
        ],
        axum::Json(LogoutResponse { end_session_url }),
    ))
}

pub(crate) async fn cleanup_expired(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL statement_timeout='2s'")
        .execute(&mut *tx)
        .await?;
    let mut count = sqlx::query("DELETE FROM auth_sessions WHERE id IN (SELECT id FROM auth_sessions WHERE expires_at<=NOW() ORDER BY expires_at LIMIT 1000 FOR UPDATE SKIP LOCKED)")
        .execute(&mut *tx).await?.rows_affected();
    // Abandoned sign-ins: the browser never came back from the provider.
    count += sqlx::query("DELETE FROM oidc_login_attempts WHERE state_digest IN (SELECT state_digest FROM oidc_login_attempts WHERE expires_at<=NOW() ORDER BY expires_at LIMIT 1000 FOR UPDATE SKIP LOCKED)")
        .execute(&mut *tx).await?.rows_affected();
    tx.commit().await?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::site_config::{ParkingLotConfig, PathNode, SiteConfig};
    use axum::response::IntoResponse;

    async fn fixture(pool: PgPool) -> (SharedState, Uuid) {
        let verified = crate::installation::test_support::provision(&pool).await;
        let user = Uuid::new_v4();
        sqlx::query("INSERT INTO users (id, role) VALUES ($1, 'user')")
            .bind(user)
            .execute(&pool)
            .await
            .unwrap();
        let state = crate::state::init_state(
            verified,
            "session-test-secret".into(),
            "test-pepper".into(),
            crate::personal_data::Keyring::for_unit_test(),
            crate::identity::IdentityProvider::for_unit_test(),
            crate::installation::test_support::package(SiteConfig {
                directory: std::env::temp_dir(),
                spots: vec![],
                parkings: vec![ParkingLotConfig {
                    path: vec![PathNode { x: 0.0, z: 0.0 }, PathNode { x: 1.0, z: 0.0 }],
                }],
            }),
            Default::default(),
        )
        .await
        .unwrap();
        (state, user)
    }

    fn headers(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        headers
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn r03b_logout_revokes_only_one_independent_session(pool: PgPool) {
        let (state, user) = fixture(pool).await;
        let first = issue(state.installation.pool(), user, "user", &state.jwt_secret)
            .await
            .unwrap();
        let other = issue(state.installation.pool(), user, "user", &state.jwt_secret)
            .await
            .unwrap();
        assert_ne!(first, other);
        let active = security::authenticated_session(
            &headers(&first),
            &state.jwt_secret,
            state.installation.pool(),
        )
        .await
        .unwrap();
        let response = logout(State(state.clone()), headers(&first))
            .await
            .unwrap()
            .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response.headers()[header::SET_COOKIE]
                .to_str()
                .unwrap()
                .contains("Max-Age=0")
        );
        assert_eq!(
            active
                .revalidate(state.installation.pool())
                .await
                .unwrap_err()
                .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            security::authenticated_session(
                &headers(&first),
                &state.jwt_secret,
                state.installation.pool()
            )
            .await
            .unwrap_err()
            .0,
            StatusCode::UNAUTHORIZED
        );
        assert!(
            security::authenticated_session(
                &headers(&other),
                &state.jwt_secret,
                state.installation.pool()
            )
            .await
            .is_ok()
        );
        assert!(logout(State(state), headers(&first)).await.is_ok());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn r03b_logout_database_failure_does_not_claim_success(pool: PgPool) {
        let (state, user) = fixture(pool).await;
        let jwt = issue(state.installation.pool(), user, "user", &state.jwt_secret)
            .await
            .unwrap();
        sqlx::query("ALTER TABLE auth_sessions RENAME TO unavailable_sessions")
            .execute(state.installation.pool())
            .await
            .unwrap();
        let status = match logout(State(state.clone()), headers(&jwt)).await {
            Err((status, _)) => status,
            Ok(_) => panic!("logout must not succeed without committing revocation"),
        };
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        sqlx::query("ALTER TABLE unavailable_sessions RENAME TO auth_sessions")
            .execute(state.installation.pool())
            .await
            .unwrap();
        assert!(
            security::authenticated_session(
                &headers(&jwt),
                &state.jwt_secret,
                state.installation.pool()
            )
            .await
            .is_ok()
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn r03b_logout_serializes_with_ticket_issuance_and_removes_committed_ticket(
        pool: PgPool,
    ) {
        let (state, user) = fixture(pool).await;
        let jwt = issue(state.installation.pool(), user, "user", &state.jwt_secret)
            .await
            .unwrap();
        let session = security::authenticated_session(
            &headers(&jwt),
            &state.jwt_secret,
            state.installation.pool(),
        )
        .await
        .unwrap();
        let mut tx = state.installation.pool().begin().await.unwrap();
        assert!(lock_active(&mut tx, &session).await.unwrap());
        sqlx::query("INSERT INTO ws_tickets (digest,session_id,user_id,role,destination,session_expires_at,expires_at) VALUES ($1,$2,$3,'user','app',$4,NOW()+INTERVAL '30 seconds')")
            .bind(vec![1u8;32]).bind(session.session_id).bind(user).bind(session.expires_at() as i64)
            .execute(&mut *tx).await.unwrap();
        let mut revoke = Box::pin(logout(State(state.clone()), headers(&jwt)));
        assert!(
            timeout(Duration::from_millis(30), &mut revoke)
                .await
                .is_err()
        );
        tx.commit().await.unwrap();
        assert!(revoke.await.is_ok());
        let pending: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM ws_tickets WHERE session_id=$1")
                .bind(session.session_id)
                .fetch_one(state.installation.pool())
                .await
                .unwrap();
        assert_eq!(pending, 0);
        let mut tx = state.installation.pool().begin().await.unwrap();
        assert!(!lock_active(&mut tx, &session).await.unwrap());
        tx.rollback().await.unwrap();
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn r03b_expired_session_cleanup_is_bounded(pool: PgPool) {
        let (state, user) = fixture(pool).await;
        for _ in 0..1001 {
            sqlx::query("INSERT INTO auth_sessions (id,user_id,role,expires_at) VALUES ($1,$2,'user',NOW()-INTERVAL '1 second')")
                .bind(Uuid::new_v4()).bind(user).execute(state.installation.pool()).await.unwrap();
        }
        let valid = issue(state.installation.pool(), user, "user", &state.jwt_secret)
            .await
            .unwrap();
        assert_eq!(
            cleanup_expired(state.installation.pool()).await.unwrap(),
            1000
        );
        assert!(
            security::authenticated_session(
                &headers(&valid),
                &state.jwt_secret,
                state.installation.pool()
            )
            .await
            .is_ok()
        );
        assert_eq!(cleanup_expired(state.installation.pool()).await.unwrap(), 1);
    }
    #[sqlx::test(migrations = "./migrations")]
    async fn r03b_downgrade_cannot_discard_unexpired_revocation_state(pool: PgPool) {
        let (state, user) = fixture(pool).await;
        let jwt = issue(state.installation.pool(), user, "user", &state.jwt_secret)
            .await
            .unwrap();
        assert!(logout(State(state.clone()), headers(&jwt)).await.is_ok());
        let mut tx = state.installation.pool().begin().await.unwrap();
        let down = sqlx::raw_sql(include_str!(
            "../migrations/20261018020000_revocable_sessions.down.sql"
        ))
        .execute(&mut *tx)
        .await;
        assert!(
            down.is_err(),
            "downgrade must preserve unexpired revoked credentials instead of re-enabling them on an old server"
        );
        tx.rollback().await.unwrap();
        assert_eq!(
            security::authenticated_session(
                &headers(&jwt),
                &state.jwt_secret,
                state.installation.pool()
            )
            .await
            .unwrap_err()
            .0,
            StatusCode::UNAUTHORIZED
        );
        sqlx::query("UPDATE auth_sessions SET expires_at=NOW()-INTERVAL '1 second'")
            .execute(state.installation.pool())
            .await
            .unwrap();
        sqlx::raw_sql(include_str!(
            "../migrations/20261018020000_revocable_sessions.down.sql"
        ))
        .execute(state.installation.pool())
        .await
        .unwrap();
        sqlx::raw_sql(include_str!(
            "../migrations/20261018020000_revocable_sessions.up.sql"
        ))
        .execute(state.installation.pool())
        .await
        .unwrap();
        assert_eq!(
            security::authenticated_session(
                &headers(&jwt),
                &state.jwt_secret,
                state.installation.pool()
            )
            .await
            .unwrap_err()
            .0,
            StatusCode::UNAUTHORIZED
        );
    }
}
