// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

use super::InstallationIdentity;
use sqlx::PgPool;

/// Identity paired with the exact pool checked by the installation startup guard.
#[derive(Debug)]
pub struct VerifiedInstallation {
    identity: InstallationIdentity,
    pool: PgPool,
}

impl VerifiedInstallation {
    pub(super) fn new(pool: PgPool, identity: InstallationIdentity) -> Self {
        Self { identity, pool }
    }

    pub fn identity(&self) -> InstallationIdentity {
        self.identity
    }
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}
