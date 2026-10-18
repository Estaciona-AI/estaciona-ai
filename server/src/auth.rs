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
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};

use crate::security::{PasswordWorkError, verify_password_async};
use crate::state::SharedState;

pub enum AuthError {
    Status(StatusCode, &'static str),
    Password(PasswordWorkError),
}

impl From<PasswordWorkError> for AuthError {
    fn from(error: PasswordWorkError) -> Self {
        Self::Password(error)
    }
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        match self {
            Self::Status(status, message) => (status, message).into_response(),
            Self::Password(error) => error.into_response(),
        }
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct LoginRequest {
    pub email: String,
    pub password: String,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct LoginResponse {
    pub token: String,
    pub role: String,
    pub name: String,
    pub id: uuid::Uuid,
}

pub async fn login_dashboard(
    State(state): State<SharedState>,
    Json(payload): Json<LoginRequest>,
) -> Result<impl IntoResponse, AuthError> {
    if payload.email.is_empty()
        || payload.email.chars().count() > 255
        || payload.password.is_empty()
        || payload.password.len() > 1024
    {
        return Err(AuthError::Status(
            StatusCode::BAD_REQUEST,
            "Invalid login input",
        ));
    }
    let user = sqlx::query!(
        "SELECT id, password_hash as \"password_hash!\", role, name FROM users WHERE email = $1",
        payload.email
    )
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| {
        tracing::error!("Database error during login: {}", e);
        AuthError::Status(StatusCode::INTERNAL_SERVER_ERROR, "Database error")
    })?;

    let user_record = match user {
        Some(record) => record,
        None => {
            return Err(AuthError::Status(
                StatusCode::UNAUTHORIZED,
                "Invalid credentials",
            ));
        }
    };

    if !verify_password_async(
        &state.auth_limits,
        payload.password,
        user_record.password_hash,
    )
    .await?
    {
        return Err(AuthError::Status(
            StatusCode::UNAUTHORIZED,
            "Invalid credentials",
        ));
    }

    let token = crate::sessions::issue(
        &state.pool,
        user_record.id,
        &user_record.role,
        &state.jwt_secret,
    )
    .await
    .map_err(|error| {
        tracing::error!(%error, "Session creation failed");
        AuthError::Status(
            StatusCode::SERVICE_UNAVAILABLE,
            "Session creation unavailable",
        )
    })?;

    let response = LoginResponse {
        token: token.clone(),
        role: user_record.role,
        name: user_record.name.unwrap_or_default(),
        id: user_record.id,
    };

    let cookie_header = format!(
        "estaciona_token={}; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=86400",
        token
    );
    let headers = axum::response::AppendHeaders([(axum::http::header::SET_COOKIE, cookie_header)]);

    Ok((StatusCode::OK, headers, Json(response)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_request_deserializes() {
        let json_data = r#"{
            "email": "test@example.com",
            "password": "my_secure_password"
        }"#;

        let parsed: LoginRequest = serde_json::from_str(json_data).unwrap();
        assert_eq!(parsed.email, "test@example.com");
        assert_eq!(parsed.password, "my_secure_password");
    }

    #[test]
    fn login_response_serializes() {
        let response = LoginResponse {
            token: "jwt.token.here".to_string(),
            role: "admin".to_string(),
            name: "John Doe".to_string(),
            id: uuid::Uuid::nil(),
        };

        let json_str = serde_json::to_string(&response).unwrap();
        assert!(json_str.contains(r#""token":"jwt.token.here""#));
        assert!(json_str.contains(r#""role":"admin""#));
        assert!(json_str.contains(r#""name":"John Doe""#));
    }
}
