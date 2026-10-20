// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

use crate::installation::{InstallationIdentity, VerifiedInstallation};
use dashmap::DashMap;
use sqlx::PgPool;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::{RwLock, broadcast};
use uuid::Uuid;

use crate::pathfinding::ParkingGraph;
use crate::site_config::SiteConfig;

pub struct AppState {
    pub(crate) installation_binding: VerifiedInstallation,
    pub pool: PgPool,
    pub lpr_mode: crate::lpr::LprMode,
    pub reservation_policy: crate::reservation_policy::ReservationPolicy,
    pub auth_limits: crate::auth_limits::AuthLimits,
    pub tx: broadcast::Sender<String>,
    pub user_sessions: DashMap<Uuid, crate::ws::private_session::PrivateSession>,
    pub graph: RwLock<ParkingGraph>,
    pub jwt_secret: String,
    pub plate_pepper: String,
    pub last_trend_prediction: Mutex<Option<String>>,
    pub(crate) forecast_writer: tokio::sync::Mutex<()>,
    pub config_store: crate::config_store::LocalConfigStore,
}

pub type SharedState = Arc<AppState>;

pub async fn init_state(
    installation: VerifiedInstallation,
    jwt_secret: String,
    plate_pepper: String,
    site: SiteConfig,
    lpr_mode: crate::lpr::LprMode,
) -> Result<SharedState, crate::site_reconciliation::ReconcileError> {
    let pool = installation.pool().clone();
    let reservation_policy = crate::reservation_policy::ReservationPolicy::load(&site.directory)
        .map_err(|error| {
            crate::site_reconciliation::ReconcileError::InvalidMap(format!(
                "Reservation policy: {error}"
            ))
        })?;
    let mut graph = crate::site_graph::build(&site).map_err(|error| {
        crate::site_reconciliation::ReconcileError::InvalidMap(error.to_string())
    })?;
    crate::site_reconciliation::apply(&pool, &site).await?;
    let forecast = crate::edge_state::restore(&pool, &mut graph)
        .await
        .map_err(crate::site_reconciliation::ReconcileError::Database)?;
    let (tx, _) = broadcast::channel(100);

    Ok(Arc::new(AppState {
        installation_binding: installation,
        pool,
        lpr_mode,
        reservation_policy,
        auth_limits: Default::default(),
        tx,
        user_sessions: DashMap::new(),
        graph: RwLock::new(graph),
        jwt_secret,
        plate_pepper,
        forecast_writer: Default::default(),
        last_trend_prediction: Mutex::new(forecast),
        config_store: crate::config_store::LocalConfigStore::new(site.directory),
    }))
}

impl AppState {
    pub fn installation_identity(&self) -> InstallationIdentity {
        self.installation_binding.identity()
    }
}
