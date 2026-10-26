// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

//! Local ownership registration for one dedicated database. No runtime tenant routing.
use std::{fmt, time::Duration};

use sqlx::{PgPool, Postgres, Transaction};
use tokio::time::timeout;
use uuid::Uuid;

use super::{Environment, InstallationIdentity};

const OPERATION_TIMEOUT: Duration = Duration::from_secs(10);
// Identifiers are audited constants, never derived from an input descriptor.
const BUSINESS_TABLES: &[&str] = &[
    "auth_sessions",
    "edge_camera_grants",
    "edge_camera_spots",
    "edge_event_receipts",
    "edge_forecast_state",
    "edge_gateways",
    "edge_path_state",
    "oidc_login_attempts",
    "physical_occupancy_hourly",
    "reservations",
    "site_map_revisions",
    "spots",
    "user_occupancy_history",
    "users",
    "vehicle_binding_audit",
    "vehicle_bindings",
    "vehicles",
    "vision_spot_mappings",
    "ws_tickets",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingStatus {
    Unbound,
    Matching,
    Mismatch,
}

#[derive(Debug)]
pub enum BindMode {
    EmptyDatabase,
    AdoptExisting { confirmed_database: String },
}

#[derive(Debug)]
pub enum StoreError {
    Database(sqlx::Error),
    Timeout,
    MissingSchema,
    Unbound,
    Migration,
    MigrationPending,
    SchemaAhead,
    IdentityMismatch,
    ExistingData,
    DatabaseConfirmationMismatch,
    UnsupportedSchema,
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Database(_) => "Installation binding storage unavailable",
            Self::Timeout => "Installation binding operation timed out",
            Self::Unbound => {
                "Database is unbound; run --installation-init or --installation-adopt explicitly"
            }
            Self::Migration => "Installation database migrations failed",
            Self::MigrationPending => {
                "Database schema is behind this release; run --migrate with the owner role"
            }
            Self::SchemaAhead => "Database schema is newer than this release",
            Self::MissingSchema => "Installation binding migration required",
            Self::IdentityMismatch => "Database belongs to a different installation identity",
            Self::ExistingData => "Database contains application data; explicit adoption required",
            Self::DatabaseConfirmationMismatch => {
                "Database confirmation does not match the connected database"
            }
            Self::UnsupportedSchema => {
                "Application table inventory does not match the supported schema"
            }
        })
    }
}
impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}
impl From<sqlx::Error> for StoreError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

fn environment_name(environment: Environment) -> &'static str {
    match environment {
        Environment::Production => "production",
        Environment::Demo => "demo",
        Environment::Test => "test",
    }
}

async fn begin(pool: &PgPool) -> Result<Transaction<'_, Postgres>, StoreError> {
    let mut tx = pool.begin().await?;
    sqlx::raw_sql("SET LOCAL lock_timeout='2s'; SET LOCAL statement_timeout='5s'")
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

async fn schema_exists(tx: &mut Transaction<'_, Postgres>) -> Result<bool, StoreError> {
    Ok(
        sqlx::query_scalar("SELECT to_regclass('public.installation_binding') IS NOT NULL")
            .fetch_one(&mut **tx)
            .await?,
    )
}

async fn status(
    tx: &mut Transaction<'_, Postgres>,
    identity: &InstallationIdentity,
) -> Result<BindingStatus, StoreError> {
    let stored: Option<(Uuid,Uuid,String)>=sqlx::query_as("SELECT organization_id,installation_id,environment FROM public.installation_binding WHERE singleton")
        .fetch_optional(&mut **tx).await?;
    Ok(match stored {
        None => BindingStatus::Unbound,
        Some((organization, installation, environment))
            if organization == identity.organization_id()
                && installation == identity.installation_id()
                && environment == environment_name(identity.environment()) =>
        {
            BindingStatus::Matching
        }
        Some(_) => BindingStatus::Mismatch,
    })
}

/// Read-only inspection also works before the binding migration exists.
pub async fn check(
    pool: &PgPool,
    identity: &InstallationIdentity,
) -> Result<BindingStatus, StoreError> {
    timeout(OPERATION_TIMEOUT, async {
        let mut tx = begin(pool).await?;
        sqlx::query("SET TRANSACTION READ ONLY")
            .execute(&mut *tx)
            .await?;
        let result = if schema_exists(&mut tx).await? {
            status(&mut tx, identity).await?
        } else {
            BindingStatus::Unbound
        };
        tx.rollback().await?;
        Ok(result)
    })
    .await
    .map_err(|_| StoreError::Timeout)?
}

