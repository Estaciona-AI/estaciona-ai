// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

use super::cli::{self, Command, Operation};
use super::store::{self, BindMode, BindingStatus};
use super::store_tests::identity;
use sqlx::PgPool;

fn args(values: &[&str]) -> Vec<String> {
    std::iter::once("server")
        .chain(values.iter().copied())
        .map(str::to_owned)
        .collect()
}

#[test]
fn parses_only_complete_unambiguous_installation_commands() {
    for (flag, operation) in [
        ("--installation-check", Operation::Check),
        ("--installation-init", Operation::Init),
    ] {
        let parsed = cli::parse(&args(&[flag, "--manifest", "/synthetic.json"]))
            .unwrap()
            .unwrap();
        let Command::Database {
            manifest,
            operation: actual,
        } = parsed
        else {
            panic!("expected database command");
        };
        assert_eq!(actual, operation);
        assert_eq!(manifest.to_str(), Some("/synthetic.json"));
    }
    assert!(matches!(
        cli::parse(&args(&[
            "--installation-adopt",
            "--manifest",
            "site.json",
            "--confirm-database",
            "testdb"
        ]))
        .unwrap()
        .unwrap(),
        Command::Database {
            operation: Operation::Adopt { .. },
            ..
        }
    ));
    for invalid in [
        vec!["--installation-init"],
        vec!["--installation-check", "--manifest", ""],
        vec!["--installation-adopt", "--manifest", "a"],
        vec![
            "--installation-init",
            "--manifest",
            "a",
            "--confirm-database",
            "secret",
        ],
        vec!["--installation-invalid"],
        vec!["--create-admin", "--installation-init"],
    ] {
        assert!(cli::parse(&args(&invalid)).is_err(), "{invalid:?}");
    }
    assert!(cli::parse(&args(&[])).unwrap().is_none());
    assert!(
        cli::parse(&args(&[
            "--create-admin",
            "test@example.invalid",
            "--password-file",
            "a"
        ]))
        .unwrap()
        .is_none()
    );
}

#[sqlx::test(migrations = false)]
async fn initializes_empty_database_and_repeats_safely(pool: PgPool) {
    let expected = identity(2, "test");
    assert_eq!(
        cli::execute(&pool, &expected, &Operation::Check)
            .await
            .unwrap(),
        BindingStatus::Unbound
    );
    let absent: bool = sqlx::query_scalar("SELECT to_regclass('public._sqlx_migrations') IS NULL")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(absent);
    for _ in 0..2 {
        assert_eq!(
            cli::execute(&pool, &expected, &Operation::Init)
                .await
                .unwrap(),
            BindingStatus::Matching
        );
    }
}

#[sqlx::test(migrations = false)]
async fn refuses_populated_legacy_before_any_migration(pool: PgPool) {
    sqlx::raw_sql("CREATE TABLE dashboard_admins (password_hash TEXT); INSERT INTO dashboard_admins VALUES ('preserved')").execute(&pool).await.unwrap();
    assert!(
        cli::execute(&pool, &identity(2, "test"), &Operation::Init)
            .await
            .is_err()
    );
    let hash: String = sqlx::query_scalar("SELECT password_hash FROM dashboard_admins")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(hash, "preserved");
    let absent: bool = sqlx::query_scalar("SELECT to_regclass('public._sqlx_migrations') IS NULL")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(absent);
}

#[sqlx::test(migrations = "./migrations")]
async fn mismatch_and_wrong_confirmation_do_not_apply_pending_migrations(pool: PgPool) {
    let expected = identity(2, "test");
    store::bind(&pool, &expected, BindMode::EmptyDatabase)
        .await
        .unwrap();
    sqlx::raw_sql("DROP TABLE physical_occupancy_hourly; DELETE FROM _sqlx_migrations WHERE version=20261019000000").execute(&pool).await.unwrap();
    assert!(
        cli::execute(&pool, &identity(3, "test"), &Operation::Init)
            .await
            .is_err()
    );
    assert!(
        cli::execute(
            &pool,
            &expected,
            &Operation::Adopt {
                confirmed_database: "wrong".into()
            }
        )
        .await
        .is_err()
    );
    let absent: bool =
        sqlx::query_scalar("SELECT to_regclass('public.physical_occupancy_hourly') IS NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(absent);
}

#[sqlx::test(migrations = "./migrations")]
async fn adopts_existing_data_with_confirmation_and_preserves_it(pool: PgPool) {
    sqlx::query("INSERT INTO users(id,email,password_hash) VALUES ('00000000-0000-4000-8000-000000000099','synthetic@example.invalid','preserved')").execute(&pool).await.unwrap();
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&pool)
        .await
        .unwrap();
    let operation = Operation::Adopt {
        confirmed_database: database,
    };
    for _ in 0..2 {
        assert_eq!(
            cli::execute(&pool, &identity(2, "test"), &operation)
                .await
                .unwrap(),
            BindingStatus::Matching
        );
    }
    let hash: String = sqlx::query_scalar("SELECT password_hash FROM users")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(hash, "preserved");
}

#[sqlx::test(migrations = false)]
async fn unknown_schema_is_rejected_before_migrations(pool: PgPool) {
    sqlx::query("CREATE TABLE unclassified (data TEXT)")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        cli::execute(&pool, &identity(2, "test"), &Operation::Init)
            .await
            .is_err()
    );
    let absent: bool = sqlx::query_scalar("SELECT to_regclass('public._sqlx_migrations') IS NULL")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(absent);
}

