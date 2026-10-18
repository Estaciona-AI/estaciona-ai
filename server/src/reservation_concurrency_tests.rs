use std::{sync::Arc, time::Duration};

use sqlx::{PgPool, postgres::PgPoolOptions};
use tokio::{sync::Barrier, task::JoinHandle, time::timeout};
use uuid::Uuid;

use crate::reservation_store::{self, CreateError, FinishReason};

const WAIT_LIMIT: Duration = Duration::from_secs(10);

struct Fixture {
    spot_id: String,
    owner: Uuid,
    other: Uuid,
}

impl Fixture {
    async fn new(pool: &PgPool) -> Result<Self, sqlx::Error> {
        let owner = Uuid::new_v4();
        let other = Uuid::new_v4();
        for id in [owner, other] {
            sqlx::query(
                "INSERT INTO users (id, name, email, password_hash) \
                 VALUES ($1, 'Concurrency Fixture', $2, 'unused-test-hash')",
            )
            .bind(id)
            .bind(format!("{id}@example.invalid"))
            .execute(pool)
            .await?;
        }
        let spot_id = format!("R01-{}", Uuid::new_v4());
        sqlx::query("INSERT INTO spots (id, status, x, y) VALUES ($1, 'free', 0, 0)")
            .bind(&spot_id)
            .execute(pool)
            .await?;
        Ok(Self {
            spot_id,
            owner,
            other,
        })
    }

    async fn assert_active_claim(&self, pool: &PgPool, id: Uuid) -> Result<(), sqlx::Error> {
        let claim: (String, String, i64) = sqlx::query_as(
            "SELECT s.status, r.status, \
                 (SELECT COUNT(*) FROM reservations WHERE spot_id = s.id AND status = 'active') \
             FROM spots s JOIN reservations r ON r.spot_id = s.id \
             WHERE s.id = $1 AND r.id = $2",
        )
        .bind(&self.spot_id)
        .bind(id)
        .fetch_one(pool)
        .await?;
        assert_eq!(claim, ("reserved".to_owned(), "active".to_owned(), 1));
        Ok(())
    }
}

async fn worker_pool(pool: &PgPool) -> Result<(PgPool, i32), sqlx::Error> {
    let worker = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(pool.connect_options().as_ref().clone())
        .await?;
    let pid = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&worker)
        .await?;
    Ok((worker, pid))
}

async fn wait_until_blocked(
    observer: &PgPool,
    worker_pid: i32,
    blocker_pid: i32,
) -> Result<(), sqlx::Error> {
    wait_until_blocked_by_any(observer, worker_pid, &[blocker_pid]).await
}

