use sqlx::PgPool;

use crate::site_config::{Coords3D, ParkingLotConfig, PathNode, SiteConfig, Spot3DDef};

fn site(spots: &[(&str, f64)]) -> SiteConfig {
    SiteConfig {
        directory: std::env::temp_dir(),
        parkings: vec![ParkingLotConfig {
            path: vec![PathNode { x: 0.0, z: 0.0 }, PathNode { x: 1.0, z: 0.0 }],
        }],
        spots: spots
            .iter()
            .map(|(id, x)| Spot3DDef {
                id: (*id).into(),
                center_3d: Coords3D {
                    x: *x,
                    y: 0.0,
                    z: 1.0,
                },
            })
            .collect(),
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn moved_spot_coordinates_follow_the_applied_map(pool: PgPool) {
    crate::state::init_state(
        pool.clone(),
        "test".into(),
        "test".into(),
        site(&[("R07-A", 2.0)]),
    )
    .await
    .unwrap();
    crate::state::init_state(
        pool.clone(),
        "test".into(),
        "test".into(),
        site(&[("R07-A", 8.0)]),
    )
    .await
    .unwrap();
    let x: f64 = sqlx::query_scalar("SELECT x FROM spots WHERE id='R07-A'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(x, 8.0, "database coordinates must follow the current graph");
}

async fn user(pool: &PgPool) -> uuid::Uuid {
    let id = uuid::Uuid::new_v4();
    sqlx::query("INSERT INTO users (id,name,email,password_hash) VALUES ($1,'R07',$2,'unused')")
        .bind(id)
        .bind(format!("{id}@example.invalid"))
        .execute(pool)
        .await
        .unwrap();
    id
}

#[sqlx::test(migrations = "./migrations")]
async fn retirement_preserves_history_status_and_can_be_reactivated(pool: PgPool) {
    let a = site(&[("R07-A", 2.0), ("R07-B", 3.0)]);
    crate::site_reconciliation::apply(&pool, &a).await.unwrap();
    let owner = user(&pool).await;
    sqlx::query(
        "UPDATE spots SET status='occupied',last_updated='2026-01-01T00:00:00Z' WHERE id='R07-A'",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO user_occupancy_history (user_id,spot_id) VALUES ($1,'R07-A')")
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
    crate::site_reconciliation::apply(&pool, &site(&[("R07-B", 9.0), ("R07-C", 4.0)]))
        .await
        .unwrap();
    let retired: (bool, String, bool) = sqlx::query_as(
        "SELECT is_active,status,last_updated='2026-01-01T00:00:00Z' FROM spots WHERE id='R07-A'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(retired, (false, "occupied".into(), true));
    let history: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM user_occupancy_history WHERE user_id=$1 AND spot_id='R07-A' AND released_at IS NULL")
        .bind(owner).fetch_one(&pool).await.unwrap();
    assert_eq!(history, 1);
    crate::site_reconciliation::apply(&pool, &a).await.unwrap();
    let active: (bool, String) =
        sqlx::query_as("SELECT is_active,status FROM spots WHERE id='R07-A'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(active, (true, "occupied".into()));
    let retired_c: bool = sqlx::query_scalar("SELECT is_active FROM spots WHERE id='R07-C'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(!retired_c);
}

#[sqlx::test(migrations = "./migrations")]
async fn removing_active_reservation_rejects_the_whole_revision(pool: PgPool) {
    crate::site_reconciliation::apply(&pool, &site(&[("R07-A", 2.0), ("R07-B", 3.0)]))
        .await
        .unwrap();
    let owner = user(&pool).await;
    let claim = crate::reservation_store::create(
        &pool,
        "R07-A",
        owner,
        chrono::Utc::now() + chrono::Duration::minutes(15),
    )
    .await
    .unwrap();
    let result =
        crate::site_reconciliation::apply(&pool, &site(&[("R07-B", 99.0), ("R07-C", 5.0)])).await;
    assert!(
        matches!(result,Err(crate::site_reconciliation::ReconcileError::ActiveReservations(ids)) if ids==["R07-A"])
    );
    let rows: Vec<(String, f64, bool)> =
        sqlx::query_as("SELECT id,x,is_active FROM spots ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        rows,
        vec![("R07-A".into(), 2.0, true), ("R07-B".into(), 3.0, true)]
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM site_map_revisions")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    let status: String = sqlx::query_scalar("SELECT status FROM reservations WHERE id=$1")
        .bind(claim.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "active");
    // Moving an allocated spot is allowed and does not replace its claim.
    crate::site_reconciliation::apply(&pool, &site(&[("R07-A", 8.0), ("R07-B", 3.0)]))
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT status FROM spots WHERE id='R07-A'")
            .fetch_one(&pool)
            .await
            .unwrap(),
        "reserved"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn identical_and_reordered_maps_reuse_revision_without_refreshing_observation(pool: PgPool) {
    let first = crate::site_reconciliation::apply(&pool, &site(&[("R07-B", 3.0), ("R07-A", 2.0)]))
        .await
        .unwrap();
    sqlx::query("UPDATE spots SET status='occupied',last_updated='2026-01-01T00:00:00Z'")
        .execute(&pool)
        .await
        .unwrap();
    let again = crate::site_reconciliation::apply(&pool, &site(&[("R07-A", 2.0), ("R07-B", 3.0)]))
        .await
        .unwrap();
    assert_eq!(again, first);
    assert_eq!(sqlx::query_scalar::<_,i64>("SELECT COUNT(*) FROM spots WHERE status='occupied' AND last_updated='2026-01-01T00:00:00Z'").fetch_one(&pool).await.unwrap(),2);
    let mut changed = site(&[("R07-A", 2.0), ("R07-B", 3.0)]);
    changed.parkings[0].path[1].x = 4.0;
    let next = crate::site_reconciliation::apply(&pool, &changed)
        .await
        .unwrap();
    assert!(next > first);
}

#[sqlx::test(migrations = "./migrations")]
async fn failed_revision_commit_rolls_back_coordinates_retirement_and_new_spots(pool: PgPool) {
    crate::site_reconciliation::apply(&pool, &site(&[("R07-A", 2.0), ("R07-B", 3.0)]))
        .await
        .unwrap();
    sqlx::raw_sql("CREATE FUNCTION fail_revision() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'revision failure'; END $$; CREATE CONSTRAINT TRIGGER fail_revision AFTER INSERT ON site_map_revisions DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION fail_revision();").execute(&pool).await.unwrap();
    let result =
        crate::site_reconciliation::apply(&pool, &site(&[("R07-B", 99.0), ("R07-C", 5.0)])).await;
    assert!(matches!(
        result,
        Err(crate::site_reconciliation::ReconcileError::Database(_))
    ));
    let rows: Vec<(String, f64, bool)> =
        sqlx::query_as("SELECT id,x,is_active FROM spots ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        rows,
        vec![("R07-A".into(), 2.0, true), ("R07-B".into(), 3.0, true)]
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM site_map_revisions")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn retired_spot_cannot_be_claimed_even_if_it_is_free(pool: PgPool) {
    crate::site_reconciliation::apply(&pool, &site(&[("R07-A", 2.0)]))
        .await
        .unwrap();
    crate::site_reconciliation::apply(&pool, &site(&[]))
        .await
        .unwrap();
    let owner = user(&pool).await;
    assert!(matches!(
        crate::reservation_store::create(
            &pool,
            "R07-A",
            owner,
            chrono::Utc::now() + chrono::Duration::minutes(15)
        )
        .await,
        Err(crate::reservation_store::CreateError::Unavailable)
    ));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM reservations")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn invalid_map_does_not_change_current_revision(pool: PgPool) {
    crate::site_reconciliation::apply(&pool, &site(&[("R07-A", 2.0)]))
        .await
        .unwrap();
    for invalid in [
        site(&[("R07-A", 5.0), ("R07-A", 9.0)]),
        site(&[("R07-A", f64::NAN)]),
        site(&[("", 2.0)]),
    ] {
        assert!(matches!(
            crate::site_reconciliation::apply(&pool, &invalid).await,
            Err(crate::site_reconciliation::ReconcileError::InvalidMap(_))
        ));
    }
    assert_eq!(
        sqlx::query_scalar::<_, f64>("SELECT x FROM spots WHERE id='R07-A'")
            .fetch_one(&pool)
            .await
            .unwrap(),
        2.0
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn reconciliation_waits_for_in_flight_reservation_then_refuses_retirement(pool: PgPool) {
    crate::site_reconciliation::apply(&pool, &site(&[("R07-A", 2.0)]))
        .await
        .unwrap();
    let owner = user(&pool).await;
    let mut tx = pool.begin().await.unwrap();
    crate::reservation_store::create_in_transaction(
        &mut tx,
        "R07-A",
        owner,
        chrono::Utc::now() + chrono::Duration::minutes(15),
    )
    .await
    .unwrap();
    let worker_pool = pool.clone();
    let worker =
        tokio::spawn(
            async move { crate::site_reconciliation::apply(&worker_pool, &site(&[])).await },
        );
    tokio::time::timeout(std::time::Duration::from_secs(2),async {
        loop {
            let blocked: bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND query LIKE 'LOCK TABLE spots%')").fetch_one(&pool).await.unwrap();
            if blocked {break;}
            tokio::task::yield_now().await;
        }
    }).await.expect("reconciliation must wait for reservation transaction");
    assert!(!worker.is_finished());
    tx.commit().await.unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(3), worker)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        result,
        Err(crate::site_reconciliation::ReconcileError::ActiveReservations(_))
    ));
}

#[sqlx::test(migrations = "./migrations")]
async fn downgrade_refuses_retired_spots_and_upgrade_preserves_legacy_rows(pool: PgPool) {
    let original = site(&[("R07-A", 2.0)]);
    crate::site_reconciliation::apply(&pool, &original)
        .await
        .unwrap();
    crate::site_reconciliation::apply(&pool, &site(&[]))
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    assert!(
        sqlx::raw_sql(include_str!(
            "../migrations/20261017010000_site_reconciliation.down.sql"
        ))
        .execute(&mut *tx)
        .await
        .is_err()
    );
    tx.rollback().await.unwrap();
    assert!(
        !sqlx::query_scalar::<_, bool>("SELECT is_active FROM spots WHERE id='R07-A'")
            .fetch_one(&pool)
            .await
            .unwrap()
    );
    crate::site_reconciliation::apply(&pool, &original)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!(
        "../migrations/20261017010000_site_reconciliation.down.sql"
    ))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::raw_sql(include_str!(
        "../migrations/20261017010000_site_reconciliation.up.sql"
    ))
    .execute(&pool)
    .await
    .unwrap();
    let row: (f64, bool) = sqlx::query_as("SELECT x,is_active FROM spots WHERE id='R07-A'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row, (2.0, true));
}
