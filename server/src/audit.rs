// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

//! Trail of administrative actions, written with the action it records.
use serde_json::Value;
use sqlx::PgExecutor;
use uuid::Uuid;

pub async fn record(
    executor: impl PgExecutor<'_>,
    actor: Uuid,
    action: &str,
    target: Option<&str>,
    details: Value,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO admin_audit_log (actor_user_id, action, target, details) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(actor)
    .bind(action)
    .bind(target)
    .bind(details)
    .execute(executor)
    .await?;
    Ok(())
}
