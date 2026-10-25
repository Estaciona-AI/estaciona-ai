// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::auth::AuthError;
use crate::security::hash_password_async;
use crate::state::SharedState;

#[derive(Deserialize, schemars::JsonSchema)]
pub struct CreateUser {
    pub name: String,
    pub email: String,
    pub password: String,
    pub date_of_birth: Option<NaiveDate>,
    pub pcd_status: Option<bool>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct UserResponse {
    pub id: Uuid,
    pub name: String,
    pub email: String,
    pub date_of_birth: Option<NaiveDate>,
    pub pcd_status: Option<bool>,
    pub role: String,
    pub created_at: Option<DateTime<Utc>>,
}

pub async fn create_user(
    State(state): State<SharedState>,
    Json(payload): Json<CreateUser>,
) -> Result<impl IntoResponse, AuthError> {
    if payload.name.is_empty()
        || payload.name.chars().count() > 255
        || payload.email.is_empty()
        || payload.email.chars().count() > 255
        || payload.password.is_empty()
        || payload.password.len() > 1024
    {
        return Err(AuthError::Status(
            StatusCode::BAD_REQUEST,
            "Invalid registration input",
        ));
    }

    let duplicate =
        sqlx::query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM users WHERE email = $1)")
            .bind(&payload.email)
            .fetch_one(state.installation.pool())
            .await
            .map_err(|error| {
                tracing::error!(%error, "Registration preflight failed");
                AuthError::Status(StatusCode::INTERNAL_SERVER_ERROR, "Failed to create user")
            })?;
    if duplicate {
        return Err(AuthError::Status(
            StatusCode::CONFLICT,
            "Email already registered",
        ));
    }

    let hashed_password = hash_password_async(&state.auth_limits, payload.password).await?;
    let new_id = Uuid::new_v4();

    let record = sqlx::query!(
        r#"
        INSERT INTO users (id, name, email, password_hash, date_of_birth, pcd_status)
        VALUES ($1, $2, $3, $4, $5, $6)
        RETURNING
        id,
        name as "name!",
        email as "email!",
        date_of_birth,
        pcd_status,
        role as "role!",
        created_at as "created_at?"
        "#,
        new_id,
        payload.name,
        payload.email,
        hashed_password,
        payload.date_of_birth,
        payload.pcd_status,
    )
    .fetch_one(state.installation.pool())
    .await
    .map_err(|e| {
        tracing::error!("Database error: {}", e);
        if e.as_database_error()
            .is_some_and(|error| error.is_unique_violation())
        {
            AuthError::Status(StatusCode::CONFLICT, "Email already registered")
        } else {
            AuthError::Status(StatusCode::INTERNAL_SERVER_ERROR, "Failed to create user")
        }
    })?;

    let response = UserResponse {
        id: record.id,
        name: record.name,
        email: record.email,
        date_of_birth: record.date_of_birth,
        pcd_status: record.pcd_status,
        role: record.role,
        created_at: record.created_at,
    };

    Ok((StatusCode::CREATED, Json(response)))
}

pub async fn get_user(
    State(state): State<SharedState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let requester = crate::security::authenticated_session(
        &headers,
        &state.jwt_secret,
        state.installation.pool(),
    )
    .await?;
    if requester.role != "admin" && requester.user_id != id {
        return Err((StatusCode::FORBIDDEN, "User access denied".to_string()));
    }
    let record = sqlx::query_as!(
        UserResponse,
        r#"
        SELECT 
        id,
        name as "name!",
        email as "email!",
        date_of_birth,
        pcd_status,
        role as "role!",
        created_at as "created_at?"
        FROM users
        WHERE id = $1
        "#,
        id
    )
    .fetch_optional(state.installation.pool())
    .await
    .map_err(|e| {
        tracing::error!("Database error: {}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to fetch user".to_string(),
        )
    })?;

    match record {
        Some(user) => Ok((StatusCode::OK, Json(user))),
        None => Err((StatusCode::NOT_FOUND, "User not found".to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_user_deserializes() {
        let json_data = r#"{
            "name": "Test User",
            "email": "test@example.com",
            "password": "secure123",
            "date_of_birth": "1990-01-01",
            "pcd_status": true
        }"#;

        let parsed: CreateUser = serde_json::from_str(json_data).unwrap();
        assert_eq!(parsed.name, "Test User");
        assert_eq!(parsed.email, "test@example.com");
        assert_eq!(parsed.password, "secure123");
        assert_eq!(
            parsed.date_of_birth.unwrap(),
            NaiveDate::from_ymd_opt(1990, 1, 1).unwrap()
        );
        assert_eq!(parsed.pcd_status, Some(true));
    }

    #[test]
    fn legacy_plate_field_is_ignored_and_never_echoed() {
        let parsed: CreateUser = serde_json::from_str(
            r#"{"name":"Legacy","email":"legacy@example.com","password":"x","plate":"ABC-1234"}"#,
        )
        .unwrap();
        let response = UserResponse {
            id: Uuid::nil(),
            name: parsed.name,
            email: parsed.email,
            date_of_birth: parsed.date_of_birth,
            pcd_status: parsed.pcd_status,
            role: "user".to_string(),
            created_at: None,
        };
        assert!(!serde_json::to_string(&response).unwrap().contains("plate"));
    }

    #[test]
    fn create_user_optional_fields_can_be_omitted() {
        let json_data = r#"{
            "name": "Minimal User",
            "email": "minimal@example.com",
            "password": "password"
        }"#;

        let parsed: CreateUser = serde_json::from_str(json_data).unwrap();
        assert_eq!(parsed.name, "Minimal User");
        assert!(parsed.date_of_birth.is_none());
        assert!(parsed.pcd_status.is_none());
    }

    #[test]
    fn user_response_serializes() {
        let response = UserResponse {
            id: Uuid::nil(),
            name: "Admin".to_string(),
            email: "admin@example.com".to_string(),
            date_of_birth: None,
            pcd_status: None,
            role: "admin".to_string(),
            created_at: None,
        };

        let json_str = serde_json::to_string(&response).unwrap();
        assert!(json_str.contains(r#""name":"Admin""#));
        assert!(json_str.contains(r#""role":"admin""#));
    }
}
