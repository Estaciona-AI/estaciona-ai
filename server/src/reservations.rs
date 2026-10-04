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
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::state::SharedState;
use crate::ws::messages::ServerToAppMsg;

#[derive(Deserialize)]
pub struct CreateReservation {
    pub spot_id: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Serialize)]
pub struct ReservationResponse {
    pub id: Uuid,
    pub user_id: Uuid,
    pub spot_id: String,
    pub status: String,
    pub created_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Deserialize)]
pub struct UpdateSpotStatus {
    pub status: String,
}

async fn require_reservation_owner(
    state: &SharedState,
    headers: &HeaderMap,
    reservation_id: Uuid,
) -> Result<(), (StatusCode, String)> {
    let requester = crate::security::authenticated_user(headers, &state.jwt_secret)?;
    let owner: Option<Uuid> = sqlx::query_scalar("SELECT user_id FROM reservations WHERE id = $1")
        .bind(reservation_id)
        .fetch_optional(&state.pool)
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
    crate::security::require_admin(&headers, &state.jwt_secret)?;
    if payload.status != "occupied" && payload.status != "free" && payload.status != "reserved" {
        return Err((StatusCode::BAD_REQUEST, "Invalid status".to_string()));
    }

    sqlx::query!(
        "UPDATE spots SET status = $1, last_updated = NOW() WHERE id = $2",
        payload.status,
        id
    )
    .execute(&state.pool)
    .await
    .map_err(|e| {
        tracing::error!("Error while update spot status: {}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Database error".to_string(),
        )
    })?;

    if payload.status == "free" {
        sqlx::query(
            "UPDATE user_occupancy_history SET released_at = NOW() \
             WHERE spot_id = $1 AND released_at IS NULL",
        )
        .bind(&id)
        .execute(&state.pool)
        .await
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Database error".to_string(),
            )
        })?;
    }

    let update_msg = ServerToAppMsg::SpotUpdate {
        spot_id: id,
        status: payload.status,
    };
    if let Ok(json_str) = serde_json::to_string(&update_msg) {
        let _ = state.tx.send(json_str);
    }

    Ok(StatusCode::OK)
}

pub async fn create_reservation(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(payload): Json<CreateReservation>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let user_id = crate::security::authenticated_user(&headers, &state.jwt_secret)?;
    let mut tx = state.pool.begin().await.map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Database error".to_string(),
        )
    })?;
    let updated_spot = sqlx::query!(
        "UPDATE spots SET status = 'reserved', last_updated = NOW()
        WHERE id = $1 AND status = 'free' RETURNING id",
        payload.spot_id
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Database error".to_string(),
        )
    })?;

    if updated_spot.is_none() {
        return Err((StatusCode::CONFLICT, "Spot is not free".to_string()));
    }

    let new_id = Uuid::new_v4();

    let record = sqlx::query!(
        r#"
        INSERT INTO reservations (id, user_id, spot_id, status, expires_at)
        VALUES ($1, $2, $3, 'active', $4)
        RETURNING id, user_id as "user_id!", spot_id, status, created_at as
        "created_at?", expires_at as "expires_at?", completed_at as "completed_at?"
        "#,
        new_id,
        user_id,
        payload.spot_id,
        payload.expires_at
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to create reservation".to_string(),
        )
    })?;

    tx.commit().await.map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Database error".to_string(),
        )
    })?;

    let update_msg = ServerToAppMsg::SpotUpdate {
        spot_id: payload.spot_id.clone(),
        status: "reserved".to_string(),
    };
    if let Ok(json_str) = serde_json::to_string(&update_msg) {
        let _ = state.tx.send(json_str);
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
    crate::security::require_admin(&headers, &state.jwt_secret)?;
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
    .fetch_all(&state.pool)
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
    let mut tx = state.pool.begin().await.map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Database error".to_string(),
        )
    })?;
    let result = sqlx::query!(
        r#"
        UPDATE reservations
        SET status = 'cancelled'
        WHERE id = $1 AND status = 'active'
        RETURNING spot_id
        "#,
        id
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| {
        tracing::error!("Database error: {}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to cancel reservation".to_string(),
        )
    })?;

    match result {
        Some(record) => {
            let updated_spot = sqlx::query!(
                "UPDATE spots SET status = 'free', last_updated = NOW() WHERE id = $1 AND status = 'reserved' RETURNING id",
                record.spot_id
            )
            .fetch_optional(&mut *tx)
            .await
            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Database error".to_string()))?;

            tx.commit().await.map_err(|_| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Database error".to_string(),
                )
            })?;

            if updated_spot.is_some() {
                let update_msg = ServerToAppMsg::SpotUpdate {
                    spot_id: record.spot_id.clone(),
                    status: "free".to_string(),
                };

                if let Ok(json_str) = serde_json::to_string(&update_msg) {
                    let _ = state.tx.send(json_str);
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
    require_reservation_owner(&state, &headers, id).await?;
    let mut tx = state.pool.begin().await.map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Database error".to_string(),
        )
    })?;
    let reservation = sqlx::query!(
        "UPDATE reservations SET status = 'completed', completed_at = NOW() WHERE id = $1 AND status = 'active' RETURNING user_id, spot_id",
        id
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Database error".to_string()))?;

    match reservation {
        Some(res) => {
            sqlx::query!(
                "INSERT INTO user_occupancy_history (user_id, spot_id) VALUES ($1, $2)",
                res.user_id,
                res.spot_id
            )
            .execute(&mut *tx)
            .await
            .map_err(|_| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to save occupancy".to_string(),
                )
            })?;

            sqlx::query!(
                r#"
            DELETE FROM user_occupancy_history
            WHERE id IN (
                SELECT id FROM user_occupancy_history
                WHERE user_id = $1
                ORDER BY occupied_at DESC
                OFFSET 30
            )
            "#,
                res.user_id
            )
            .execute(&mut *tx)
            .await
            .map_err(|_| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to prune occupancy history".to_string(),
                )
            })?;

            tx.commit().await.map_err(|_| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Database error".to_string(),
                )
            })?;

            Ok((
                StatusCode::OK,
                "Occupancy confirmed and history saved.".to_string(),
            ))
        }
        None => Err((
            StatusCode::NOT_FOUND,
            "Active reservation not found".to_string(),
        )),
    }
}

