// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

use super::store::{self, ChangeError};
use sqlx::PgPool;
use uuid::Uuid;

async fn user(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id,name,email,password_hash) VALUES ($1,'Vehicle fixture',$2,'unused')",
    )
    .bind(id)
    .bind(format!("{id}@example.invalid"))
    .execute(pool)
    .await
    .unwrap();
    id
}
async fn request(pool: &PgPool, id: Uuid) -> store::Binding {
    let mut tx = pool.begin().await.unwrap();
    let binding = store::request_binding(&mut tx, id, "fixture-plate-hash", None)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    binding
}
async fn approved(pool: &PgPool) -> Option<Uuid> {
    let mut tx = pool.begin().await.unwrap();
    let owner = store::approved_user(&mut tx, "fixture-plate-hash")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    owner
}

#[sqlx::test(migrations = "./migrations")]
async fn declarations_are_pending_nonexclusive_and_idempotent(pool: PgPool) {
    let a = user(&pool).await;
    let b = user(&pool).await;
    let one = request(&pool, a).await;
    let two = request(&pool, b).await;
    assert_eq!(one.vehicle_id, two.vehicle_id);
    assert_eq!(one.status, "pending");
    assert_eq!(two.status, "pending");
    assert_eq!(request(&pool, a).await.id, one.id);
    assert_eq!(approved(&pool).await, None);
    let audits: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM vehicle_binding_audit")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(audits, 2);
}

#[sqlx::test(migrations = "./migrations")]
async fn approval_reassignment_revocation_and_audit_are_consistent(pool: PgPool) {
    let actor = user(&pool).await;
    let a = user(&pool).await;
    let b = user(&pool).await;
    let one = request(&pool, a).await;
    let two = request(&pool, b).await;
    let mut tx = pool.begin().await.unwrap();
    store::approve(&mut tx, actor, one.id, None, "institutional review")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(approved(&pool).await, Some(a));
    let mut tx = pool.begin().await.unwrap();
    assert!(matches!(
        store::approve(&mut tx, actor, two.id, None, "stale request").await,
        Err(ChangeError::Conflict)
    ));
    tx.rollback().await.unwrap();
    assert_eq!(approved(&pool).await, Some(a));
    let mut tx = pool.begin().await.unwrap();
    store::approve(&mut tx, actor, two.id, Some(one.id), "ownership recovery")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(approved(&pool).await, Some(b));
    let mut tx = pool.begin().await.unwrap();
    store::approve(&mut tx, actor, two.id, Some(one.id), "idempotent retry")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    store::revoke(&mut tx, actor, two.id, "access withdrawn")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(approved(&pool).await, None);
    let audits: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM vehicle_binding_audit")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(audits, 6);
}

#[sqlx::test(migrations = "./migrations")]
async fn concurrent_approvals_have_one_winner(pool: PgPool) {
    let actor = user(&pool).await;
    let one = request(&pool, user(&pool).await).await;
    let two = request(&pool, user(&pool).await).await;
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let mut tasks = Vec::new();
    for id in [one.id, two.id] {
        let pool = pool.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            let mut tx = pool.begin().await.unwrap();
            barrier.wait().await;
            match store::approve(&mut tx, actor, id, None, "concurrent approval").await {
                Ok(_) => {
                    tx.commit().await.unwrap();
                    true
                }
                Err(ChangeError::Conflict) => {
                    tx.rollback().await.unwrap();
                    false
                }
                other => panic!("unexpected approval result: {other:?}"),
            }
        }));
    }
    let mut winners = 0;
    for task in tasks {
        winners += usize::from(task.await.unwrap());
    }
    assert_eq!(winners, 1);
    let active: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM vehicle_bindings WHERE status='approved'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(active, 1);
}

async fn lock_waiter(pool: &PgPool) {
    tokio::time::timeout(std::time::Duration::from_secs(2),async {
        loop {
            let blocked:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND query LIKE '%vehicles%FOR%')")
                .fetch_one(pool).await.unwrap();
            if blocked {break}tokio::task::yield_now().await;
        }
    }).await.unwrap();
}

