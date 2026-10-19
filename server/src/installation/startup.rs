// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

use super::{
    InstallationIdentity,
    store::{self, BindingStatus, StoreError},
};
use sqlx::PgPool;
use std::time::Duration;

/// No application writes, administrative dispatch or listener may precede this guard.
pub async fn prepare(pool: &PgPool, identity: &InstallationIdentity) -> Result<(), StoreError> {
    tokio::time::timeout(Duration::from_secs(10), async {
        require_match(pool, identity).await?;
        store::migrate(pool).await?;
        require_match(pool, identity).await
    })
    .await
    .map_err(|_| StoreError::Timeout)?
}

async fn require_match(pool: &PgPool, identity: &InstallationIdentity) -> Result<(), StoreError> {
    match store::check(pool, identity).await? {
        BindingStatus::Matching => Ok(()),
        BindingStatus::Unbound => Err(StoreError::Unbound),
        BindingStatus::Mismatch => Err(StoreError::IdentityMismatch),
    }
}
