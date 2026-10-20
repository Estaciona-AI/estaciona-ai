// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

use super::{InstallationIdentity, VerifiedInstallation};
use crate::{pathfinding::ParkingGraph, site_config::SiteConfig};
use dashmap::DashMap;
use sqlx::PgPool;
use std::sync::Mutex;
use tokio::sync::{RwLock, broadcast};
use uuid::Uuid;

/// Resources of one dedicated installation. The database remains authoritative.
pub struct InstallationContext {
    binding: VerifiedInstallation,
    pub(crate) lpr_mode: crate::lpr::LprMode,
    pub(crate) reservation_policy: crate::reservation_policy::ReservationPolicy,
    pub(crate) tx: broadcast::Sender<String>,
    pub(crate) user_sessions: DashMap<Uuid, crate::ws::private_session::PrivateSession>,
    pub(crate) graph: RwLock<ParkingGraph>,
    pub(crate) last_trend_prediction: Mutex<Option<String>>,
    pub(crate) forecast_writer: tokio::sync::Mutex<()>,
    pub(crate) config_store: crate::config_store::LocalConfigStore,
}

impl InstallationContext {
    pub fn pool(&self) -> &PgPool {
        self.binding.pool()
    }
    pub fn identity(&self) -> InstallationIdentity {
        self.binding.identity()
    }

    pub(crate) async fn load(
        binding: VerifiedInstallation,
        site: SiteConfig,
        lpr_mode: crate::lpr::LprMode,
    ) -> Result<Self, crate::site_reconciliation::ReconcileError> {
        let reservation_policy = crate::reservation_policy::ReservationPolicy::load(
            &site.directory,
        )
        .map_err(|error| {
            crate::site_reconciliation::ReconcileError::InvalidMap(format!(
                "Reservation policy: {error}"
            ))
        })?;
        let mut graph = crate::site_graph::build(&site).map_err(|error| {
            crate::site_reconciliation::ReconcileError::InvalidMap(error.to_string())
        })?;
        crate::site_reconciliation::apply(binding.pool(), &site).await?;
        let forecast = crate::edge_state::restore(binding.pool(), &mut graph)
            .await
            .map_err(crate::site_reconciliation::ReconcileError::Database)?;
        let (tx, _) = broadcast::channel(100);
        Ok(Self {
            binding,
            lpr_mode,
            reservation_policy,
            tx,
            user_sessions: DashMap::new(),
            graph: RwLock::new(graph),
            forecast_writer: Default::default(),
            last_trend_prediction: Mutex::new(forecast),
            config_store: crate::config_store::LocalConfigStore::new(site.directory),
        })
    }

    /// Unit fixtures can intentionally bypass reconciliation or use offline pools.
    #[cfg(test)]
    pub(crate) fn for_unit_test(
        binding: VerifiedInstallation,
        graph: ParkingGraph,
        directory: std::path::PathBuf,
    ) -> Self {
        let (tx, _) = broadcast::channel(100);
        Self {
            binding,
            graph: RwLock::new(graph),
            lpr_mode: Default::default(),
            reservation_policy: Default::default(),
            tx,
            user_sessions: DashMap::new(),
            forecast_writer: Default::default(),
            last_trend_prediction: Mutex::new(None),
            config_store: crate::config_store::LocalConfigStore::new(directory),
        }
    }
}
