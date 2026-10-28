// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

use super::{
    InstallationIdentity, VerifiedInstallation,
    store::{self, BindingStatus, StoreError},
};
use sqlx::PgPool;
use std::time::Duration;

/// No application writes, administrative dispatch or listener may precede this guard.
/// The runtime role cannot migrate: a schema behind this binary stops startup.
pub async fn prepare(
    pool: PgPool,
    identity: InstallationIdentity,
) -> Result<VerifiedInstallation, StoreError> {
    tokio::time::timeout(Duration::from_secs(10), async {
        require_match(&pool, &identity).await?;
        store::require_migrated(&pool).await?;
        require_restricted_role(&pool, &identity).await?;
        Ok(VerifiedInstallation::new(pool, identity))
    })
    .await
    .map_err(|_| StoreError::Timeout)?
}

/// `--migrate`, run with the schema owner role before starting a new release.
pub async fn migrate(pool: &PgPool, identity: &InstallationIdentity) -> Result<(), StoreError> {
    // Checks and migration are each bounded on their own (see store::migrate).
    require_match(pool, identity).await?;
    store::migrate(pool).await?;
    require_match(pool, identity).await
}

/// Production runs only with the DML role of infra/app-role.sql; demo and
/// test installations may use the owner and are warned.
async fn require_restricted_role(
    pool: &PgPool,
    identity: &InstallationIdentity,
) -> Result<(), StoreError> {
    let excess = store::excess_privileges(pool).await?;
    if excess.is_empty() {
        return Ok(());
    }
    if identity.environment() == super::Environment::Production {
        tracing::error!(privileges = ?excess, "Refusing a privileged database role");
        return Err(StoreError::PrivilegedRuntimeRole);
    }
    tracing::warn!(privileges = ?excess, "Database role has more privileges than the service needs");
    Ok(())
}

async fn require_match(pool: &PgPool, identity: &InstallationIdentity) -> Result<(), StoreError> {
    match store::check(pool, identity).await? {
        BindingStatus::Matching => Ok(()),
        BindingStatus::Unbound => Err(StoreError::Unbound),
        BindingStatus::Mismatch => Err(StoreError::IdentityMismatch),
    }
}
