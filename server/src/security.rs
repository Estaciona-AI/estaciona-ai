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
    Argon2,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
};
use axum::http::{HeaderMap, StatusCode, header};
use chrono::{Duration, Utc};
use hmac::{Hmac, KeyInit, Mac};
use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation, decode, encode};
use rand_core::OsRng;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
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
    let token_data = decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &Validation::default(),
    )?;

    Ok(token_data.claims)
}

pub type AuthError = (StatusCode, String);

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

pub fn authenticated_user(headers: &HeaderMap, secret: &str) -> Result<Uuid, AuthError> {
    let claims = authenticated_claims(headers, secret)?;
    Uuid::parse_str(&claims.sub)
        .map_err(|_| (StatusCode::UNAUTHORIZED, "Invalid user token".to_string()))
}

pub fn require_admin(headers: &HeaderMap, secret: &str) -> Result<(), AuthError> {
    let claims = authenticated_claims(headers, secret)?;
    if claims.role != "admin" {
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
            require_admin(&headers, "secret").unwrap_err().0,
            StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn authenticated_user_accepts_bearer_or_cookie_and_rejects_missing_token() {
        let user_id = Uuid::new_v4();
        let token = create_jwt(&user_id.to_string(), "user", "secret").unwrap();
        let mut headers = HeaderMap::new();
        assert_eq!(
            authenticated_user(&headers, "secret").unwrap_err().0,
            StatusCode::UNAUTHORIZED
        );
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        assert_eq!(authenticated_user(&headers, "secret").unwrap(), user_id);
        headers.remove(header::AUTHORIZATION);
        headers.insert(
            header::COOKIE,
            format!("estaciona_token={token}").parse().unwrap(),
        );
        assert_eq!(authenticated_user(&headers, "secret").unwrap(), user_id);
        assert_eq!(
            require_admin(&headers, "secret").unwrap_err().0,
            StatusCode::FORBIDDEN
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
}