async fn verify_inventory(tx: &mut Transaction<'_, Postgres>) -> Result<(), StoreError> {
    let tables:Vec<(String,String)>=sqlx::query_as("SELECT n.nspname::text,c.relname::text FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname NOT IN ('pg_catalog','information_schema') AND n.nspname !~ '^pg_' AND c.relkind IN ('r','p','f','m')")
        .fetch_all(&mut **tx).await?;
    let mut found = 0;
    for (schema, name) in tables {
        if schema != "public" {
            return Err(StoreError::UnsupportedSchema);
        }
        if name == "installation_binding" || name == "_sqlx_migrations" {
            continue;
        }
        if !BUSINESS_TABLES.contains(&name.as_str()) {
            return Err(StoreError::UnsupportedSchema);
        }
        found += 1;
    }
    if found != BUSINESS_TABLES.len() {
        return Err(StoreError::UnsupportedSchema);
    }
    Ok(())
}

/// Binds explicitly after migrations; never migrates, infers ownership or rebinds.
/// Provisioning assumes maintenance mode with no concurrent application DDL.
pub async fn bind(
    pool: &PgPool,
    identity: &InstallationIdentity,
    mode: BindMode,
) -> Result<(), StoreError> {
    timeout(OPERATION_TIMEOUT, async {
        let mut tx = begin(pool).await?;
        if !schema_exists(&mut tx).await? {
            return Err(StoreError::MissingSchema);
        }
        sqlx::query("LOCK TABLE public.installation_binding IN EXCLUSIVE MODE")
            .execute(&mut *tx)
            .await?;
        if let BindMode::AdoptExisting { confirmed_database } = &mode {
            let actual: String = sqlx::query_scalar("SELECT pg_catalog.current_database()::text")
                .fetch_one(&mut *tx)
                .await?;
            if &actual != confirmed_database {
                return Err(StoreError::DatabaseConfirmationMismatch);
            }
        }
        match status(&mut tx, identity).await? {
            BindingStatus::Matching => {
                tx.rollback().await?;
                return Ok(());
            }
            BindingStatus::Mismatch => return Err(StoreError::IdentityMismatch),
            BindingStatus::Unbound => {}
        }
        verify_inventory(&mut tx).await?;
        // SHARE blocks legacy INSERT/UPDATE/DELETE writers until registration commits.
        let mut locks = sqlx::QueryBuilder::<Postgres>::new("LOCK TABLE ");
        for (index, table) in BUSINESS_TABLES.iter().enumerate() {
            if index != 0 {
                locks.push(",");
            }
            locks.push("public.").push(*table);
        }
        locks.push(" IN SHARE MODE").build().execute(&mut *tx).await?;
        verify_inventory(&mut tx).await?;
        if matches!(mode, BindMode::EmptyDatabase) {
            let mut query = sqlx::QueryBuilder::<Postgres>::new("SELECT ");
            for (index, table) in BUSINESS_TABLES.iter().enumerate() {
                if index != 0 {
                    query.push(" OR ");
                }
                query.push("EXISTS(SELECT 1 FROM public.").push(*table).push(")");
            }
            let occupied: bool = query.build_query_scalar().fetch_one(&mut *tx).await?;
            if occupied {
                return Err(StoreError::ExistingData);
            }
        }
        sqlx::query(
            "INSERT INTO public.installation_binding (organization_id,installation_id,environment) VALUES ($1,$2,$3)",
        )
        .bind(identity.organization_id())
        .bind(identity.installation_id())
        .bind(environment_name(identity.environment()))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    })
    .await
    .map_err(|_| StoreError::Timeout)?
}

