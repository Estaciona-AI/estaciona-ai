// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

use axum::{
    Form, Router,
    extract::{DefaultBodyLimit, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tower_http::limit::RequestBodyLimitLayer;
use uuid::Uuid;

use super::{LogoutTarget, SignedIn};
use crate::{personal_data::Field, security, state::SharedState};

/// Binds a sign-in to the browser that started it (no login CSRF).
const SIGN_IN_COOKIE: &str = "__Host-estaciona_signin";
const SIGN_IN_SECONDS: i64 = 600;
const MAX_RETURN_PATH: usize = 256;
const LOGOUT_BODY_LIMIT: usize = 16 * 1024;

pub fn routes(state: SharedState) -> Router<SharedState> {
    let guarded = Router::new()
        .route("/auth/login", get(start))
        .route("/auth/callback", get(finish))
        .route_layer(axum::middleware::from_fn_with_state(
            state,
            crate::auth_limits::guard_sign_in,
        ));
    let backchannel = Router::new()
        .route("/auth/backchannel-logout", post(backchannel_logout))
        .layer(DefaultBodyLimit::max(LOGOUT_BODY_LIMIT))
        .layer(RequestBodyLimitLayer::new(LOGOUT_BODY_LIMIT));
    guarded.merge(backchannel)
}

fn digest(state: &str) -> Vec<u8> {
    Sha256::digest(state.as_bytes()).to_vec()
}

/// A path inside the app, never another origin: `/x`, not `//x` or `/\x`.
fn return_path(value: Option<&str>) -> Option<String> {
    let path = value.unwrap_or("/");
    let valid = path.starts_with('/')
        && !path.starts_with("//")
        && path.len() <= MAX_RETURN_PATH
        && path
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && byte != b'\\');
    valid.then(|| path.to_owned())
}

fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .find_map(|part| part.trim().strip_prefix(name)?.strip_prefix('='))
}

fn no_store(response: &mut Response) {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
}

fn redirect(location: &str, cookies: &[String]) -> Response {
    let mut response = StatusCode::SEE_OTHER.into_response();
    match HeaderValue::from_str(location) {
        Ok(value) => {
            response.headers_mut().insert(header::LOCATION, value);
        }
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
    for cookie in cookies {
        if let Ok(value) = HeaderValue::from_str(cookie) {
            response.headers_mut().append(header::SET_COOKIE, value);
        }
    }
    no_store(&mut response);
    response
}

fn clear_sign_in_cookie() -> String {
    format!("{SIGN_IN_COOKIE}=; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=0")
}

/// Every failure looks the same to the browser and starts over cleanly.
fn failed(status: StatusCode) -> Response {
    let mut response = (status, "Sign-in could not be completed; start again.").into_response();
    if let Ok(value) = HeaderValue::from_str(&clear_sign_in_cookie()) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    no_store(&mut response);
    response
}

#[derive(Deserialize)]
struct StartQuery {
    return_to: Option<String>,
}

async fn start(State(state): State<SharedState>, Query(query): Query<StartQuery>) -> Response {
    let Some(return_path) = return_path(query.return_to.as_deref()) else {
        return failed(StatusCode::BAD_REQUEST);
    };
    let sign_in = match state.identity.start().await {
        Ok(sign_in) => sign_in,
        Err(error) => {
            tracing::warn!(%error, "Sign-in unavailable");
            return failed(StatusCode::SERVICE_UNAVAILABLE);
        }
    };
    let stored = sqlx::query(
        "INSERT INTO oidc_login_attempts (state_digest, pkce_verifier, nonce, return_path, expires_at) \
         VALUES ($1, $2, $3, $4, NOW() + make_interval(secs => $5))",
    )
    .bind(digest(&sign_in.state))
    .bind(&sign_in.pkce_verifier)
    .bind(&sign_in.nonce)
    .bind(&return_path)
    .bind(SIGN_IN_SECONDS as f64)
    .execute(state.installation.pool())
    .await;
    if let Err(error) = stored {
        tracing::error!(%error, "Sign-in attempt could not be stored");
        return failed(StatusCode::SERVICE_UNAVAILABLE);
    }
    redirect(
        sign_in.authorize_url.as_str(),
        &[format!(
            "{SIGN_IN_COOKIE}={}; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age={SIGN_IN_SECONDS}",
            sign_in.state
        )],
    )
}

#[derive(Deserialize)]
struct FinishQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// The highest role this installation grants the account, if any.
fn local_role(roles: &[String]) -> Option<&'static str> {
    if roles.iter().any(|role| role == "admin") {
        Some("admin")
    } else if roles.iter().any(|role| role == "user") {
        Some("user")
    } else {
        None
    }
}

