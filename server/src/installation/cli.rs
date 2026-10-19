// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

use super::{
    InstallationDescriptor, InstallationIdentity,
    store::{self, BindMode, BindingStatus, StoreError},
};
use sqlx::PgPool;
use std::{path::PathBuf, time::Duration};

#[derive(Debug, PartialEq, Eq)]
pub enum Operation {
    Check,
    Init,
    Adopt { confirmed_database: String },
}

#[derive(Debug)]
pub struct Command {
    pub manifest: PathBuf,
    pub operation: Operation,
}

pub fn parse(args: &[String]) -> Result<Option<Command>, &'static str> {
    const USAGE: &str = "Usage: --installation-{check|init} --manifest PATH; --installation-adopt --manifest PATH --confirm-database DATABASE_NAME";
    let Some(flag) = args.get(1) else {
        return Ok(None);
    };
    if !flag.starts_with("--installation-") {
        if args.iter().skip(1).any(|arg| {
            arg.starts_with("--installation-") || arg == "--manifest" || arg == "--confirm-database"
        }) {
            return Err(USAGE);
        }
        return Ok(None);
    }
    if args.len() < 4 || args[2] != "--manifest" || args[3].is_empty() || args[3].starts_with("--")
    {
        return Err(USAGE);
    }
    let operation = match flag.as_str() {
        "--installation-check" if args.len() == 4 => Operation::Check,
        "--installation-init" if args.len() == 4 => Operation::Init,
        "--installation-adopt"
            if args.len() == 6
                && args[4] == "--confirm-database"
                && !args[5].is_empty()
                && !args[5].starts_with("--") =>
        {
            Operation::Adopt {
                confirmed_database: args[5].clone(),
            }
        }
        _ => return Err(USAGE),
    };
    Ok(Some(Command {
        manifest: PathBuf::from(&args[3]),
        operation,
    }))
}

pub async fn execute(
    pool: &PgPool,
    identity: &InstallationIdentity,
    operation: &Operation,
) -> Result<BindingStatus, StoreError> {
    if matches!(operation, Operation::Check) {
        return store::check(pool, identity).await;
    }
    let mode = match operation {
        Operation::Init => BindMode::EmptyDatabase,
        Operation::Adopt { confirmed_database } => BindMode::AdoptExisting {
            confirmed_database: confirmed_database.clone(),
        },
        Operation::Check => unreachable!(),
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        store::preflight(pool, identity, &mode).await?;
        store::migrate(pool).await?;
        store::bind(pool, identity, mode).await?;
        Ok(BindingStatus::Matching)
    })
    .await
    .map_err(|_| StoreError::Timeout)?
}

/// Exit codes: matching=0, operational/unbound/mismatch=1, invalid descriptor=2.
pub async fn run(command: Command) -> i32 {
    let descriptor = match InstallationDescriptor::load(&command.manifest) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("{error}");
            return 2;
        }
    };
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL is required");
        return 1;
    };
    let pool = match sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&url)
        .await
    {
        Ok(pool) => pool,
        Err(_) => {
            eprintln!("Installation database connection failed");
            return 1;
        }
    };
    let result = execute(&pool, &descriptor.identity(), &command.operation).await;
    pool.close().await;
    match result {
        Ok(status) => {
            println!(
                "Installation binding: {status:?}; organization={}; installation={}; environment={:?}",
                descriptor.identity().organization_id(),
                descriptor.identity().installation_id(),
                descriptor.identity().environment()
            );
            i32::from(status != BindingStatus::Matching)
        }
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}
