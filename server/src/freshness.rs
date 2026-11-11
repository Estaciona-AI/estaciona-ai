// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

//! A spot's physical state is only as good as its latest observation. Cameras
//! report every spot at least every 30 s; without a recent observation (a
//! camera down, or a spot never seen) the state is unknown: neither shown as
//! free nor reservable nor recommended. Reservations stay server-managed.
use std::{collections::HashSet, sync::OnceLock, time::Duration};

use tokio::sync::broadcast::{
    Receiver,
    error::{RecvError, TryRecvError},
};

use crate::{state::SharedState, ws::messages::ServerToAppMsg};

/// Four missed 30-second heartbeats.
pub const DEFAULT_STALE_AFTER_SECS: u32 = 120;
const SWEEP_INTERVAL: Duration = Duration::from_secs(15);

static STALE_AFTER_SECS: OnceLock<u32> = OnceLock::new();

/// Set once at startup from the runtime configuration.
pub fn configure(seconds: u32) {
    let _ = STALE_AFTER_SECS.set(seconds);
}

pub fn stale_after_secs() -> f64 {
    f64::from(*STALE_AFTER_SECS.get().unwrap_or(&DEFAULT_STALE_AFTER_SECS))
}

/// Active spots with the status apps see; binds the threshold as `$1`.
pub const SNAPSHOT_SQL: &str = "SELECT id, CASE WHEN status = 'reserved' THEN status \
     WHEN observation_timestamp IS NULL \
       OR observation_timestamp <= NOW() - make_interval(secs => $1) THEN 'unknown' \
     ELSE status END AS status FROM spots WHERE is_active";

const STALE_SQL: &str = "SELECT id FROM spots WHERE is_active AND status <> 'reserved' \
     AND NOT COALESCE(observation_timestamp > NOW() - make_interval(secs => $1), FALSE)";

async fn stale_spots(state: &SharedState) -> Result<HashSet<String>, sqlx::Error> {
    let ids: Vec<String> = sqlx::query_scalar(STALE_SQL)
        .bind(stale_after_secs())
        .fetch_all(state.installation.pool())
        .await?;
    Ok(ids.into_iter().collect())
}

/// Keeps connected clients at what a snapshot would show them: `announced`
/// holds the spots whose last published status is `unknown`.
pub struct Sweeper {
    updates: Receiver<String>,
    announced: HashSet<String>,
}

impl Sweeper {
    /// Spots already unknown reach clients through the snapshot, so they are
    /// not announced again.
    pub async fn starting(state: &SharedState) -> Self {
        // Subscribed before the first read: no later status goes unheard.
        let updates = state.installation.tx.subscribe();
        let announced = stale_spots(state).await.unwrap_or_default();
        Self { updates, announced }
    }

    /// A status published for a spot replaces `unknown` on connected clients,
    /// whatever its source: an observation, an administrator, a reservation.
    fn heard(&mut self, frame: &str) {
        if let Ok(ServerToAppMsg::SpotUpdate { spot_id, status }) = serde_json::from_str(frame)
            && status != "unknown"
        {
            self.announced.remove(&spot_id);
        }
    }

    /// Statuses went unheard: every quiet spot is announced again.
    fn missed(&mut self) {
        self.announced.clear();
    }

    /// Announces the spots that are unknown and not shown as such, and returns
    /// them. A spot that recovers is announced by the observation that
    /// refreshes it.
    pub async fn sweep(&mut self, state: &SharedState) -> Result<Vec<String>, sqlx::Error> {
        loop {
            match self.updates.try_recv() {
                Ok(frame) => self.heard(&frame),
                Err(TryRecvError::Lagged(_)) => self.missed(),
                Err(TryRecvError::Empty | TryRecvError::Closed) => break,
            }
        }
        let stale = stale_spots(state).await?;
        let mut newly: Vec<String> = stale.difference(&self.announced).cloned().collect();
        newly.sort();
        for spot_id in &newly {
            let update = ServerToAppMsg::SpotUpdate {
                spot_id: spot_id.clone(),
                status: "unknown".to_owned(),
            };
            if let Ok(json) = serde_json::to_string(&update) {
                let _ = state.installation.tx.send(json);
            }
        }
        self.announced = stale;
        Ok(newly)
    }

    async fn run(mut self, state: SharedState) {
        let mut interval = tokio::time::interval(SWEEP_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if let Err(error) = self.sweep(&state).await {
                        tracing::error!(%error, "Occupancy freshness sweep failed");
                    }
                }
                // Heard between sweeps too, so a busy channel never lags here.
                received = self.updates.recv() => match received {
                    Ok(frame) => self.heard(&frame),
                    Err(RecvError::Lagged(_)) => self.missed(),
                    Err(RecvError::Closed) => return,
                },
            }
        }
    }
}

/// Runs for the life of the server.
pub fn spawn_sweeper(state: SharedState) {
    tokio::spawn(async move { Sweeper::starting(&state).await.run(state).await });
}
