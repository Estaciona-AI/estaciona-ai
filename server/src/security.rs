// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

use axum::http::{HeaderMap, StatusCode, header};
use chrono::{Duration, Utc};
use hmac::{Hmac, KeyInit, Mac};
use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use sqlx::PgPool;
use std::time::{Duration as StdDuration, SystemTime, UNIX_EPOCH};
use tokio::time::{Instant, MissedTickBehavior, interval_at, timeout_at};
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

// Anonimação de placas
pub fn hash_plate(plate: &str, pepper: &str) -> String {
    let mut mac =
        HmacSha256::new_from_slice(pepper.as_bytes()).expect("HMAC can take key of any size");

    let normalized_plate = plate.to_uppercase().replace("-", "").replace(" ", "");

    mac.update(normalized_plate.as_bytes());
    let result = mac.finalize();
    let bytes = result.into_bytes();

    use std::fmt::Write;
    let mut hex_string = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        write!(&mut hex_string, "{:02x}", b).unwrap();
    }

    hex_string
}

// Autenticação de sessão com JWT

/// A work shift; shorter than the identity provider's maximum session.
pub const SESSION_HOURS: i64 = 8;
/// `__Host-`: Secure, no Domain, Path=/. A sibling subdomain cannot set it.
pub const SESSION_COOKIE: &str = "__Host-estaciona_session";

/// The session cookie the browser keeps; JavaScript never sees the token.
pub fn session_cookie(token: &str) -> String {
    format!(
        "{SESSION_COOKIE}={token}; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age={}",
        SESSION_HOURS * 3600
    )
}

pub fn clear_session_cookie() -> &'static str {
    "__Host-estaciona_session=; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=0"
}

pub fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|value| !value.is_empty())
}

pub fn session_cookie_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .find_map(|part| part.trim().strip_prefix(SESSION_COOKIE)?.strip_prefix('='))
        .filter(|value| !value.is_empty())
}

/// Bearer for programs, the session cookie for browsers.
pub fn session_token(headers: &HeaderMap) -> Option<&str> {
    bearer_token(headers).or_else(|| session_cookie_token(headers))
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Claims {
    pub jti: Uuid,    // One independently revokable login session.
    pub sub: String,  // ID
    pub role: String, // Função
    pub exp: usize,   // Timestamp
}

pub fn create_jwt(
    subject: &str,
    role: &str,
    secret: &str,
) -> Result<String, jsonwebtoken::errors::Error> {
    let expiration = Utc::now()
        .checked_add_signed(Duration::hours(SESSION_HOURS))
        .expect("Invalid timestamp")
        .timestamp() as usize;

    let claims = Claims {
        jti: Uuid::new_v4(),
        sub: subject.to_owned(),
        role: role.to_owned(),
        exp: expiration,
    };

    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
}

pub fn verify_jwt(token: &str, secret: &str) -> Result<Claims, jsonwebtoken::errors::Error> {
    let mut validation = Validation::default();
    validation.leeway = 0;
    let token_data = decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    )?;

    // jsonwebtoken accepts exp == now; our session ends at exp, without grace.
    if token_data.claims.exp as u64 <= jsonwebtoken::get_current_timestamp() {
        return Err(jsonwebtoken::errors::ErrorKind::ExpiredSignature.into());
    }

    Ok(token_data.claims)
}

pub type AuthError = (StatusCode, String);

const AUTHORIZATION_TIMEOUT: StdDuration = StdDuration::from_secs(2);
const SESSION_RECHECK_INTERVAL: StdDuration = StdDuration::from_secs(5);

#[derive(Debug)]
pub struct AuthSession {
    pub session_id: Uuid,
    pub user_id: Uuid,
    pub role: String,
    expires_at: usize,
    pub deadline: Instant,
}

fn expired_session() -> AuthError {
    (StatusCode::UNAUTHORIZED, "Expired session".to_owned())
}

fn authorization_unavailable() -> AuthError {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "Authorization unavailable".to_owned(),
    )
}

