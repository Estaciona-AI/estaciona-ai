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

use dashmap::DashMap;
use sqlx::PgPool;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::{RwLock, broadcast};
use uuid::Uuid;

use crate::pathfinding::ParkingGraph;
use crate::site_config::SiteConfig;

pub struct AppState {
    pub pool: PgPool,
    pub lpr_mode: crate::lpr::LprMode,
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
    pool: PgPool,
    jwt_secret: String,
    plate_pepper: String,
    site: SiteConfig,
    lpr_mode: crate::lpr::LprMode,
) -> Result<SharedState, crate::site_reconciliation::ReconcileError> {
    let mut graph = crate::site_graph::build(&site).map_err(|error| {
        crate::site_reconciliation::ReconcileError::InvalidMap(error.to_string())
    })?;
    crate::site_reconciliation::apply(&pool, &site).await?;
    let forecast = crate::edge_state::restore(&pool, &mut graph)
        .await
        .map_err(crate::site_reconciliation::ReconcileError::Database)?;
    let (tx, _) = broadcast::channel(100);

    Ok(Arc::new(AppState {
        pool,
        lpr_mode,
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
