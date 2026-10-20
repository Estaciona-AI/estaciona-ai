// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::reservation_store::{self, CreateError, FinishReason};
use crate::state::SharedState;
use crate::ws::messages::ServerToAppMsg;

#[derive(Deserialize, schemars::JsonSchema)]
pub struct CreateReservation {
    pub spot_id: String,
    /// Compatibility input only; the server determines the actual expiration.
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct ReservationResponse {
    pub id: Uuid,
    pub user_id: Uuid,
    pub spot_id: String,
    pub status: String,
    pub created_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct UpdateSpotStatus {
    #[schemars(schema_with = "crate::contracts::spot_status_schema")]
    pub status: String,
}

async fn require_reservation_owner(
    state: &SharedState,
    headers: &HeaderMap,
    reservation_id: Uuid,
) -> Result<(), (StatusCode, String)> {
    let requester =
        crate::security::authenticated_user(headers, &state.jwt_secret, state.installation.pool())
            .await?;
    let owner: Option<Uuid> = sqlx::query_scalar("SELECT user_id FROM reservations WHERE id = $1")
        .bind(reservation_id)
        .fetch_optional(state.installation.pool())
        .await
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Database error".to_string(),
            )
        })?
        .flatten();
    if owner != Some(requester) {
        return Err((StatusCode::NOT_FOUND, "Reservation not found".to_string()));
    }
    Ok(())
}

pub async fn update_spot_status(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<UpdateSpotStatus>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    crate::security::require_admin(&headers, &state.jwt_secret, state.installation.pool()).await?;
    if payload.status != "occupied" && payload.status != "free" && payload.status != "reserved" {
        return Err((StatusCode::BAD_REQUEST, "Invalid status".to_string()));
    }

    let mut tx = state.installation.pool().begin().await.map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Database error".to_owned(),
        )
    })?;
    let changed = sqlx::query!(
        "UPDATE spots SET status = $1, last_updated = NOW() WHERE id = $2 AND is_active",
        payload.status,
        id
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| {
        tracing::error!("Error while update spot status: {}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Database error".to_string(),
        )
    })?;

    if changed.rows_affected() == 0 {
        return Err((StatusCode::NOT_FOUND, "Active spot not found".into()));
    }

    if payload.status == "free" {
        sqlx::query(
            "UPDATE user_occupancy_history SET released_at = NOW() \
             WHERE spot_id = $1 AND released_at IS NULL",
        )
        .bind(&id)
        .execute(&mut *tx)
        .await
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Database error".to_string(),
            )
        })?;
    }

    tx.commit().await.map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Database error".to_owned(),
        )
    })?;

    let update_msg = ServerToAppMsg::SpotUpdate {
        spot_id: id,
        status: payload.status,
    };
    if let Ok(json_str) = serde_json::to_string(&update_msg) {
        let _ = state.installation.tx.send(json_str);
    }

    Ok(StatusCode::OK)
}

pub async fn create_reservation(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(payload): Json<CreateReservation>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let user_id =
        crate::security::authenticated_user(&headers, &state.jwt_secret, state.installation.pool())
            .await?;
    let _legacy_client_expiry = payload.expires_at;
    let record = reservation_store::create(
        state.installation.pool(),
        &payload.spot_id,
        user_id,
        &state.installation.reservation_policy,
    )
    .await
    .map_err(|error| match error {
        CreateError::UserLimit => (
            StatusCode::CONFLICT,
            "Active reservation limit reached".to_string(),
        ),
        CreateError::Unavailable => (StatusCode::CONFLICT, "Spot is not free".to_string()),
        CreateError::Database(error) => {
            tracing::error!("Failed to create reservation: {}", error);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Database error".to_string(),
            )
        }
    })?;

    let update_msg = ServerToAppMsg::SpotUpdate {
        spot_id: payload.spot_id.clone(),
        status: "reserved".to_string(),
    };
    if let Ok(json_str) = serde_json::to_string(&update_msg) {
        let _ = state.installation.tx.send(json_str);
    }

    Ok(Json(ReservationResponse {
        id: record.id,
        user_id: record.user_id,
        spot_id: record.spot_id,
        status: record.status,
        created_at: record.created_at,
        expires_at: record.expires_at,
        completed_at: record.completed_at,
    }))
}

pub async fn get_reservations(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    crate::security::require_admin(&headers, &state.jwt_secret, state.installation.pool()).await?;
    let records = sqlx::query_as!(
        ReservationResponse,
        r#"
        SELECT 
            id, 
            user_id as "user_id!", 
            spot_id, 
            status, 
            created_at as "created_at?", 
            expires_at as "expires_at?", 
            completed_at as "completed_at?"
        FROM reservations
        WHERE status = 'active'
        "#
    )
    .fetch_all(state.installation.pool())
    .await
    .map_err(|e| {
        tracing::error!("Database error: {}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to fetch reservations".to_string(),
        )
    })?;

    Ok((StatusCode::OK, Json(records)))
}