impl AuthSession {
    pub fn from_claims(claims: Claims) -> Result<Self, AuthError> {
        let user_id = Uuid::parse_str(&claims.sub)
            .map_err(|_| (StatusCode::UNAUTHORIZED, "Invalid user token".to_owned()))?;
        let monotonic_now = Instant::now();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| expired_session())?;
        let remaining = StdDuration::from_secs(claims.exp as u64)
            .checked_sub(now)
            .filter(|duration| !duration.is_zero())
            .ok_or_else(expired_session)?;
        let deadline = monotonic_now
            .checked_add(remaining)
            .ok_or_else(expired_session)?;
        Ok(Self {
            session_id: claims.jti,
            user_id,
            role: claims.role,
            expires_at: claims.exp,
            deadline,
        })
    }

    pub fn expires_at(&self) -> usize {
        self.expires_at
    }

    pub fn check_expiration(&self) -> Result<(), AuthError> {
        if Instant::now() >= self.deadline
            || jsonwebtoken::get_current_timestamp() >= self.expires_at as u64
        {
            return Err(expired_session());
        }
        Ok(())
    }

    pub async fn revalidate(&self, pool: &PgPool) -> Result<(), AuthError> {
        self.check_expiration()?;
        let query_deadline = self.deadline.min(Instant::now() + AUTHORIZATION_TIMEOUT);
        let result = timeout_at(
            query_deadline,
            sqlx::query_scalar::<_, String>("SELECT u.role FROM auth_sessions s JOIN users u ON u.id=s.user_id WHERE s.id=$1 AND s.user_id=$2 AND s.role=$3 AND s.expires_at=$4 AND s.expires_at>NOW() AND s.revoked_at IS NULL")
                .bind(self.session_id)
                .bind(self.user_id)
                .bind(&self.role)
                .bind(crate::sessions::expires_at(self)?)
                .fetch_optional(pool),
        )
        .await;
        // A query that finishes after exp must not extend the session.
        self.check_expiration()?;
        let role = result
            .map_err(|_| authorization_unavailable())?
            .map_err(|error| {
                tracing::error!(%error, "Session authorization query failed");
                authorization_unavailable()
            })?
            .ok_or((StatusCode::UNAUTHORIZED, "Invalid user session".to_owned()))?;
        if role != self.role {
            return Err((
                StatusCode::FORBIDDEN,
                "Session permissions changed".to_owned(),
            ));
        }
        Ok(())
    }

    pub async fn wait_for_invalidation(&self, pool: &PgPool) -> AuthError {
        let mut interval = interval_at(
            Instant::now() + SESSION_RECHECK_INTERVAL,
            SESSION_RECHECK_INTERVAL,
        );
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if let Err(error) = self.revalidate(pool).await {
                return error;
            }
        }
    }
}

pub fn authenticated_claims(headers: &HeaderMap, secret: &str) -> Result<Claims, AuthError> {
    let token =
        session_token(headers).ok_or((StatusCode::UNAUTHORIZED, "Missing token".to_string()))?;
    verify_jwt(token, secret).map_err(|_| (StatusCode::UNAUTHORIZED, "Invalid token".to_string()))
}

pub async fn authenticated_session(
    headers: &HeaderMap,
    secret: &str,
    pool: &PgPool,
) -> Result<AuthSession, AuthError> {
    let claims = authenticated_claims(headers, secret)?;
    let session = AuthSession::from_claims(claims)?;
    session.revalidate(pool).await?;
    Ok(session)
}

pub async fn authenticated_user(
    headers: &HeaderMap,
    secret: &str,
    pool: &PgPool,
) -> Result<Uuid, AuthError> {
    Ok(authenticated_session(headers, secret, pool).await?.user_id)
}

