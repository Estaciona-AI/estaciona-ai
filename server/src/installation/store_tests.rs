// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

use super::store::{self, BindMode, BindingStatus, StoreError};
use super::{InstallationDescriptor, InstallationIdentity};
use sqlx::PgPool;
use std::fs;
use uuid::Uuid;

pub(crate) fn identity(site: u128, environment: &str) -> InstallationIdentity {
    // Synthetic coordinates only; file names are unrelated to ownership IDs.
    let path =
        std::env::temp_dir().join(format!("estaciona-binding-fixture-{}.json", Uuid::new_v4()));
    fs::write(&path,serde_json::json!({"schema_version":1,
        "organization":{"id":Uuid::from_u128(1).to_string(),"slug":"synthetic-org","name":"Synthetic organization"},
        "installation":{"id":Uuid::from_u128(site).to_string(),"slug":"synthetic-site","name":"Synthetic installation"},
        "environment":environment}).to_string()).unwrap();
    let result = InstallationDescriptor::load(&path);
    fs::remove_file(path).unwrap();
    result.unwrap().identity()
}

#[sqlx::test(migrations = "./migrations")]
async fn empty_database_can_be_bound_idempotently_without_rebinding(pool: PgPool) {
    let expected = identity(2, "test");
    assert_eq!(
        store::check(&pool, &expected).await.unwrap(),
        BindingStatus::Unbound
    );
    store::bind(&pool, &expected, BindMode::EmptyDatabase)
        .await
        .unwrap();
    assert_eq!(
        store::check(&pool, &expected).await.unwrap(),
        BindingStatus::Matching
    );
    let created: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT created_at FROM installation_binding")
            .fetch_one(&pool)
            .await
            .unwrap();
    store::bind(&pool, &expected, BindMode::EmptyDatabase)
        .await
        .unwrap();
    let unchanged: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT created_at FROM installation_binding")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(created, unchanged);
    for changed in [identity(3, "test"), identity(2, "production")] {
        assert_eq!(
            store::check(&pool, &changed).await.unwrap(),
            BindingStatus::Mismatch
        );
        assert!(matches!(
            store::bind(&pool, &changed, BindMode::EmptyDatabase).await,
            Err(StoreError::IdentityMismatch)
        ));
    }
    sqlx::query("UPDATE installation_binding SET organization_id=$1")
        .bind(Uuid::from_u128(4))
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        store::check(&pool, &expected).await.unwrap(),
        BindingStatus::Mismatch
    );
    assert!(matches!(
        store::bind(&pool, &expected, BindMode::EmptyDatabase).await,
        Err(StoreError::IdentityMismatch)
    ));
}

#[sqlx::test(migrations = "./migrations")]
async fn existing_data_requires_explicit_database_confirmation(pool: PgPool) {
    let expected = identity(2, "test");
    sqlx::query("INSERT INTO users (id,username) VALUES ('00000000-0000-4000-8000-000000000099','preserved-hash')").execute(&pool).await.unwrap();
    assert!(matches!(
        store::bind(&pool, &expected, BindMode::EmptyDatabase).await,
        Err(StoreError::ExistingData)
    ));
    assert!(matches!(
        store::bind(
            &pool,
            &expected,
            BindMode::AdoptExisting {
                confirmed_database: "wrong-database".into()
            }
        )
        .await,
        Err(StoreError::DatabaseConfirmationMismatch)
    ));
    assert_eq!(
        store::check(&pool, &expected).await.unwrap(),
        BindingStatus::Unbound
    );
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&pool)
        .await
        .unwrap();
    store::bind(
        &pool,
        &expected,
        BindMode::AdoptExisting {
            confirmed_database: database,
        },
    )
    .await
    .unwrap();
    let hash: String = sqlx::query_scalar(
        "SELECT username FROM users WHERE id='00000000-0000-4000-8000-000000000099'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(hash, "preserved-hash");
}