async fn finish(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(query): Query<FinishQuery>,
) -> Response {
    let (Some(returned), Some(started)) = (
        query.state.as_deref(),
        cookie_value(&headers, SIGN_IN_COOKIE),
    ) else {
        return failed(StatusCode::BAD_REQUEST);
    };
    // The state digest is the lookup key, so only a matching cookie finds a row.
    if returned != started {
        return failed(StatusCode::BAD_REQUEST);
    }
    let attempt: Result<Option<(String, String, String)>, _> = sqlx::query_as(
        "DELETE FROM oidc_login_attempts WHERE state_digest = $1 AND expires_at > NOW() \
         RETURNING pkce_verifier, nonce, return_path",
    )
    .bind(digest(returned))
    .fetch_optional(state.installation.pool())
    .await;
    let (pkce_verifier, nonce, return_path) = match attempt {
        Ok(Some(attempt)) => attempt,
        Ok(None) => return failed(StatusCode::BAD_REQUEST),
        Err(error) => {
            tracing::error!(%error, "Sign-in attempt lookup failed");
            return failed(StatusCode::SERVICE_UNAVAILABLE);
        }
    };
    let app_url = state.identity.config().app_url();
    if query.error.is_some() {
        // Cancelled at the provider: back to the app, signed out.
        return redirect(&format!("{app_url}/"), &[clear_sign_in_cookie()]);
    }
    let Some(code) = query.code else {
        return failed(StatusCode::BAD_REQUEST);
    };
    let signed_in = match state.identity.finish(code, pkce_verifier, nonce).await {
        Ok(signed_in) => signed_in,
        Err(error @ super::IdentityError::Discovery) => {
            tracing::warn!(%error, "Sign-in unavailable");
            return failed(StatusCode::SERVICE_UNAVAILABLE);
        }
        Err(error) => {
            tracing::warn!(%error, "Sign-in rejected");
            return failed(StatusCode::BAD_REQUEST);
        }
    };
    let Some(role) = local_role(&signed_in.roles) else {
        return failed(StatusCode::FORBIDDEN);
    };
    let user = match upsert_account(&state, &signed_in, role).await {
        Ok(user) => user,
        Err(error) => {
            tracing::error!(%error, "Account could not be stored");
            return failed(StatusCode::SERVICE_UNAVAILABLE);
        }
    };
    let token = match crate::sessions::issue_for_provider_session(
        state.installation.pool(),
        user,
        role,
        &state.jwt_secret,
        signed_in.provider_session.as_deref(),
    )
    .await
    {
        Ok(token) => token,
        Err(error) => {
            tracing::error!(%error, "Session creation failed");
            return failed(StatusCode::SERVICE_UNAVAILABLE);
        }
    };
    redirect(
        &format!("{app_url}{return_path}"),
        &[security::session_cookie(&token), clear_sign_in_cookie()],
    )
}

#[derive(Debug)]
enum AccountError {
    Database(sqlx::Error),
    Encryption(crate::personal_data::CipherError),
}

impl std::fmt::Display for AccountError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database(error) => write!(f, "account storage: {error}"),
            Self::Encryption(error) => write!(f, "account encryption: {error}"),
        }
    }
}