pub async fn require_admin(
    headers: &HeaderMap,
    secret: &str,
    pool: &PgPool,
) -> Result<(), AuthError> {
    let session = authenticated_session(headers, secret, pool).await?;
    if session.role != "admin" {
        return Err((StatusCode::FORBIDDEN, "Admin required".to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_token_cannot_grant_backend_admin_access() {
        assert!(verify_jwt("demo-token", "secret").is_err());
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer demo-token".parse().unwrap());
        assert_eq!(
            authenticated_claims(&headers, "secret").unwrap_err().0,
            StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn authenticated_user_accepts_bearer_or_cookie_and_rejects_missing_token() {
        let user_id = Uuid::new_v4();
        let token = create_jwt(&user_id.to_string(), "user", "secret").unwrap();
        let mut headers = HeaderMap::new();
        assert_eq!(
            authenticated_claims(&headers, "secret").unwrap_err().0,
            StatusCode::UNAUTHORIZED
        );
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        assert_eq!(
            AuthSession::from_claims(authenticated_claims(&headers, "secret").unwrap())
                .unwrap()
                .user_id,
            user_id
        );
        headers.remove(header::AUTHORIZATION);
        headers.insert(
            header::COOKIE,
            format!("theme=dark; {SESSION_COOKIE}={token}")
                .parse()
                .unwrap(),
        );
        assert_eq!(
            AuthSession::from_claims(authenticated_claims(&headers, "secret").unwrap())
                .unwrap()
                .user_id,
            user_id
        );
    }

    #[test]
    fn hash_plate_is_deterministic() {
        let hash1 = hash_plate("ABC1234", "pepper");
        let hash2 = hash_plate("ABC1234", "pepper");
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn hash_plate_normalizes_dashes() {
        let with_dash = hash_plate("ABC-1D23", "pepper");
        let without_dash = hash_plate("ABC1D23", "pepper");
        assert_eq!(with_dash, without_dash);
    }

    #[test]
    fn hash_plate_normalizes_spaces() {
        let with_space = hash_plate("ABC 1D23", "pepper");
        let without_space = hash_plate("ABC1D23", "pepper");
        assert_eq!(with_space, without_space);
    }

    #[test]
    fn hash_plate_normalizes_case() {
        let lower = hash_plate("abc1d23", "pepper");
        let upper = hash_plate("ABC1D23", "pepper");
        assert_eq!(lower, upper);
    }

    #[test]
    fn hash_plate_different_pepper_produces_different_hash() {
        let hash1 = hash_plate("ABC1234", "pepper1");
        let hash2 = hash_plate("ABC1234", "pepper2");
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn hash_plate_different_plates_produce_different_hashes() {
        let hash1 = hash_plate("ABC1234", "pepper");
        let hash2 = hash_plate("XYZ9876", "pepper");
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn hash_plate_returns_hex_string() {
        let hash = hash_plate("ABC1234", "pepper");
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(hash.len(), 64);
    }

    #[test]
    fn create_jwt_returns_token() {
        let token = create_jwt("user-123", "admin", "secret").unwrap();
        assert!(!token.is_empty());
        assert_eq!(token.matches('.').count(), 2);
    }

    #[test]
    fn verify_jwt_roundtrip() {
        let secret = "test_secret_key";
        let token = create_jwt("user-456", "viewer", secret).unwrap();
        let claims = verify_jwt(&token, secret).unwrap();
        assert_eq!(claims.sub, "user-456");
        assert_eq!(claims.role, "viewer");
    }

    #[test]
    fn verify_jwt_wrong_secret_fails() {
        let token = create_jwt("user-789", "admin", "correct_secret").unwrap();
        let result = verify_jwt(&token, "wrong_secret");
        assert!(result.is_err());
    }

    #[test]
    fn verify_jwt_garbage_token_fails() {
        let result = verify_jwt("not.a.jwt", "secret");
        assert!(result.is_err());
    }

    #[test]
    fn verify_jwt_empty_token_fails() {
        let result = verify_jwt("", "secret");
        assert!(result.is_err());
    }

    #[test]
    fn create_jwt_expiration_is_in_the_future() {
        let secret = "test_secret";
        let token = create_jwt("user", "role", secret).unwrap();
        let claims = verify_jwt(&token, secret).unwrap();
        let now = Utc::now().timestamp() as usize;
        assert!(claims.exp > now);
    }

    #[test]
    fn verify_jwt_expired_token_fails() {
        let secret = "test_secret";
        let claims = Claims {
            jti: Uuid::new_v4(),
            sub: "user".to_owned(),
            role: "admin".to_owned(),
            exp: 0,
        };
        let token = encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(secret.as_bytes()),
        )
        .unwrap();
        let result = verify_jwt(&token, secret);
        assert!(result.is_err());
    }

    #[test]
    fn verify_jwt_rejects_expiration_without_grace_period() {
        for seconds_ago in [0, 1, 30] {
            let claims = Claims {
                jti: Uuid::new_v4(),
                sub: Uuid::new_v4().to_string(),
                role: "user".to_owned(),
                exp: (Utc::now().timestamp() - seconds_ago) as usize,
            };
            let token = encode(
                &Header::default(),
                &claims,
                &EncodingKey::from_secret(b"secret"),
            )
            .unwrap();
            assert!(
                verify_jwt(&token, "secret").is_err(),
                "a token expired {seconds_ago} seconds ago must be rejected"
            );
        }
    }
    #[sqlx::test(migrations = "./migrations")]
    async fn r03b_signed_but_unregistered_session_is_not_authorization(pool: PgPool) {
        let user = Uuid::new_v4();
        sqlx::query("INSERT INTO users (id, role) VALUES ($1, 'user')")
            .bind(user)
            .execute(&pool)
            .await
            .unwrap();
        let token = create_jwt(&user.to_string(), "user", "r03b-secret").unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        let result = authenticated_session(&headers, "r03b-secret", &pool).await;
        assert!(
            result.is_err(),
            "a signature alone must not authorize a session absent from the registry"
        );
    }
}
