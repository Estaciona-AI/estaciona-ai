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

use argon2::{
    Argon2, Params,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::{Duration, Utc};
use hmac::{Hmac, KeyInit, Mac};
use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation, decode, encode};
use rand_core::OsRng;
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

// Segurança de senhas com Argon2
pub fn hash_password(password: &str) -> Result<String, argon2::password_hash::Error> {
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();

    let password_hash = argon2
        .hash_password(password.as_bytes(), &salt)?
        .to_string();

    Ok(password_hash)
}

pub fn verify_password(password: &str, password_hash: &str) -> bool {
    let parsed_hash = match PasswordHash::new(password_hash) {
        Ok(hash) => hash,
        Err(_) => return false,
    };

    Argon2::default()
        .verify_password(password.as_bytes(), &parsed_hash)
        .is_ok()
}

#[derive(Debug)]
pub enum PasswordWorkError {
    Work(crate::auth_limits::AuthLimitError),
    Hash(argon2::password_hash::Error),
    UnsupportedCost,
}

impl IntoResponse for PasswordWorkError {
    fn into_response(self) -> Response {
        match self {
            Self::Work(error) => error.into_response(),
            Self::Hash(_error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Password processing failed",
            )
                .into_response(),
            Self::UnsupportedCost => (
                StatusCode::SERVICE_UNAVAILABLE,
                [(header::RETRY_AFTER, "1")],
                "Password verification unavailable",
            )
                .into_response(),
        }
    }
}

impl From<crate::auth_limits::AuthLimitError> for PasswordWorkError {
    fn from(error: crate::auth_limits::AuthLimitError) -> Self {
        Self::Work(error)
    }
}

impl From<argon2::password_hash::Error> for PasswordWorkError {
    fn from(error: argon2::password_hash::Error) -> Self {
        Self::Hash(error)
    }
}

const MAX_PASSWORD_MEMORY_KIB: u32 = 65_536;
const MAX_PASSWORD_ITERATIONS: u32 = 6;
const MAX_PASSWORD_PARALLELISM: u32 = 4;

fn password_hash_allowed(password_hash: &str) -> Result<bool, PasswordWorkError> {
    let hash = match PasswordHash::new(password_hash) {
        Ok(hash) => hash,
        Err(_) => return Ok(false),
    };
    if argon2::Algorithm::try_from(hash.algorithm).is_err()
        || hash
            .version
            .is_some_and(|version| argon2::Version::try_from(version).is_err())
    {
        return Ok(false);
    }
    let (Some(salt), Some(_)) = (hash.salt, hash.hash) else {
        return Ok(false);
    };
    let mut salt_buffer = [0u8; 64];
    match salt.decode_b64(&mut salt_buffer) {
        Ok(bytes) if bytes.len() >= argon2::MIN_SALT_LEN => {}
        _ => return Ok(false),
    }

    // Params::new multiplies p * 8 before its own upper-bound check.
    if let Some(value) = hash.params.get("p") {
        match value.decimal() {
            Ok(parallelism) if parallelism <= Params::MAX_P_COST => {}
            _ => return Ok(false),
        }
    }
    let params = match Params::try_from(&hash) {
        Ok(params) => params,
        Err(_) => return Ok(false),
    };
    if params.m_cost() > MAX_PASSWORD_MEMORY_KIB
        || params.t_cost() > MAX_PASSWORD_ITERATIONS
        || params.p_cost() > MAX_PASSWORD_PARALLELISM
    {
        return Err(PasswordWorkError::UnsupportedCost);
    }
    Ok(true)
}

pub async fn hash_password_async(
    limits: &crate::auth_limits::AuthLimits,
    password: String,
) -> Result<String, PasswordWorkError> {
    limits
        .run_blocking(move || hash_password(&password))
        .await?
        .map_err(PasswordWorkError::Hash)
}

pub async fn verify_password_async(
    limits: &crate::auth_limits::AuthLimits,
    password: String,
    password_hash: String,
) -> Result<bool, PasswordWorkError> {
    if !password_hash_allowed(&password_hash)? {
        return Ok(false);
    }
    limits
        .run_blocking(move || verify_password(&password, &password_hash))
        .await
        .map_err(PasswordWorkError::Work)
}

// Autenticação de sessão com JWT
#[derive(Debug, Serialize, Deserialize)]
pub struct Claims {
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
        .checked_add_signed(Duration::hours(24))
        .expect("Invalid timestamp")
        .timestamp() as usize;

