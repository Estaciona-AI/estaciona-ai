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
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::security::{hash_password, hash_plate};
use crate::state::SharedState;

#[derive(Deserialize)]
pub struct CreateUser {
    pub name: String,
    pub email: String,
    pub password: String,
    pub date_of_birth: Option<NaiveDate>,
    pub pcd_status: Option<bool>,
    pub plate: Option<String>,
}

#[derive(Serialize)]
pub struct UserResponse {
    pub id: Uuid,
    pub name: String,
    pub email: String,
    pub date_of_birth: Option<NaiveDate>,
    pub pcd_status: Option<bool>,
    pub plate: Option<String>,
    pub role: String,
    pub created_at: Option<DateTime<Utc>>,
}

pub async fn create_user(
    State(state): State<SharedState>,
    Json(payload): Json<CreateUser>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let new_id = Uuid::new_v4();

    let hashed_password = hash_password(&payload.password).map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Error hashing password".to_string(),
        )
    })?;

    let hashed_plate = payload
        .plate
        .as_deref()
        .map(|p| hash_plate(p, &state.plate_pepper));

    let record = sqlx::query!(
        r#"
        INSERT INTO users (id, name, email, password_hash, date_of_birth, pcd_status, plate)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        RETURNING
        id,
        name as "name!",
        email as "email!",
        date_of_birth,
        pcd_status,
        plate,
        role as "role!",
        created_at as "created_at?"
        "#,
        new_id,
        payload.name,
        payload.email,
        hashed_password,
        payload.date_of_birth,
        payload.pcd_status,
        hashed_plate,
    )
    .fetch_one(&state.pool)
    .await
    .map_err(|e| {
        tracing::error!("Database error: {}", e);
        if e.to_string().contains("duplicate key value") {
            (
                StatusCode::CONFLICT,
                "Email or Plate already registered".to_string(),
            )
        } else {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to create user".to_string(),
            )
        }
    })?;

    let response = UserResponse {
        id: record.id,
        name: record.name,
        email: record.email,
        date_of_birth: record.date_of_birth,
        pcd_status: record.pcd_status,
        plate: record.plate,
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
    let requester =
        crate::security::authenticated_session(&headers, &state.jwt_secret, &state.pool).await?;
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
        plate,
        role as "role!",
        created_at as "created_at?"
        FROM users
        WHERE id = $1
        "#,
        id
    )
    .fetch_optional(&state.pool)
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
            "pcd_status": true,
            "plate": "ABC-1234"
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
        assert_eq!(parsed.plate, Some("ABC-1234".to_string()));
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
        assert!(parsed.plate.is_none());
    }

    #[test]
    fn user_response_serializes() {
        let response = UserResponse {
            id: Uuid::nil(),
            name: "Admin".to_string(),
            email: "admin@example.com".to_string(),
            date_of_birth: None,
            pcd_status: None,
            plate: None,
            role: "admin".to_string(),
            created_at: None,
        };

        let json_str = serde_json::to_string(&response).unwrap();
        assert!(json_str.contains(r#""name":"Admin""#));
        assert!(json_str.contains(r#""role":"admin""#));
    }
}