#[sqlx::test(migrations = "./migrations")]
async fn resolution_after_a_lock_wait_sees_the_new_approved_owner(pool: PgPool) {
    let actor = user(&pool).await;
    let a = user(&pool).await;
    let b = user(&pool).await;
    let one = request(&pool, a).await;
    let two = request(&pool, b).await;
    let mut tx = pool.begin().await.unwrap();
    store::approve(&mut tx, actor, one.id, None, "approve")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut change = pool.begin().await.unwrap();
    store::approve(&mut change, actor, two.id, Some(one.id), "transfer")
        .await
        .unwrap();
    let other = pool.clone();
    let lookup = tokio::spawn(async move { approved(&other).await });
    lock_waiter(&pool).await;
    change.commit().await.unwrap();
    assert_eq!(lookup.await.unwrap(), Some(b));
}

#[sqlx::test(migrations = "./migrations")]
async fn revocation_waits_for_the_in_flight_vehicle_lease(pool: PgPool) {
    let actor = user(&pool).await;
    let owner = user(&pool).await;
    let binding = request(&pool, owner).await;
    let mut tx = pool.begin().await.unwrap();
    store::approve(&mut tx, actor, binding.id, None, "approve")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut lease = pool.begin().await.unwrap();
    assert_eq!(
        store::approved_user(&mut lease, "fixture-plate-hash")
            .await
            .unwrap(),
        Some(owner)
    );
    let other = pool.clone();
    let revoke = tokio::spawn(async move {
        let mut tx = other.begin().await.unwrap();
        store::revoke(&mut tx, actor, binding.id, "revoke")
            .await
            .unwrap();
        tx.commit().await.unwrap();
    });
    lock_waiter(&pool).await;
    assert!(!revoke.is_finished());
    lease.commit().await.unwrap();
    revoke.await.unwrap();
    assert_eq!(approved(&pool).await, None);
}

#[sqlx::test(migrations = "./migrations")]
async fn failed_audit_rolls_back_approval(pool: PgPool) {
    let owner = user(&pool).await;
    let binding = request(&pool, owner).await;
    sqlx::raw_sql("CREATE FUNCTION reject_vehicle_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'audit unavailable'; END $$; CREATE TRIGGER reject_vehicle_audit BEFORE INSERT ON vehicle_binding_audit FOR EACH ROW EXECUTE FUNCTION reject_vehicle_audit();").execute(&pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    assert!(matches!(
        store::approve(&mut tx, owner, binding.id, None, "approve").await,
        Err(ChangeError::Database(_))
    ));
    tx.rollback().await.unwrap();
    assert_eq!(approved(&pool).await, None);
    let status: String = sqlx::query_scalar("SELECT status FROM vehicle_bindings WHERE id=$1")
        .bind(binding.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "pending");
}

#[sqlx::test(migrations = "./migrations")]
async fn legacy_declarations_upgrade_as_pending_and_downgrade_is_blocked(pool: PgPool) {
    sqlx::raw_sql("DROP TABLE vehicle_binding_audit; DROP TABLE vehicle_bindings; DROP TABLE vehicles; ALTER TABLE users ADD CONSTRAINT users_plate_key UNIQUE(plate);").execute(&pool).await.unwrap();
    let id = user(&pool).await;
    sqlx::query("UPDATE users SET plate='legacy-unverified' WHERE id=$1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!(
        "../../migrations/20261017020000_vehicle_bindings.up.sql"
    ))
    .execute(&pool)
    .await
    .unwrap();
    let status: String = sqlx::query_scalar("SELECT status FROM vehicle_bindings WHERE user_id=$1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "pending");
    assert!(
        sqlx::raw_sql(include_str!(
            "../../migrations/20261017020000_vehicle_bindings.down.sql"
        ))
        .execute(&pool)
        .await
        .is_err()
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM vehicle_bindings")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}