async fn wait_until_blocked_by_any(
    observer: &PgPool,
    worker_pid: i32,
    blocker_pids: &[i32],
) -> Result<(), sqlx::Error> {
    timeout(WAIT_LIMIT, async {
        loop {
            let blockers: Vec<i32> = sqlx::query_scalar("SELECT pg_blocking_pids($1)")
                .bind(worker_pid)
                .fetch_one(observer)
                .await?;
            if blockers.iter().any(|pid| blocker_pids.contains(pid)) {
                return Ok(());
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the worker must reach the expected PostgreSQL lock")
}

async fn completed<T>(mut task: JoinHandle<T>) -> T {
    match timeout(WAIT_LIMIT, &mut task).await {
        Ok(result) => result.expect("reservation worker must not panic"),
        Err(_) => {
            task.abort();
            let _ = task.await;
            panic!("reservation worker did not finish after its blocker was released");
        }
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn stale_terminal_operations_preserve_the_next_active_reservation(
    pool: PgPool,
) -> Result<(), sqlx::Error> {
    let fixture = Fixture::new(&pool).await?;
    let first = reservation_store::create(
        &pool,
        &fixture.spot_id,
        fixture.owner,
        &crate::reservation_policy::ReservationPolicy::default(),
    )
    .await
    .expect("first reservation must be created");
    assert!(
        reservation_store::finish(
            &pool,
            first.id,
            FinishReason::Cancel {
                user_id: fixture.owner,
            },
        )
        .await?
        .is_some()
    );
    let next = reservation_store::create(
        &pool,
        &fixture.spot_id,
        fixture.other,
        &crate::reservation_policy::ReservationPolicy::default(),
    )
    .await
    .expect("the released spot must accept its next reservation");

    for reason in [
        FinishReason::Cancel {
            user_id: fixture.owner,
        },
        FinishReason::Expired,
        FinishReason::UnconfirmedOccupancy,
    ] {
        assert!(
            reservation_store::finish(&pool, first.id, reason)
                .await?
                .is_none(),
            "a terminal reservation must not transition again: {reason:?}",
        );
        fixture.assert_active_claim(&pool, next.id).await?;
    }
    let first_status: String = sqlx::query_scalar("SELECT status FROM reservations WHERE id = $1")
        .bind(first.id)
        .fetch_one(&pool)
        .await?;
    assert_eq!(first_status, "cancelled");
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn expiration_rechecks_an_extension_after_waiting_for_the_spot_lock(
    pool: PgPool,
) -> Result<(), sqlx::Error> {
    let fixture = Fixture::new(&pool).await?;
    let reservation = reservation_store::create(
        &pool,
        &fixture.spot_id,
        fixture.owner,
        &crate::reservation_policy::ReservationPolicy::default(),
    )
    .await
    .expect("expired fixture reservation must be created");
    sqlx::query("UPDATE reservations SET expires_at=NOW()-INTERVAL '1 hour' WHERE id=$1")
        .bind(reservation.id)
        .execute(&pool)
        .await?;
    let (worker, worker_pid) = worker_pool(&pool).await?;
    let mut coordinator = pool.begin().await?;
    let coordinator_pid = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *coordinator)
        .await?;
    sqlx::query("SELECT id FROM spots WHERE id = $1 FOR NO KEY UPDATE")
        .bind(&fixture.spot_id)
        .fetch_one(&mut *coordinator)
        .await?;

    let worker_clone = worker.clone();
    let expiration = tokio::spawn(async move {
        reservation_store::finish(&worker_clone, reservation.id, FinishReason::Expired).await
    });
    wait_until_blocked(&pool, worker_pid, coordinator_pid).await?;
    sqlx::query("UPDATE reservations SET expires_at = NOW() + INTERVAL '1 hour' WHERE id = $1")
        .bind(reservation.id)
        .execute(&pool)
        .await?;
    coordinator.commit().await?;

    assert!(completed(expiration).await?.is_none());
    fixture.assert_active_claim(&pool, reservation.id).await?;
    worker.close().await;
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn concurrent_creators_have_one_winner_without_a_partial_spot_update(
    pool: PgPool,
) -> Result<(), sqlx::Error> {
    let fixture = Fixture::new(&pool).await?;
    let (first_worker, first_pid) = worker_pool(&pool).await?;
    let (second_worker, second_pid) = worker_pool(&pool).await?;
    let mut coordinator = pool.begin().await?;
    let coordinator_pid = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *coordinator)
        .await?;
    sqlx::query("SELECT id FROM spots WHERE id = $1 FOR NO KEY UPDATE")
        .bind(&fixture.spot_id)
        .fetch_one(&mut *coordinator)
        .await?;

    let barrier = Arc::new(Barrier::new(2));
    let spawn_creator = |worker: PgPool, user_id: Uuid| {
        let barrier = barrier.clone();
        let spot_id = fixture.spot_id.clone();
        tokio::spawn(async move {
            barrier.wait().await;
            reservation_store::create(
                &worker,
                &spot_id,
                user_id,
                &crate::reservation_policy::ReservationPolicy::default(),
            )
            .await
        })
    };
    let first = spawn_creator(first_worker.clone(), fixture.owner);
    let second = spawn_creator(second_worker.clone(), fixture.other);
    // A queued row writer may wait for its peer's tuple lock instead of the holder.
    wait_until_blocked_by_any(&pool, first_pid, &[coordinator_pid, second_pid]).await?;
    wait_until_blocked_by_any(&pool, second_pid, &[coordinator_pid, first_pid]).await?;
    coordinator.commit().await?;

    let results = [completed(first).await, completed(second).await];
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(CreateError::Unavailable)))
            .count(),
        1,
        "the losing creator must return an unavailable spot, not a database failure: {results:?}",
    );
    let winner = results
        .iter()
        .find_map(|result| result.as_ref().ok())
        .expect("one creator must own the spot");
    fixture.assert_active_claim(&pool, winner.id).await?;
    first_worker.close().await;
    second_worker.close().await;
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn confirmation_foreign_key_does_not_deadlock_with_creation_and_stale_cancellation(
    pool: PgPool,
) -> Result<(), sqlx::Error> {
    let fixture = Fixture::new(&pool).await?;
    let first = reservation_store::create(
        &pool,
        &fixture.spot_id,
        fixture.owner,
        &crate::reservation_policy::ReservationPolicy::default(),
    )
    .await
    .expect("first reservation must be created");
    // A sensor/admin writer can expose a free spot while its reservation is still active.
    sqlx::query("UPDATE spots SET status = 'free' WHERE id = $1")
        .bind(&fixture.spot_id)
        .execute(&pool)
        .await?;
    let (creator_pool, creator_pid) = worker_pool(&pool).await?;
    let (cancel_pool, cancel_pid) = worker_pool(&pool).await?;
    let mut confirmation = pool.begin().await?;
    let confirmation_pid = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *confirmation)
        .await?;
    sqlx::query(
        "UPDATE reservations SET status = 'completed', completed_at = NOW() \
         WHERE id = $1 AND status = 'active'",
    )
    .bind(first.id)
    .execute(&mut *confirmation)
    .await?;

    let creator_clone = creator_pool.clone();
    let next_spot = fixture.spot_id.clone();
    let next_owner = fixture.other;
    let creation = tokio::spawn(async move {
        reservation_store::create(
            &creator_clone,
            &next_spot,
            next_owner,
            &crate::reservation_policy::ReservationPolicy::default(),
        )
        .await
    });
    // The creator owns the spot and waits for confirmation's uncommitted index entry.
    wait_until_blocked(&pool, creator_pid, confirmation_pid).await?;
    let cancel_clone = cancel_pool.clone();
    let owner = fixture.owner;
    let cancellation = tokio::spawn(async move {
        reservation_store::finish(
            &cancel_clone,
            first.id,
            FinishReason::Cancel { user_id: owner },
        )
        .await
    });
    wait_until_blocked(&pool, cancel_pid, creator_pid).await?;

    // The history FK requests KEY SHARE, which must coexist with the creator's spot lock.
    timeout(
        WAIT_LIMIT,
        sqlx::query("INSERT INTO user_occupancy_history (user_id, spot_id) VALUES ($1, $2)")
            .bind(fixture.owner)
            .bind(&fixture.spot_id)
            .execute(&mut *confirmation),
    )
    .await
    .expect("confirmation's foreign key must not wait for the creator's spot lock")?;
    confirmation.commit().await?;

    let next = completed(creation)
        .await
        .expect("creation must succeed after the first reservation is completed");
    assert!(completed(cancellation).await?.is_none());
    fixture.assert_active_claim(&pool, next.id).await?;
    let completed_status: String =
        sqlx::query_scalar("SELECT status FROM reservations WHERE id = $1")
            .bind(first.id)
            .fetch_one(&pool)
            .await?;
    assert_eq!(completed_status, "completed");
    let history_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM user_occupancy_history WHERE spot_id = $1")
            .bind(&fixture.spot_id)
            .fetch_one(&pool)
            .await?;
    assert_eq!(history_count, 1);
    creator_pool.close().await;
    cancel_pool.close().await;
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn uniqueness_migration_rejects_legacy_duplicates_without_changing_reservations(
    pool: PgPool,
) -> Result<(), sqlx::Error> {
    let fixture = Fixture::new(&pool).await?;
    sqlx::raw_sql(include_str!(
        "../migrations/20261011000000_unique_active_reservation.down.sql"
    ))
    .execute(&pool)
    .await?;
    for owner in [fixture.owner, fixture.other] {
        sqlx::query(
            "INSERT INTO reservations (id, user_id, spot_id, status, expires_at) \
             VALUES ($1, $2, $3, 'active', NOW() + INTERVAL '1 hour')",
        )
        .bind(Uuid::new_v4())
        .bind(owner)
        .bind(&fixture.spot_id)
        .execute(&pool)
        .await?;
    }
    let before: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT id, status FROM reservations WHERE spot_id = $1 ORDER BY id")
            .bind(&fixture.spot_id)
            .fetch_all(&pool)
            .await?;
    let error = sqlx::raw_sql(include_str!(
        "../migrations/20261011000000_unique_active_reservation.up.sql"
    ))
    .execute(&pool)
    .await
    .expect_err("legacy duplicate claims must require reconciliation rather than cancellation");
    assert_eq!(
        error
            .as_database_error()
            .and_then(|database_error| database_error.code())
            .as_deref(),
        Some("23505"),
    );
    let after: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT id, status FROM reservations WHERE spot_id = $1 ORDER BY id")
            .bind(&fixture.spot_id)
            .fetch_all(&pool)
            .await?;
    assert_eq!(before.len(), 2);
    assert_eq!(after, before);
    assert!(after.iter().all(|(_, status)| status == "active"));
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn uniqueness_migration_accepts_valid_legacy_reservations(
    pool: PgPool,
) -> Result<(), sqlx::Error> {
    let fixture = Fixture::new(&pool).await?;
    sqlx::raw_sql(include_str!(
        "../migrations/20261011000000_unique_active_reservation.down.sql"
    ))
    .execute(&pool)
    .await?;
    for status in ["active", "completed", "cancelled", "expired"] {
        sqlx::query(
            "INSERT INTO reservations (user_id, spot_id, status, expires_at) \
             VALUES ($1, $2, $3, NOW() + INTERVAL '1 hour')",
        )
        .bind(fixture.owner)
        .bind(&fixture.spot_id)
        .bind(status)
        .execute(&pool)
        .await?;
    }
    sqlx::raw_sql(include_str!(
        "../migrations/20261011000000_unique_active_reservation.up.sql"
    ))
    .execute(&pool)
    .await?;
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM reservations WHERE spot_id = $1")
        .bind(&fixture.spot_id)
        .fetch_one(&pool)
        .await?;
    assert_eq!(total, 4);
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn quota_coordination_does_not_block_security_updates_while_waiting_for_a_spot(
    pool: PgPool,
) -> Result<(), sqlx::Error> {
    let fixture = Fixture::new(&pool).await?;
    let (worker, pid) = worker_pool(&pool).await?;
    let mut coordinator = pool.begin().await?;
    let coordinator_pid = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *coordinator)
        .await?;
    sqlx::query("SELECT id FROM spots WHERE id=$1 FOR NO KEY UPDATE")
        .bind(&fixture.spot_id)
        .execute(&mut *coordinator)
        .await?;
    let cloned = worker.clone();
    let spot = fixture.spot_id.clone();
    let owner = fixture.owner;
    let creation = tokio::spawn(async move {
        reservation_store::create(
            &cloned,
            &spot,
            owner,
            &crate::reservation_policy::ReservationPolicy::default(),
        )
        .await
    });
    wait_until_blocked(&pool, pid, coordinator_pid).await?;
    let updated = timeout(
        Duration::from_secs(2),
        sqlx::query("UPDATE users SET role='admin' WHERE id=$1")
            .bind(owner)
            .execute(&pool),
    )
    .await;
    coordinator.rollback().await?;
    let created = completed(creation).await;
    assert!(
        updated
            .expect("quota lock must not block security updates")
            .is_ok()
    );
    assert!(created.is_ok());
    Ok(())
}
