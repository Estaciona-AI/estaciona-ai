// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct Binding {
    pub id: Uuid,
    pub vehicle_id: Uuid,
    pub user_id: Uuid,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub async fn audit(
    tx: &mut Transaction<'_, Postgres>,
    binding: &Binding,
    actor: Option<Uuid>,
    action: &str,
    reason: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO vehicle_binding_audit (binding_id,vehicle_id,subject_user_id,actor_id,action,reason) VALUES ($1,$2,$3,$4,$5,$6)")
        .bind(binding.id).bind(binding.vehicle_id).bind(binding.user_id).bind(actor).bind(action).bind(reason)
        .execute(&mut **tx).await?;
    Ok(())
}

pub async fn request_binding(
    tx: &mut Transaction<'_, Postgres>,
    user_id: Uuid,
    plate_hash: &str,
    actor: Option<Uuid>,
) -> Result<Binding, sqlx::Error> {
    let vehicle_id: Uuid = sqlx::query_scalar("INSERT INTO vehicles (plate_hash) VALUES ($1) ON CONFLICT (plate_hash) DO UPDATE SET plate_hash=EXCLUDED.plate_hash RETURNING id")
        .bind(plate_hash).fetch_one(&mut **tx).await?;
    let inserted = sqlx::query_as::<_, Binding>("INSERT INTO vehicle_bindings (vehicle_id,user_id) VALUES ($1,$2) ON CONFLICT (vehicle_id,user_id) DO NOTHING RETURNING *")
        .bind(vehicle_id).bind(user_id).fetch_optional(&mut **tx).await?;
    if let Some(binding) = inserted {
        audit(tx, &binding, actor, "requested", "declared_unverified").await?;
        Ok(binding)
    } else {
        sqlx::query_as("SELECT * FROM vehicle_bindings WHERE vehicle_id=$1 AND user_id=$2")
            .bind(vehicle_id)
            .bind(user_id)
            .fetch_one(&mut **tx)
            .await
    }
}

// The vehicle lease serializes owner resolution with approval/revocation.
// Resolve the binding in a second statement AFTER acquiring the lease so a
// statement snapshot taken before a lock wait cannot select the former owner.
pub async fn approved_user(
    tx: &mut Transaction<'_, Postgres>,
    hash: &str,
) -> Result<Option<Uuid>, sqlx::Error> {
    let vehicle: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM vehicles WHERE plate_hash=$1 FOR SHARE")
            .bind(hash)
            .fetch_optional(&mut **tx)
            .await?;
    let Some(vehicle) = vehicle else {
        return Ok(None);
    };
    sqlx::query_scalar(
        "SELECT user_id FROM vehicle_bindings WHERE vehicle_id=$1 AND status='approved'",
    )
    .bind(vehicle)
    .fetch_optional(&mut **tx)
    .await
}

#[derive(Debug)]
pub enum ChangeError {
    Database(sqlx::Error),
    NotFound,
    Conflict,
}
impl From<sqlx::Error> for ChangeError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

async fn lock_binding(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
) -> Result<Binding, ChangeError> {
    let vehicle: Uuid = sqlx::query_scalar("SELECT vehicle_id FROM vehicle_bindings WHERE id=$1")
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(ChangeError::NotFound)?;
    sqlx::query("SELECT id FROM vehicles WHERE id=$1 FOR UPDATE")
        .bind(vehicle)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(ChangeError::NotFound)?;
    sqlx::query_as("SELECT * FROM vehicle_bindings WHERE id=$1 FOR UPDATE")
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(ChangeError::NotFound)
}

pub async fn approve(
    tx: &mut Transaction<'_, Postgres>,
    actor: Uuid,
    id: Uuid,
    expected_previous: Option<Uuid>,
    reason: &str,
) -> Result<Binding, ChangeError> {
    let binding = lock_binding(tx, id).await?;
    let previous: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM vehicle_bindings WHERE vehicle_id=$1 AND status='approved'",
    )
    .bind(binding.vehicle_id)
    .fetch_optional(&mut **tx)
    .await?;
    if previous == Some(id) {
        return Ok(binding);
    }
    if previous != expected_previous {
        return Err(ChangeError::Conflict);
    }
    if let Some(previous) = previous {
        let replaced: Binding = sqlx::query_as(
            "UPDATE vehicle_bindings SET status='revoked',updated_at=NOW() WHERE id=$1 RETURNING *",
        )
        .bind(previous)
        .fetch_one(&mut **tx)
        .await?;
        audit(tx, &replaced, Some(actor), "reassigned", reason).await?;
    }
    let approved = sqlx::query_as(
        "UPDATE vehicle_bindings SET status='approved',updated_at=NOW() WHERE id=$1 RETURNING *",
    )
    .bind(id)
    .fetch_one(&mut **tx)
    .await?;
    audit(tx, &approved, Some(actor), "approved", reason).await?;
    Ok(approved)
}

pub async fn revoke(
    tx: &mut Transaction<'_, Postgres>,
    actor: Uuid,
    id: Uuid,
    reason: &str,
) -> Result<Binding, ChangeError> {
    let binding = lock_binding(tx, id).await?;
    if binding.status == "revoked" {
        return Ok(binding);
    }
    let revoked = sqlx::query_as(
        "UPDATE vehicle_bindings SET status='revoked',updated_at=NOW() WHERE id=$1 RETURNING *",
    )
    .bind(id)
    .fetch_one(&mut **tx)
    .await?;
    audit(tx, &revoked, Some(actor), "revoked", reason).await?;
    Ok(revoked)
}
