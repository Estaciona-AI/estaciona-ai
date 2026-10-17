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
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use serde::Deserialize;
use sqlx::{Postgres, Transaction};
use tokio::time::{Duration, Instant, timeout_at};
use uuid::Uuid;

use super::store::{self, Binding, ChangeError};
use crate::{
    lpr::LprMode,
    security::{self, AuthSession},
    state::SharedState,
};

type ApiError = (StatusCode, String);

pub fn routes(mode: LprMode) -> Router<SharedState> {
    if mode == LprMode::Disabled {
        return Router::new();
    }
    Router::new()
        .route("/admin/vehicle-bindings", get(list).post(create))
        .route("/admin/vehicle-bindings/{id}/approve", post(approve))
        .route("/admin/vehicle-bindings/{id}/revoke", post(revoke))
        .layer(DefaultBodyLimit::max(16 * 1024))
}

fn unavailable(error: impl std::fmt::Display) -> ApiError {
    tracing::error!(%error, "Vehicle binding operation unavailable");
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "Vehicle binding service unavailable".into(),
    )
}
fn bad_input() -> ApiError {
    (
        StatusCode::BAD_REQUEST,
        "Invalid vehicle binding request".into(),
    )
}
fn changed(error: ChangeError) -> ApiError {
    match error {
        ChangeError::NotFound => (StatusCode::NOT_FOUND, "Vehicle binding not found".into()),
        ChangeError::Conflict => (
            StatusCode::CONFLICT,
            "Approved binding changed; review the current binding".into(),
        ),
        ChangeError::Database(error) => unavailable(error),
    }
}
fn deadline(session: &AuthSession) -> Instant {
    session
        .deadline
        .min(Instant::now() + Duration::from_secs(5))
}
async fn admin(state: &SharedState, headers: &HeaderMap) -> Result<AuthSession, ApiError> {
    let session = security::authenticated_session(headers, &state.jwt_secret, &state.pool).await?;
    if session.role != "admin" {
        return Err((StatusCode::FORBIDDEN, "Admin required".into()));
    }
    Ok(session)
}
async fn begin<'a>(
    state: &'a SharedState,
    session: &AuthSession,
) -> Result<Transaction<'a, Postgres>, ApiError> {
    let mut tx = state.pool.begin().await.map_err(unavailable)?;
    sqlx::raw_sql("SET LOCAL statement_timeout = '2s'; SET LOCAL lock_timeout = '2s'")
        .execute(&mut *tx)
        .await
        .map_err(unavailable)?;
    let role: Option<String> = sqlx::query_scalar("SELECT role FROM users WHERE id=$1 FOR SHARE")
        .bind(session.user_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(unavailable)?;
    session.check_expiration()?;
    if role.as_deref() != Some("admin") {
        return Err((StatusCode::FORBIDDEN, "Admin permissions changed".into()));
    }
    Ok(tx)
}
fn valid_reason(reason: &str) -> bool {
    !reason.trim().is_empty() && reason.len() <= 512
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Create {
    pub user_id: Uuid,
    pub plate: String,
}

pub async fn create(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(request): Json<Create>,
) -> Result<(StatusCode, Json<Binding>), ApiError> {
    let session = admin(&state, &headers).await?;
    if request.plate.is_empty() || request.plate.len() > 32 {
        return Err(bad_input());
    }
    let hash = security::hash_plate(&request.plate, &state.plate_pepper);
    timeout_at(deadline(&session), async {
        let mut tx = begin(&state, &session).await?;
        let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE id=$1)")
            .bind(request.user_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(unavailable)?;
        if !exists {
            return Err((StatusCode::NOT_FOUND, "User not found".into()));
        }
        let binding =
            store::request_binding(&mut tx, request.user_id, &hash, Some(session.user_id))
                .await
                .map_err(unavailable)?;
        session.check_expiration()?;
        tx.commit().await.map_err(unavailable)?;
        Ok((StatusCode::CREATED, Json(binding)))
    })
    .await
    .map_err(unavailable)?
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Approval {
    pub reason: String,
    pub expected_previous_binding: Option<Uuid>,
}

pub async fn approve(
    State(state): State<SharedState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Json(request): Json<Approval>,
) -> Result<Json<Binding>, ApiError> {
    let session = admin(&state, &headers).await?;
    if !valid_reason(&request.reason) {
        return Err(bad_input());
    }
    timeout_at(deadline(&session), async {
        let mut tx = begin(&state, &session).await?;
        let binding = store::approve(
            &mut tx,
            session.user_id,
            id,
            request.expected_previous_binding,
            &request.reason,
        )
        .await
        .map_err(changed)?;
        session.check_expiration()?;
        tx.commit().await.map_err(unavailable)?;
        Ok(Json(binding))
    })
    .await
    .map_err(unavailable)?
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reason {
    pub reason: String,
}

pub async fn revoke(
    State(state): State<SharedState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Json(request): Json<Reason>,
) -> Result<Json<Binding>, ApiError> {
    let session = admin(&state, &headers).await?;
    if !valid_reason(&request.reason) {
        return Err(bad_input());
    }
    timeout_at(deadline(&session), async {
        let mut tx = begin(&state, &session).await?;
        let binding = store::revoke(&mut tx, session.user_id, id, &request.reason)
            .await
            .map_err(changed)?;
        session.check_expiration()?;
        tx.commit().await.map_err(unavailable)?;
        Ok(Json(binding))
    })
    .await
    .map_err(unavailable)?
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Page {
    after: Option<Uuid>,
    limit: Option<u16>,
    status: Option<String>,
}

async fn list(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Query(page): Query<Page>,
) -> Result<Json<Vec<Binding>>, ApiError> {
    let session = admin(&state, &headers).await?;
    let limit = page.limit.unwrap_or(20);
    if !(1..=100).contains(&limit)
        || page
            .status
            .as_deref()
            .is_some_and(|s| !matches!(s, "pending" | "approved" | "revoked"))
    {
        return Err(bad_input());
    }
    timeout_at(deadline(&session), async {
        let mut tx = begin(&state, &session).await?;
        let rows = sqlx::query_as("SELECT * FROM vehicle_bindings WHERE id>$1 AND ($2::text IS NULL OR status=$2) ORDER BY id LIMIT $3")
            .bind(page.after.unwrap_or(Uuid::nil())).bind(page.status).bind(i64::from(limit)).fetch_all(&mut *tx).await.map_err(unavailable)?;
        session.check_expiration()?;
        tx.commit().await.map_err(unavailable)?;
        Ok(Json(rows))
    }).await.map_err(unavailable)?
}