/// The provider is the authority for username, name and role; each sign-in
/// refreshes them. A role change also invalidates sessions with the old role.
async fn upsert_account(
    state: &SharedState,
    signed_in: &SignedIn,
    role: &str,
) -> Result<Uuid, AccountError> {
    // Two first sign-ins of one account race on the unique subject: retry once.
    for attempt in 0..2 {
        let mut tx = state
            .installation
            .pool()
            .begin()
            .await
            .map_err(AccountError::Database)?;
        let existing: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM users WHERE oidc_issuer = $1 AND oidc_subject = $2 FOR UPDATE",
        )
        .bind(&signed_in.issuer)
        .bind(&signed_in.subject)
        .fetch_optional(&mut *tx)
        .await
        .map_err(AccountError::Database)?;
        let id = existing.unwrap_or_else(Uuid::new_v4);
        let display_name = state
            .personal_data
            .encrypt(Field::UserDisplayName, id, &signed_in.display_name)
            .map_err(AccountError::Encryption)?;
        let written = if existing.is_some() {
            sqlx::query(
                "UPDATE users SET username = $2, display_name_enc = $3, role = $4 WHERE id = $1",
            )
            .bind(id)
            .bind(&signed_in.username)
            .bind(display_name)
            .bind(role)
            .execute(&mut *tx)
            .await
        } else {
            sqlx::query(
                "INSERT INTO users (id, oidc_issuer, oidc_subject, username, display_name_enc, role) \
                 VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(id)
            .bind(&signed_in.issuer)
            .bind(&signed_in.subject)
            .bind(&signed_in.username)
            .bind(display_name)
            .bind(role)
            .execute(&mut *tx)
            .await
        };
        match written {
            Ok(_) => {
                tx.commit().await.map_err(AccountError::Database)?;
                return Ok(id);
            }
            Err(error)
                if attempt == 0
                    && error
                        .as_database_error()
                        .is_some_and(|error| error.is_unique_violation()) =>
            {
                tx.rollback().await.map_err(AccountError::Database)?;
            }
            Err(error) => return Err(AccountError::Database(error)),
        }
    }
    unreachable!("the second attempt returns")
}

#[derive(Deserialize)]
struct BackchannelLogout {
    logout_token: String,
}

/// OIDC Back-Channel Logout 1.0: the provider ends a session or every session
/// of an account, for example when an administrator revokes access.
async fn backchannel_logout(
    State(state): State<SharedState>,
    Form(form): Form<BackchannelLogout>,
) -> Response {
    let target = match state.identity.verify_logout(&form.logout_token).await {
        Ok(target) => target,
        Err(error @ super::IdentityError::Discovery) => {
            tracing::warn!(%error, "Back-channel logout cannot be verified yet");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        Err(error) => {
            tracing::warn!(%error, "Back-channel logout rejected");
            let mut response = (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({"error": "invalid_request"})),
            )
                .into_response();
            no_store(&mut response);
            return response;
        }
    };
    let issuer = state.identity.config().issuer.as_str().to_owned();
    let revoked = match &target {
        LogoutTarget::ProviderSession(sid) => {
            crate::sessions::revoke_provider_session(state.installation.pool(), sid).await
        }
        LogoutTarget::Subject(subject) => {
            crate::sessions::revoke_account(state.installation.pool(), &issuer, subject).await
        }
    };
    let mut response = match revoked {
        Ok(count) => {
            tracing::info!(sessions = count, "Back-channel logout applied");
            StatusCode::OK.into_response()
        }
        Err(error) => {
            tracing::error!(%error, "Back-channel logout could not be applied");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    };
    no_store(&mut response);
    response
}

#[cfg(test)]
pub(super) mod unit {
    use super::*;

    #[test]
    fn return_paths_stay_inside_the_app() {
        assert_eq!(return_path(None).as_deref(), Some("/"));
        for path in ["/", "/app.html", "/app.html?campus=centro#vaga"] {
            assert_eq!(return_path(Some(path)).as_deref(), Some(path));
        }
        let long = format!("/{}", "a".repeat(MAX_RETURN_PATH));
        for path in [
            "",
            "app.html",
            "//evil.example",
            "/\\evil.example",
            "https://evil.example",
            "/a b",
            "/a\nb",
            long.as_str(),
        ] {
            assert_eq!(return_path(Some(path)), None, "{path:?}");
        }
    }

    #[test]
    fn the_highest_installation_role_wins_and_others_grant_nothing() {
        let roles = |values: &[&str]| values.iter().map(|v| v.to_string()).collect::<Vec<_>>();
        assert_eq!(local_role(&roles(&["user", "admin"])), Some("admin"));
        assert_eq!(local_role(&roles(&["user"])), Some("user"));
        assert_eq!(local_role(&roles(&["manage-account", "operator"])), None);
        assert_eq!(local_role(&[]), None);
    }

    #[test]
    fn only_the_named_cookie_is_read() {
        let mut headers = HeaderMap::new();
        headers.append(
            header::COOKIE,
            HeaderValue::from_static("x__Host-estaciona_signin=bad; __Host-estaciona_signin=good"),
        );
        assert_eq!(cookie_value(&headers, SIGN_IN_COOKIE), Some("good"));
    }
}