pub async fn extend_reservation(
    State(state): State<SharedState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    require_reservation_owner(&state, &headers, id).await?;
    let result = sqlx::query!(
        "UPDATE reservations SET expires_at = NOW() + INTERVAL '45 seconds' WHERE id = $1 AND status = 'active' RETURNING spot_id",
        id
    )
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    match result {
        Some(record) => {
            let update_msg = ServerToAppMsg::SpotUpdate {
                spot_id: record.spot_id,
                status: "reserved".to_string(),
            };
            if let Ok(json_str) = serde_json::to_string(&update_msg) {
                let _ = state.tx.send(json_str);
            }
            Ok((StatusCode::OK, "Reservation extended.".to_string()))
        }
        None => Err((
            StatusCode::NOT_FOUND,
            "Active reservation not found".to_string(),
        )),
    }
}

use axum::extract::Query;

#[derive(Deserialize)]
pub struct RecommendQuery {
    pub user_id: Uuid,
}

pub async fn recommend_spot(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(query): Query<RecommendQuery>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let requester = crate::security::authenticated_user(&headers, &state.jwt_secret)?;
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
        WHERE s.status = 'free'
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
    .fetch_optional(&state.pool)
    .await
    .unwrap_or(None);

    if let Some(priority) = priority_spot {
        let graph = state.graph.read().await;
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
    .fetch_one(&state.pool)
    .await
    .unwrap_or(Some(0))
    .unwrap_or(0);

    if history_count >= 3 {
        let favorite_spot = sqlx::query!(
            r#"
            SELECT h.spot_id, COUNT(*) as uses 
            FROM user_occupancy_history h 
            JOIN spots s ON s.id = h.spot_id
            WHERE h.user_id = $1 
              AND s.status = 'free' 
              AND s.id NOT IN ('A-01', 'A-02', 'A-03', 'A-04')
            GROUP BY h.spot_id
            ORDER BY uses DESC
            LIMIT 1
            "#,
            query.user_id
        )
        .fetch_optional(&state.pool)
        .await
        .unwrap_or(None);

        if let Some(fav) = favorite_spot {
            let graph = state.graph.read().await;
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
        WHERE status = 'free' 
          AND id NOT IN ('A-01', 'A-02', 'A-03', 'A-04')
        "#
    )
    .fetch_all(&state.pool)
    .await
    .unwrap_or_default();

    if free_spots.is_empty() {
        return Err((StatusCode::NOT_FOUND, "Estacionamento Lotado".to_string()));
    }

    let mut closest_spot_id = None;
    let mut min_cost = u32::MAX;

    {
        let graph = state.graph.read().await;
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
            let graph = state.graph.read().await;
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
        assert_eq!(parsed.expires_at.to_rfc3339(), "2026-10-10T10:00:00+00:00");
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
}
