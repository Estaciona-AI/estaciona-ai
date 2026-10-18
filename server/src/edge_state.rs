// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

//! Durable projections of edge observations. PostgreSQL is authoritative;
//! in-process graph/cache publication follows commit without another await.
use sqlx::{PgPool, Postgres, Transaction};
use tokio::sync::RwLockWriteGuard;

use crate::{
    edge_auth::{self, AuthError, GlobalOperation, Principal},
    edge_delivery::Outcome,
    pathfinding::ParkingGraph,
    state::SharedState,
    ws::messages::{EdgeToServerMsg, ServerToAppMsg},
};

pub(crate) enum Prepared<'a> {
    Path {
        graph: RwLockWriteGuard<'a, ParkingGraph>,
        from: String,
        to: String,
        active: bool,
    },
    Forecast {
        json: String,
        writer: tokio::sync::MutexGuard<'a, ()>,
    },
    Terminal(Outcome, &'static str),
}

impl Prepared<'_> {
    pub(crate) fn outcome(&self) -> (Outcome, Option<String>) {
        match self {
            Self::Terminal(outcome, reason) => (*outcome, Some((*reason).into())),
            _ => (Outcome::Applied, None),
        }
    }

    pub(crate) fn publish(self, state: &SharedState) {
        match self {
            Self::Path {
                mut graph,
                from,
                to,
                active,
            } => graph.set_edge_status(&from, &to, active),
            Self::Forecast { json, writer } => {
                // The durable value is authoritative even if a previous task poisoned the cache.
                let mut cache = state
                    .last_trend_prediction
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                *cache = Some(json.clone());
                drop(cache);
                let _ = state.tx.send(json);
                drop(writer);
            }
            Self::Terminal(..) => {}
        }
    }
}

pub(crate) async fn prepare<'a>(
    tx: &mut Transaction<'_, Postgres>,
    state: &'a SharedState,
    principal: &Principal,
    message: EdgeToServerMsg,
) -> Result<Prepared<'a>, AuthError> {
    let operation = match &message {
        EdgeToServerMsg::PathUpdate { .. } => GlobalOperation::PathUpdate,
        EdgeToServerMsg::TrendPrediction { .. } => GlobalOperation::TrendPrediction,
        _ => return Ok(Prepared::Terminal(Outcome::Rejected, "unsupported_event")),
    };
    if !edge_auth::global_allowed(tx, principal, operation).await? {
        return Ok(Prepared::Terminal(Outcome::Rejected, "scope_forbidden"));
    }
    // Serialize projection writers across gateways; also orders publication at commit.
    // The transaction's existing lock/statement deadlines bound admission.
    sqlx::query("SELECT pg_advisory_xact_lock(1404280321)")
        .execute(&mut **tx)
        .await?;
    match message {
        EdgeToServerMsg::PathUpdate {
            from_node,
            to_node,
            is_active,
        } => {
            let graph = state.graph.write().await;
            if !graph.has_edge(&from_node, &to_node) {
                return Ok(Prepared::Terminal(Outcome::Rejected, "unknown_edge"));
            }
            let (from, to) = if from_node <= to_node {
                (from_node, to_node)
            } else {
                (to_node, from_node)
            };
            sqlx::query("INSERT INTO edge_path_state (map_revision,from_node,to_node,is_active) VALUES ($1,$2,$3,$4) ON CONFLICT (map_revision,from_node,to_node) DO UPDATE SET is_active=EXCLUDED.is_active")
                .bind(graph.revision()).bind(&from).bind(&to).bind(is_active).execute(&mut **tx).await?;
            Ok(Prepared::Path {
                graph,
                from,
                to,
                active: is_active,
            })
        }
        EdgeToServerMsg::TrendPrediction {
            timestamp,
            avg_stay_duration_mins,
            stay_duration_distribution,
            max_capacity,
            model_health,
            next_24h_occupancy,
        } => {
            // PostgreSQL releases its advisory lock before commit().await necessarily
            // resumes. Hold an in-process writer through cache publication as well.
            let writer = state.forecast_writer.lock().await;
            let frame = ServerToAppMsg::TrendPrediction {
                timestamp,
                avg_stay_duration_mins,
                stay_duration_distribution,
                max_capacity,
                model_health,
                next_24h_occupancy,
            };
            let json = serde_json::to_value(frame)
                .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
            sqlx::query("INSERT INTO edge_forecast_state (singleton,payload) VALUES (true,$1) ON CONFLICT (singleton) DO UPDATE SET payload=EXCLUDED.payload")
                .bind(sqlx::types::Json(&json)).execute(&mut **tx).await?;
            Ok(Prepared::Forecast {
                json: json.to_string(),
                writer,
            })
        }
        _ => unreachable!("operation discriminant was checked above"),
    }
}

pub(crate) async fn restore(
    pool: &PgPool,
    graph: &mut ParkingGraph,
) -> Result<Option<String>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL statement_timeout='2s'")
        .execute(&mut *tx)
        .await?;
    let revision = graph.revision();
    let paths: Vec<(String, String, bool)> = sqlx::query_as(
        "SELECT from_node,to_node,is_active FROM edge_path_state WHERE map_revision=$1",
    )
    .bind(&revision)
    .fetch_all(&mut *tx)
    .await?;
    let forecast: Option<sqlx::types::Json<serde_json::Value>> =
        sqlx::query_scalar("SELECT payload FROM edge_forecast_state WHERE singleton")
            .fetch_optional(&mut *tx)
            .await?;
    tx.commit().await?;
    for (from, to, active) in paths {
        graph.set_edge_status(&from, &to, active);
    }
    Ok(forecast.map(|value| value.0.to_string()))
}
