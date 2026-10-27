// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

use crate::installation::{InstallationContext, InstallationPackage, VerifiedInstallation};
use std::sync::Arc;

pub struct AppState {
    pub installation: InstallationContext,
    pub auth_limits: crate::auth_limits::AuthLimits,
    pub jwt_secret: String,
    pub plate_pepper: String,
    pub personal_data: crate::personal_data::PersonalDataCipher,
    pub identity: crate::identity::IdentityProvider,
    pub ws_admission: crate::ws_admission::WsAdmission,
}

pub type SharedState = Arc<AppState>;

#[cfg(test)]
pub async fn init_state(
    installation: VerifiedInstallation,
    jwt_secret: String,
    plate_pepper: String,
    personal_data_keys: crate::personal_data::Keyring,
    identity: crate::identity::IdentityProvider,
    package: InstallationPackage,
    lpr_mode: crate::lpr::LprMode,
) -> Result<SharedState, crate::site_reconciliation::ReconcileError> {
    init_state_with(
        installation,
        jwt_secret,
        plate_pepper,
        personal_data_keys,
        identity,
        package,
        lpr_mode,
        Default::default(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn init_state_with(
    installation: VerifiedInstallation,
    jwt_secret: String,
    plate_pepper: String,
    personal_data_keys: crate::personal_data::Keyring,
    identity: crate::identity::IdentityProvider,
    package: InstallationPackage,
    lpr_mode: crate::lpr::LprMode,
    ws_limits: crate::ws_admission::WsLimits,
) -> Result<SharedState, crate::site_reconciliation::ReconcileError> {
    let installation = InstallationContext::load(installation, package, lpr_mode).await?;
    let personal_data = crate::personal_data::PersonalDataCipher::new(
        personal_data_keys,
        installation.identity().installation_id(),
    );
    Ok(Arc::new(AppState {
        installation,
        auth_limits: Default::default(),
        jwt_secret,
        plate_pepper,
        personal_data,
        identity,
        ws_admission: crate::ws_admission::WsAdmission::new(ws_limits),
    }))
}
