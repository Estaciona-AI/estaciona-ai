// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

use crate::installation::{InstallationContext, InstallationPackage, VerifiedInstallation};
use std::sync::Arc;

pub struct AppState {
    pub installation: InstallationContext,
    pub auth_limits: crate::auth_limits::AuthLimits,
    pub jwt_secret: String,
    pub plate_pepper: String,
}

pub type SharedState = Arc<AppState>;

pub async fn init_state(
    installation: VerifiedInstallation,
    jwt_secret: String,
    plate_pepper: String,
    package: InstallationPackage,
    lpr_mode: crate::lpr::LprMode,
) -> Result<SharedState, crate::site_reconciliation::ReconcileError> {
    let installation = InstallationContext::load(installation, package, lpr_mode).await?;
    Ok(Arc::new(AppState {
        installation,
        auth_limits: Default::default(),
        jwt_secret,
        plate_pepper,
    }))
}