pub async fn cancel_reservation(
    State(state): State<SharedState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    require_reservation_owner(&state, &headers, id).await?;
    let user_id =
        crate::security::authenticated_user(&headers, &state.jwt_secret, state.installation.pool())
            .await?;
    let transition = reservation_store::finish(
        state.installation.pool(),
        id,
        FinishReason::Cancel { user_id },
    )
    .await
    .map_err(|error| {
        tracing::error!("Failed to cancel reservation: {}", error);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Database error".to_string(),
        )
    })?;

    match transition {
        Some(transition) => {
            if let Some(status) = transition.spot_status {
                let update_msg = ServerToAppMsg::SpotUpdate {
                    spot_id: transition.spot_id,
                    status: status.to_string(),
                };
                if let Ok(json) = serde_json::to_string(&update_msg) {
                    let _ = state.installation.tx.send(json);
                }
            }
            Ok((StatusCode::OK, "Reservation cancelled.".to_string()))
        }
        None => Err((
            StatusCode::NOT_FOUND,
            "Active reservation not found.".to_string(),
        )),
    }
}

pub async fn confirm_occupancy(
    State(state): State<SharedState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let user_id =
        crate::security::authenticated_user(&headers, &state.jwt_secret, state.installation.pool())
            .await?;
    reservation_store::confirm(state.installation.pool(), id, user_id)
        .await
        .map_err(|error| match error {
            reservation_store::ConfirmError::NotFound => (
                StatusCode::NOT_FOUND,
                "Active reservation not found".to_owned(),
            ),
            reservation_store::ConfirmError::NotReady => (
                StatusCode::CONFLICT,
                "Reservation must be unexpired and the spot observed occupied".to_owned(),
            ),
            reservation_store::ConfirmError::Database(error) => {
                tracing::error!("Failed to confirm occupancy: {}", error);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Database error".to_owned(),
                )
            }
        })?;
    Ok((
        StatusCode::OK,
        "Occupancy confirmed and history saved.".to_owned(),
    ))
}

pub async fn extend_reservation(
    State(state): State<SharedState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    require_reservation_owner(&state, &headers, id).await?;
    let user_id =
        crate::security::authenticated_user(&headers, &state.jwt_secret, state.installation.pool())
            .await?;
    let result = reservation_store::extend(
        state.installation.pool(),
        id,
        user_id,
        &state.installation.reservation_policy,
    )
    .await
    .map_err(|error| {
        tracing::error!(%error,"Failed to extend reservation");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Database error".to_owned(),
        )
    })?;
    match result {
        Some((spot_id, status)) => {
            let update_msg = ServerToAppMsg::SpotUpdate { spot_id, status };
            if let Ok(json) = serde_json::to_string(&update_msg) {
                let _ = state.installation.tx.send(json);
            }
            Ok((StatusCode::OK, "Reservation extended.".to_owned()))
        }
        None => Err((
            StatusCode::CONFLICT,
            "Reservation is expired, inactive or at its lifetime limit".to_owned(),
        )),
    }
}

#[derive(Deserialize)]
pub struct RecommendQuery {
    pub user_id: Uuid,
}

#[derive(Debug)]
struct RecommendationQueryError {
    query: &'static str,
    source: sqlx::Error,
}

impl std::fmt::Display for RecommendationQueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Recommendation temporarily unavailable")
    }
}

impl std::error::Error for RecommendationQueryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

impl RecommendationQueryError {
    fn into_http_error(self) -> (StatusCode, String) {
        tracing::error!(query = self.query, error = %self.source, "Recommendation query failed");
        (StatusCode::SERVICE_UNAVAILABLE, self.to_string())
    }
}