/// Conservative inspection before migrations, including supported historical tables.
/// Maintenance mode is required: this does not coordinate with arbitrary legacy DDL.
pub async fn preflight(
    pool: &PgPool,
    identity: &InstallationIdentity,
    mode: &BindMode,
) -> Result<(), StoreError> {
    timeout(OPERATION_TIMEOUT, async {
        let mut tx = begin(pool).await?;
        sqlx::query("SET TRANSACTION READ ONLY").execute(&mut *tx).await?;
        if let BindMode::AdoptExisting { confirmed_database } = mode {
            let actual: String = sqlx::query_scalar("SELECT current_database()::text")
                .fetch_one(&mut *tx).await?;
            if actual != *confirmed_database {
                return Err(StoreError::DatabaseConfirmationMismatch);
            }
        }
        let bound = if schema_exists(&mut tx).await? {
            status(&mut tx, identity).await?
        } else {
            BindingStatus::Unbound
        };
        if bound == BindingStatus::Mismatch {
            return Err(StoreError::IdentityMismatch);
        }
        let tables: Vec<(String, String)> = sqlx::query_as(
            "SELECT n.nspname::text,c.relname::text FROM pg_catalog.pg_class c
             JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace
             WHERE n.nspname NOT IN ('pg_catalog','information_schema')
             AND n.nspname !~ '^pg_' AND c.relkind IN ('r','p','f','m')",
        ).fetch_all(&mut *tx).await?;
        for (schema, name) in tables {
            if schema != "public" { return Err(StoreError::UnsupportedSchema); }
            if matches!(name.as_str(), "_sqlx_migrations" | "installation_binding") { continue; }
            // Only these audited literals can become SQL identifiers.
            let table = BUSINESS_TABLES.iter().copied().chain(["dashboard_admins"])
                .find(|candidate| *candidate == name).ok_or(StoreError::UnsupportedSchema)?;
            let mut query = sqlx::QueryBuilder::<Postgres>::new("SELECT EXISTS(SELECT 1 FROM public.");
            query.push(table).push(")");
            let populated: bool = query.build_query_scalar().fetch_one(&mut *tx).await?;
            if !populated { continue; }
            if matches!(mode, BindMode::EmptyDatabase) && bound == BindingStatus::Unbound {
                return Err(StoreError::ExistingData);
            }
            // Historical destructive migrations need separate operator reconciliation.
            if table == "dashboard_admins" { return Err(StoreError::UnsupportedSchema); }
            if table == "ws_tickets" {
                let has_session: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM pg_catalog.pg_attribute WHERE attrelid='public.ws_tickets'::regclass AND attname='session_id' AND NOT attisdropped)",
                ).fetch_one(&mut *tx).await?;
                if !has_session { return Err(StoreError::UnsupportedSchema); }
            }
        }
        tx.rollback().await?;
        Ok(())
    }).await.map_err(|_| StoreError::Timeout)?
}

/// The runtime role cannot change the schema, so it only confirms that every
/// migration of this binary was applied, unchanged, and that none is unknown.
pub(crate) async fn require_migrated(pool: &PgPool) -> Result<(), StoreError> {
    let exists: bool =
        sqlx::query_scalar("SELECT to_regclass('public._sqlx_migrations') IS NOT NULL")
            .fetch_one(pool)
            .await?;
    if !exists {
        return Err(StoreError::MissingSchema);
    }
    let applied: Vec<(i64, Vec<u8>, bool)> =
        sqlx::query_as("SELECT version, checksum, success FROM public._sqlx_migrations")
            .fetch_all(pool)
            .await?;
    let migrator = sqlx::migrate!();
    let expected: std::collections::HashMap<i64, &[u8]> = migrator
        .iter()
        .filter(|migration| migration.migration_type.is_up_migration())
        .map(|migration| (migration.version, migration.checksum.as_ref()))
        .collect();
    if applied
        .iter()
        .any(|(version, _, _)| !expected.contains_key(version))
    {
        return Err(StoreError::SchemaAhead);
    }
    let current = expected.iter().all(|(version, checksum)| {
        applied
            .iter()
            .any(|(applied, stored, success)| applied == version && stored == checksum && *success)
    });
    if current {
        Ok(())
    } else {
        Err(StoreError::MigrationPending)
    }
}

/// A cancelled migrator must not return a session advisory lock to the pool.
pub(crate) async fn migrate(pool: &PgPool) -> Result<(), StoreError> {
    let mut connection = pool.acquire().await?;
    connection.close_on_drop();
    sqlx::query("SET lock_timeout='2s'")
        .execute(&mut *connection)
        .await?;
    sqlx::query("SET statement_timeout='5s'")
        .execute(&mut *connection)
        .await?;
    sqlx::migrate!()
        .run(&mut *connection)
        .await
        .map_err(|_| StoreError::Migration)
}