#[sqlx::test(migrations = "./migrations")]
async fn unknown_application_tables_are_refused_without_changes(pool: PgPool) {
    let expected = identity(2, "test");
    sqlx::query("CREATE TABLE public.unclassified_data (value TEXT)")
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        store::bind(&pool, &expected, BindMode::EmptyDatabase).await,
        Err(StoreError::UnsupportedSchema)
    ));
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM installation_binding")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn concurrent_different_identities_have_one_winner(pool: PgPool) {
    let first = identity(2, "test");
    let second = identity(3, "test");
    let (left, right) = tokio::join!(
        store::bind(&pool, &first, BindMode::EmptyDatabase),
        store::bind(&pool, &second, BindMode::EmptyDatabase)
    );
    assert_ne!(left.is_ok(), right.is_ok());
    let (winner, loser) = if left.is_ok() {
        (first, right)
    } else {
        (second, left)
    };
    assert!(matches!(loser, Err(StoreError::IdentityMismatch)));
    assert_eq!(
        store::check(&pool, &winner).await.unwrap(),
        BindingStatus::Matching
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM installation_binding")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

async fn wait_for_blocked_lock(pool: &PgPool, table: &str) {
    tokio::time::timeout(std::time::Duration::from_millis(1500),async {
        loop {
            let blocked:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_catalog.pg_locks WHERE database=(SELECT oid FROM pg_catalog.pg_database WHERE datname=current_database()) AND relation=to_regclass($1) AND NOT granted)")
                .bind(table).fetch_one(pool).await.unwrap();
            if blocked { break; }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }).await.expect("binding must actually contend with the held database lock");
}

#[sqlx::test(migrations = "./migrations")]
async fn legacy_writer_cannot_race_the_empty_database_check(pool: PgPool) {
    let expected = identity(2, "test");
    let mut writer = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO users (id,username) VALUES ('00000000-0000-4000-8000-000000000098','preserved')").execute(&mut *writer).await.unwrap();
    let worker_pool = pool.clone();
    let worker =
        tokio::spawn(
            async move { store::bind(&worker_pool, &expected, BindMode::EmptyDatabase).await },
        );
    wait_for_blocked_lock(&pool, "public.users").await;
    writer.commit().await.unwrap();
    assert!(matches!(
        worker.await.unwrap(),
        Err(StoreError::ExistingData)
    ));
    assert_eq!(
        store::check(&pool, &expected).await.unwrap(),
        BindingStatus::Unbound
    );
    let preserved: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM users WHERE id='00000000-0000-4000-8000-000000000098'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(preserved, 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn lock_timeout_does_not_bind_or_leave_locks_after_failure(pool: PgPool) {
    let expected = identity(2, "test");
    let mut blocker = pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE public.installation_binding IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *blocker)
        .await
        .unwrap();
    let worker_pool = pool.clone();
    let worker =
        tokio::spawn(
            async move { store::bind(&worker_pool, &expected, BindMode::EmptyDatabase).await },
        );
    wait_for_blocked_lock(&pool, "public.installation_binding").await;
    let error = worker.await.unwrap().unwrap_err();
    match error {
        StoreError::Database(error) => assert_eq!(
            error.as_database_error().unwrap().code().as_deref(),
            Some("55P03")
        ),
        other => panic!("expected PostgreSQL lock timeout, got {other:?}"),
    }
    blocker.rollback().await.unwrap();
    assert_eq!(
        store::check(&pool, &expected).await.unwrap(),
        BindingStatus::Unbound
    );
    store::bind(&pool, &expected, BindMode::EmptyDatabase)
        .await
        .unwrap();
}

#[sqlx::test(migrations = "./migrations")]
async fn failed_commit_preserves_unbound_state_and_can_be_retried(pool: PgPool) {
    let expected = identity(2, "test");
    sqlx::raw_sql("CREATE FUNCTION public.reject_synthetic_binding() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'synthetic commit failure'; END $$; CREATE CONSTRAINT TRIGGER reject_synthetic_binding AFTER INSERT ON installation_binding DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.reject_synthetic_binding();")
        .execute(&pool).await.unwrap();
    assert!(matches!(
        store::bind(&pool, &expected, BindMode::EmptyDatabase).await,
        Err(StoreError::Database(_))
    ));
    assert_eq!(
        store::check(&pool, &expected).await.unwrap(),
        BindingStatus::Unbound
    );
    sqlx::query("DROP TRIGGER reject_synthetic_binding ON installation_binding")
        .execute(&pool)
        .await
        .unwrap();
    store::bind(&pool, &expected, BindMode::EmptyDatabase)
        .await
        .unwrap();
}

#[sqlx::test(migrations = "./migrations")]
async fn missing_schema_is_not_created_by_inspection_or_binding(pool: PgPool) {
    sqlx::query("DROP TABLE installation_binding")
        .execute(&pool)
        .await
        .unwrap();
    let expected = identity(2, "test");
    assert_eq!(
        store::check(&pool, &expected).await.unwrap(),
        BindingStatus::Unbound
    );
    assert!(matches!(
        store::bind(&pool, &expected, BindMode::EmptyDatabase).await,
        Err(StoreError::MissingSchema)
    ));
    let exists: bool =
        sqlx::query_scalar("SELECT to_regclass('public.installation_binding') IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!exists);
}

#[sqlx::test(migrations = "./migrations")]
async fn other_application_schemas_and_missing_business_tables_are_refused(pool: PgPool) {
    let expected = identity(2, "test");
    sqlx::raw_sql(
        "CREATE SCHEMA synthetic_other; CREATE TABLE synthetic_other.application_data (value TEXT)",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert!(matches!(
        store::bind(&pool, &expected, BindMode::EmptyDatabase).await,
        Err(StoreError::UnsupportedSchema)
    ));
    sqlx::raw_sql("DROP SCHEMA synthetic_other CASCADE; DROP TABLE vision_spot_mappings")
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        store::bind(&pool, &expected, BindMode::EmptyDatabase).await,
        Err(StoreError::UnsupportedSchema)
    ));
    assert_eq!(
        store::check(&pool, &expected).await.unwrap(),
        BindingStatus::Unbound
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn adoption_confirmation_is_required_even_for_an_existing_matching_binding(pool: PgPool) {
    let expected = identity(2, "test");
    store::bind(&pool, &expected, BindMode::EmptyDatabase)
        .await
        .unwrap();
    assert!(matches!(
        store::bind(
            &pool,
            &expected,
            BindMode::AdoptExisting {
                confirmed_database: "wrong-database".into()
            }
        )
        .await,
        Err(StoreError::DatabaseConfirmationMismatch)
    ));
    assert_eq!(
        store::check(&pool, &expected).await.unwrap(),
        BindingStatus::Matching
    );
}