    let claims = Claims {
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
            user_id,
            role: claims.role,
            expires_at: claims.exp,
            deadline,
        })
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
            sqlx::query_scalar::<_, String>("SELECT role FROM users WHERE id = $1")
                .bind(self.user_id)
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
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let cookie = headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            value
                .split(';')
                .find_map(|part| part.trim().strip_prefix("estaciona_token="))
        });
    let token = bearer
        .or(cookie)
        .ok_or((StatusCode::UNAUTHORIZED, "Missing token".to_string()))?;
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

    const PHC_SALT: &str = "MTIzNDU2Nzg";
    const PHC_OUTPUT: &str = "QUFBQUFBQUFBQUFBQUFBQQ";

    fn phc_fixture(params: &str) -> String {
        if params.is_empty() {
            format!("$argon2id$v=19${PHC_SALT}${PHC_OUTPUT}")
        } else {
            format!("$argon2id$v=19${params}${PHC_SALT}${PHC_OUTPUT}")
        }
    }

    #[test]
    fn r04_phc_cost_ceiling_is_inclusive_and_defaults_match_argon2() {
        for params in [
            "m=65536,t=6,p=4",
            "m=19456,t=2,p=1",
            "t=2,p=1",
            "m=19456,p=1",
            "m=19456,t=2",
            "",
        ] {
            let hash = phc_fixture(params);
            assert!(password_hash_allowed(&hash).unwrap(), "{params}");
        }
    }

    #[test]
    fn r04_phc_cost_above_budget_is_rejected_without_kdf_allocation() {
        for params in [
            "m=65537,t=2,p=1",
            "m=4294967295,t=2,p=1",
            "m=19456,t=7,p=1",
            "m=19456,t=4294967295,p=1",
            "m=65536,t=2,p=5",
            "m=4294967295,t=2,p=16777215",
        ] {
            assert!(
                matches!(
                    password_hash_allowed(&phc_fixture(params)),
                    Err(PasswordWorkError::UnsupportedCost)
                ),
                "{params}",
            );
        }
    }

    #[test]
    fn r04_invalid_phc_is_an_authentication_failure_without_panicking() {
        for params in [
            "m=0,t=2,p=1",
            "m=8,t=0,p=1",
            "m=8,t=1,p=0",
            "m=8,t=1,p=2",
            "m=4294967295,t=1,p=4294967295",
            "m=4294967296,t=2,p=1",
            "m=invalid,t=2,p=1",
            "m=19456,t=2,p=1,unknown=1",
            "m=65537,t=2,p=1,unknown=1",
        ] {
            assert!(
                !password_hash_allowed(&phc_fixture(params)).unwrap(),
                "{params}"
            );
        }
        for hash in [
            "",
            "not-a-password-hash",
            "$argon2id$v=20$m=8,t=1,p=1$MTIzNDU2Nzg$QUFBQUFBQUFBQUFBQUFBQQ",
            "$unknown$v=19$m=65537,t=1,p=1$MTIzNDU2Nzg$QUFBQUFBQUFBQUFBQUFBQQ",
            "$argon2id$v=19$m=8,t=1,p=1$YWJj$QUFBQUFBQUFBQUFBQUFBQQ",
            "$argon2id$v=19$m=8,t=1,p=1$----$QUFBQUFBQUFBQUFBQUFBQQ",
            "$argon2id$v=19$m=8,t=1,p=1$MTIzNDU2Nzg$YQ",
            "$argon2id$v=19$m=8,t=1,p=1$MTIzNDU2Nzg",
            "$argon2id$v=19$m=8,t=1,p=1",
        ] {
            assert!(!password_hash_allowed(hash).unwrap(), "{hash}");
        }
    }

    #[tokio::test]
    async fn r04_async_password_hashing_remains_salted_and_verifiable() {
        let limits = crate::auth_limits::AuthLimits::default();
        let first = hash_password_async(&limits, "r04-secret".to_owned())
            .await
            .unwrap();
        let second = hash_password_async(&limits, "r04-secret".to_owned())
            .await
            .unwrap();
        assert_ne!(first, second);
        assert!(verify_password("r04-secret", &first));
        assert!(
            verify_password_async(&limits, "r04-secret".to_owned(), first.clone())
                .await
                .unwrap()
        );
        assert!(
            verify_password_async(
                &limits,
                "r04-secret".to_owned(),
                first.replace("$m=19456,t=2,p=1", "").replace("$v=19", ""),
            )
            .await
            .unwrap()
        );
        assert!(
            !verify_password_async(&limits, "wrong-secret".to_owned(), first)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn r04_async_verification_preserves_supported_algorithms_versions_and_output() {
        let limits = crate::auth_limits::AuthLimits::default();
        let salt = SaltString::encode_b64(b"12345678").unwrap();
        for algorithm in [
            argon2::Algorithm::Argon2d,
            argon2::Algorithm::Argon2i,
            argon2::Algorithm::Argon2id,
        ] {
            for version in [argon2::Version::V0x10, argon2::Version::V0x13] {
                for output_len in [16, 64] {
                    let params = argon2::Params::new(32, 3, 2, Some(output_len)).unwrap();
                    let hash = Argon2::new(algorithm, version, params)
                        .hash_password(b"legacy-secret", &salt)
                        .unwrap()
                        .to_string();
                    assert!(
                        verify_password_async(&limits, "legacy-secret".to_owned(), hash)
                            .await
                            .unwrap()
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn r04_invalid_and_over_budget_phc_do_not_acquire_a_busy_worker() {
        let limits = std::sync::Arc::new(crate::auth_limits::AuthLimits::default());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worker_limits = limits.clone();
        let worker = tokio::spawn(async move {
            worker_limits
                .run_blocking(move || {
                    started_tx.send(()).unwrap();
                    release_rx.recv_timeout(StdDuration::from_secs(5)).unwrap();
                })
                .await
        });
        tokio::time::timeout(StdDuration::from_secs(2), started_rx)
            .await
            .unwrap()
            .unwrap();

        let valid_phc = phc_fixture("m=8,t=1,p=1");
        let verify_busy = verify_password_async(&limits, "secret".to_owned(), valid_phc).await;
        let hash_busy = hash_password_async(&limits, "secret".to_owned()).await;
        let invalid = verify_password_async(
            &limits,
            "secret".to_owned(),
            "not-a-password-hash".to_owned(),
        )
        .await;
        let over_budget = verify_password_async(
            &limits,
            "secret".to_owned(),
            phc_fixture("m=4294967295,t=2,p=1"),
        )
        .await;

        release_tx.send(()).unwrap();
        worker.await.unwrap().unwrap();
        assert!(matches!(
            verify_busy,
            Err(PasswordWorkError::Work(
                crate::auth_limits::AuthLimitError::Unavailable
            ))
        ));
        assert!(matches!(
            hash_busy,
            Err(PasswordWorkError::Work(
                crate::auth_limits::AuthLimitError::Unavailable
            ))
        ));
        assert!(!invalid.unwrap());
        assert!(matches!(
            over_budget,
            Err(PasswordWorkError::UnsupportedCost)
        ));
    }

    #[test]
    fn r04_password_work_errors_preserve_http_status_and_retry_headers() {
        let unsupported = PasswordWorkError::UnsupportedCost.into_response();
        assert_eq!(unsupported.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(unsupported.headers()[header::RETRY_AFTER], "1");
        let hashing =
            PasswordWorkError::Hash(argon2::password_hash::Error::Password).into_response();
        assert_eq!(hashing.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!hashing.headers().contains_key(header::RETRY_AFTER));
        for (error, status, retry) in [
            (
                crate::auth_limits::AuthLimitError::RateLimited,
                StatusCode::TOO_MANY_REQUESTS,
                true,
            ),
            (
                crate::auth_limits::AuthLimitError::Unavailable,
                StatusCode::SERVICE_UNAVAILABLE,
                true,
            ),
            (
                crate::auth_limits::AuthLimitError::Internal,
                StatusCode::INTERNAL_SERVER_ERROR,
                false,
            ),
        ] {
            let response = PasswordWorkError::Work(error).into_response();
            assert_eq!(response.status(), status);
            assert_eq!(response.headers().contains_key(header::RETRY_AFTER), retry);
        }
    }

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
            format!("estaciona_token={token}").parse().unwrap(),
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
    fn hash_password_returns_valid_hash() {
        let hash = hash_password("my_secure_password").unwrap();
        assert!(hash.starts_with("$argon2"));
    }

    #[test]
    fn hash_password_produces_unique_salts() {
        let hash1 = hash_password("same_password").unwrap();
        let hash2 = hash_password("same_password").unwrap();
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn verify_password_correct() {
        let password = "test_password_123";
        let hash = hash_password(password).unwrap();
        assert!(verify_password(password, &hash));
    }

    #[test]
    fn verify_password_wrong() {
        let hash = hash_password("correct_password").unwrap();
        assert!(!verify_password("wrong_password", &hash));
    }

    #[test]
    fn verify_password_invalid_hash() {
        assert!(!verify_password("any_password", "not_a_valid_hash"));
    }

    #[test]
    fn verify_password_empty_hash() {
        assert!(!verify_password("any_password", ""));
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
}