pub async fn recommend_spot(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(query): Query<RecommendQuery>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let requester =
        crate::security::authenticated_user(&headers, &state.jwt_secret, state.installation.pool())
            .await?;
    if requester != query.user_id {
        return Err((StatusCode::FORBIDDEN, "User access denied".to_string()));
    }
    let priority_spot = sqlx::query!(
        r#"
        WITH user_data AS (
            SELECT pcd_status, EXTRACT(YEAR FROM age(CURRENT_DATE, date_of_birth)) as age 
            FROM users WHERE id = $1
        )
        SELECT s.id 
        FROM spots s, user_data u
        WHERE s.status = 'free' AND s.is_active
          AND (
              (u.pcd_status = true AND s.id IN ('A-01', 'A-02'))
              OR 
              (u.age >= 60 AND s.id IN ('A-03', 'A-04'))
          )
        ORDER BY s.id
        LIMIT 1
        "#,
        query.user_id
    )
    .fetch_optional(state.installation.pool())
    .await
    .map_err(|source| {
        RecommendationQueryError {
            query: "priority",
            source,
        }
        .into_http_error()
    })?;

    if let Some(priority) = priority_spot {
        let graph = state.installation.graph.read().await;
        let route = graph.calculate_route("cam-01", &priority.id);
        return Ok((
            StatusCode::OK,
            Json(serde_json::json!({
                "recommended_spot": priority.id,
                "route": route
            })),
        ));
    }

    let history_count: i64 = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM user_occupancy_history WHERE user_id = $1",
        query.user_id
    )
    .fetch_one(state.installation.pool())
    .await
    .map_err(|source| {
        RecommendationQueryError {
            query: "history_count",
            source,
        }
        .into_http_error()
    })?
    .unwrap_or(0);

    if history_count >= 3 {
        let favorite_spot = sqlx::query!(
            r#"
            SELECT h.spot_id, COUNT(*) as uses 
            FROM user_occupancy_history h 
            JOIN spots s ON s.id = h.spot_id
            WHERE h.user_id = $1 
              AND s.status = 'free' AND s.is_active
              AND s.id NOT IN ('A-01', 'A-02', 'A-03', 'A-04')
            GROUP BY h.spot_id
            ORDER BY uses DESC
            LIMIT 1
            "#,
            query.user_id
        )
        .fetch_optional(state.installation.pool())
        .await
        .map_err(|source| {
            RecommendationQueryError {
                query: "favorite",
                source,
            }
            .into_http_error()
        })?;

        if let Some(fav) = favorite_spot {
            let graph = state.installation.graph.read().await;
            let route = graph.calculate_route("cam-01", &fav.spot_id);
            return Ok((
                StatusCode::OK,
                Json(serde_json::json!({
                    "recommended_spot": fav.spot_id,
                    "route": route
                })),
            ));
        }
    }

    let free_spots = sqlx::query!(
        r#"
        SELECT id 
        FROM spots 
        WHERE status = 'free' AND is_active
          AND id NOT IN ('A-01', 'A-02', 'A-03', 'A-04')
        "#
    )
    .fetch_all(state.installation.pool())
    .await
    .map_err(|source| {
        RecommendationQueryError {
            query: "free_spots",
            source,
        }
        .into_http_error()
    })?;

    if free_spots.is_empty() {
        return Err((StatusCode::NOT_FOUND, "Estacionamento Lotado".to_string()));
    }

    let mut closest_spot_id = None;
    let mut min_cost = u32::MAX;

    {
        let graph = state.installation.graph.read().await;
        for spot in &free_spots {
            if let Some(cost) = graph.calculate_cost("cam-01", &spot.id)
                && cost < min_cost
            {
                min_cost = cost;
                closest_spot_id = Some(spot.id.clone());
            }
        }
    }

    match closest_spot_id {
        Some(spot_id) => {
            let graph = state.installation.graph.read().await;
            let route = graph.calculate_route("cam-01", &spot_id);
            Ok((
                StatusCode::OK,
                Json(serde_json::json!({
                    "recommended_spot": spot_id,
                    "route": route
                })),
            ))
        }
        None => Err((StatusCode::NOT_FOUND, "Estacionamento Lotado".to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_reservation_deserializes() {
        let json_data = r#"{
            "user_id": "123e4567-e89b-12d3-a456-426614174000",
            "spot_id": "A-01",
            "expires_at": "2026-10-10T10:00:00Z"
        }"#;

        let parsed: CreateReservation = serde_json::from_str(json_data).unwrap();
        assert_eq!(parsed.spot_id, "A-01");
        assert_eq!(
            parsed.expires_at.unwrap().to_rfc3339(),
            "2026-10-10T10:00:00+00:00"
        );
    }

    #[test]
    fn update_spot_status_deserializes() {
        let json_data = r#"{"status": "occupied"}"#;
        let parsed: UpdateSpotStatus = serde_json::from_str(json_data).unwrap();
        assert_eq!(parsed.status, "occupied");
    }

    #[test]
    fn recommend_query_deserializes() {
        let json_data = r#"{"user_id": "123e4567-e89b-12d3-a456-426614174000"}"#;
        let parsed: RecommendQuery = serde_json::from_str(json_data).unwrap();
        assert_eq!(
            parsed.user_id,
            uuid::Uuid::parse_str("123e4567-e89b-12d3-a456-426614174000").unwrap()
        );
    }
    #[test]
    fn rest_reservation_does_not_require_client_expiry() {
        assert!(
            serde_json::from_value::<CreateReservation>(serde_json::json!({"spot_id":"A-01"}))
                .is_ok()
        );
    }
}