#[sqlx::test(migrations = false)]
async fn adoption_refuses_destructive_historical_migrations(pool: PgPool) {
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&pool)
        .await
        .unwrap();
    let operation = Operation::Adopt {
        confirmed_database: database,
    };
    for table in ["dashboard_admins", "ws_tickets"] {
        let mut create = sqlx::QueryBuilder::<sqlx::Postgres>::new("CREATE TABLE ");
        create
            .push(table)
            .push(" (data TEXT)")
            .build()
            .execute(&pool)
            .await
            .unwrap();
        let mut insert = sqlx::QueryBuilder::<sqlx::Postgres>::new("INSERT INTO ");
        insert
            .push(table)
            .push(" VALUES ('preserved')")
            .build()
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            cli::execute(&pool, &identity(2, "test"), &operation)
                .await
                .is_err()
        );
        let absent: bool =
            sqlx::query_scalar("SELECT to_regclass('public._sqlx_migrations') IS NULL")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(absent);
        let mut drop = sqlx::QueryBuilder::<sqlx::Postgres>::new("DROP TABLE ");
        drop.push(table).build().execute(&pool).await.unwrap();
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn migration_rechecks_identity_after_migrations(pool: PgPool) {
    let expected = identity(2, "test");
    store::bind(&pool, &expected, BindMode::EmptyDatabase)
        .await
        .unwrap();
    sqlx::raw_sql(
        "DROP TABLE physical_occupancy_hourly;
         DELETE FROM _sqlx_migrations WHERE version=20261019000000;
         CREATE FUNCTION synthetic_change_identity() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           UPDATE installation_binding SET environment='production';
           RETURN NEW;
         END $$;
         CREATE TRIGGER synthetic_change_identity AFTER INSERT ON _sqlx_migrations
         FOR EACH ROW EXECUTE FUNCTION synthetic_change_identity();",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert!(matches!(
        super::startup::migrate(&pool, &expected).await,
        Err(store::StoreError::IdentityMismatch)
    ));
    assert_eq!(
        store::check(&pool, &expected).await.unwrap(),
        BindingStatus::Mismatch
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn migration_lock_timeout_releases_connection_for_retry(pool: PgPool) {
    let expected = identity(2, "test");
    store::bind(&pool, &expected, BindMode::EmptyDatabase)
        .await
        .unwrap();
    let mut blocker = pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE _sqlx_migrations IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *blocker)
        .await
        .unwrap();
    assert!(matches!(
        super::startup::migrate(&pool, &expected).await,
        Err(store::StoreError::Migration)
    ));
    blocker.rollback().await.unwrap();
    super::startup::migrate(&pool, &expected).await.unwrap();
    assert_eq!(
        store::check(&pool, &expected).await.unwrap(),
        BindingStatus::Matching
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn startup_refuses_a_schema_behind_or_ahead_of_the_binary(pool: PgPool) {
    const VERSION: i64 = 20261019000000;
    let expected = identity(2, "test");
    store::bind(&pool, &expected, BindMode::EmptyDatabase)
        .await
        .unwrap();
    super::startup::prepare(pool.clone(), expected)
        .await
        .unwrap();
    let (description, checksum): (String, Vec<u8>) =
        sqlx::query_as("SELECT description, checksum FROM _sqlx_migrations WHERE version=$1")
            .bind(VERSION)
            .fetch_one(&pool)
            .await
            .unwrap();
    let pending = |result| matches!(result, Err(store::StoreError::MigrationPending));

    sqlx::query("UPDATE _sqlx_migrations SET checksum='\\x00' WHERE version=$1")
        .bind(VERSION)
        .execute(&pool)
        .await
        .unwrap();
    assert!(pending(
        super::startup::prepare(pool.clone(), expected).await
    ));
    sqlx::query("UPDATE _sqlx_migrations SET checksum=$2, success=false WHERE version=$1")
        .bind(VERSION)
        .bind(&checksum)
        .execute(&pool)
        .await
        .unwrap();
    assert!(pending(
        super::startup::prepare(pool.clone(), expected).await
    ));
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version=$1")
        .bind(VERSION)
        .execute(&pool)
        .await
        .unwrap();
    assert!(pending(
        super::startup::prepare(pool.clone(), expected).await
    ));
    // Startup does not repair the schema: the table of the removed version stays.
    let table: bool =
        sqlx::query_scalar("SELECT to_regclass('public.physical_occupancy_hourly') IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(table);
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
         VALUES ($1, $2, true, $3, 0), (29991231000000, 'future', true, '\\x00', 0)",
    )
    .bind(VERSION)
    .bind(&description)
    .bind(&checksum)
    .execute(&pool)
    .await
    .unwrap();
    assert!(matches!(
        super::startup::prepare(pool.clone(), expected).await,
        Err(store::StoreError::SchemaAhead)
    ));
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version=29991231000000")
        .execute(&pool)
        .await
        .unwrap();
    super::startup::prepare(pool.clone(), expected)
        .await
        .unwrap();
}

#[test]
fn offline_package_validation_has_an_explicit_complete_command() {
    assert!(matches!(
        cli::parse(&args(&[
            "--installation-validate",
            "--manifest",
            "synthetic.json"
        ])),
        Ok(Some(Command::Validate { .. }))
    ));
    for values in [
        vec!["--installation-validate"],
        vec!["--installation-validate", "--manifest", ""],
        vec![
            "--installation-validate",
            "--manifest",
            "a",
            "--confirm-database",
            "b",
        ],
    ] {
        assert!(cli::parse(&args(&values)).is_err());
    }
}
