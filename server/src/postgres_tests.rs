use sqlx::PgPool;
use uuid::Uuid;

#[sqlx::test(migrations = "./migrations")]
async fn migrations_persist_a_reservation_with_user_and_spot(
    pool: PgPool,
) -> Result<(), sqlx::Error> {
    let user_id = Uuid::new_v4();
    let reservation_id = Uuid::new_v4();
    let spot_id = "migration-test-spot";

    sqlx::query(
        "INSERT INTO users (id, name, email, password_hash) \
         VALUES ($1, 'Migration Fixture', 'migration@example.invalid', 'unused-test-hash')",
    )
    .bind(user_id)
    .execute(&pool)
    .await?;

    sqlx::query("INSERT INTO spots (id, status, x, y) VALUES ($1, 'free', 0, 0)")
        .bind(spot_id)
        .execute(&pool)
        .await?;

    sqlx::query(
        "INSERT INTO reservations (id, user_id, spot_id, status, expires_at) \
         VALUES ($1, $2, $3, 'active', NOW() + INTERVAL '15 minutes')",
    )
    .bind(reservation_id)
    .bind(user_id)
    .bind(spot_id)
    .execute(&pool)
    .await?;

    let persisted: (Uuid, String, String) = sqlx::query_as(
        "SELECT users.id, spots.id, reservations.status FROM reservations \
         JOIN users ON users.id = reservations.user_id \
         JOIN spots ON spots.id = reservations.spot_id \
         WHERE reservations.id = $1",
    )
    .bind(reservation_id)
    .fetch_one(&pool)
    .await?;

    assert_eq!(
        persisted,
        (user_id, spot_id.to_owned(), "active".to_owned())
    );

    Ok(())
}

async fn assert_edge_registry_exists(pool: &PgPool) -> Result<(), sqlx::Error> {
    let exists: bool = sqlx::query_scalar(
        "SELECT to_regclass('edge_gateways') IS NOT NULL \
         AND to_regclass('edge_camera_grants') IS NOT NULL \
         AND to_regclass('edge_camera_spots') IS NOT NULL",
    )
    .fetch_one(pool)
    .await?;
    assert!(
        exists,
        "edge registry migration must create all authority tables"
    );
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn edge_registry_starts_without_grants_or_global_authority(
    pool: PgPool,
) -> Result<(), sqlx::Error> {
    assert_edge_registry_exists(&pool).await?;
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO edge_gateways (id, name, credential_digest) VALUES ($1, 'Fixture', $2)",
    )
    .bind(id)
    .bind(vec![1_u8; 32])
    .execute(&pool)
    .await?;
    let authority: (bool, bool, Option<chrono::DateTime<chrono::Utc>>) = sqlx::query_as(
        "SELECT path_update, trend_prediction, revoked_at FROM edge_gateways WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(authority, (false, false, None));
    let grant_count: i64 = sqlx::query_scalar(
        "SELECT (SELECT COUNT(*) FROM edge_camera_grants) + \
         (SELECT COUNT(*) FROM edge_camera_spots)",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(grant_count, 0);
    Ok(())
}

fn assert_database_code(error: sqlx::Error, expected: &str) {
    assert_eq!(
        error.as_database_error().and_then(|error| error.code()),
        Some(std::borrow::Cow::Borrowed(expected))
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn edge_registry_enforces_digest_size_and_unique_credentials(
    pool: PgPool,
) -> Result<(), sqlx::Error> {
    assert_edge_registry_exists(&pool).await?;
    for size in [31, 33] {
        let error = sqlx::query(
            "INSERT INTO edge_gateways (id, name, credential_digest) VALUES ($1, 'Fixture', $2)",
        )
        .bind(Uuid::new_v4())
        .bind(vec![1_u8; size])
        .execute(&pool)
        .await
        .unwrap_err();
        assert_database_code(error, "23514");
    }
    sqlx::query(
        "INSERT INTO edge_gateways (id, name, credential_digest) VALUES ($1, 'Fixture', $2)",
    )
    .bind(Uuid::new_v4())
    .bind(vec![2_u8; 32])
    .execute(&pool)
    .await?;
    let error = sqlx::query(
        "INSERT INTO edge_gateways (id, name, credential_digest) VALUES ($1, 'Other', $2)",
    )
    .bind(Uuid::new_v4())
    .bind(vec![2_u8; 32])
    .execute(&pool)
    .await
    .unwrap_err();
    assert_database_code(error, "23505");
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn edge_registry_enforces_gateway_camera_spot_relationships(
    pool: PgPool,
) -> Result<(), sqlx::Error> {
    assert_edge_registry_exists(&pool).await?;
    let gateway = Uuid::new_v4();
    let other_gateway = Uuid::new_v4();
    for (id, digest) in [(gateway, vec![3_u8; 32]), (other_gateway, vec![4_u8; 32])] {
        sqlx::query(
            "INSERT INTO edge_gateways (id, name, credential_digest) VALUES ($1, 'Fixture', $2)",
        )
        .bind(id)
        .bind(digest)
        .execute(&pool)
        .await?;
    }
    sqlx::query("INSERT INTO spots (id, status, x, y) VALUES ('scoped-spot', 'free', 0, 0)")
        .execute(&pool)
        .await?;
    let unknown_gateway = sqlx::query(
        "INSERT INTO edge_camera_grants (gateway_id, camera_id, spot_update, car_detected) \
         VALUES ($1, 'cam', true, false)",
    )
    .bind(Uuid::new_v4())
    .execute(&pool)
    .await
    .unwrap_err();
    assert_database_code(unknown_gateway, "23503");
    sqlx::query(
        "INSERT INTO edge_camera_grants (gateway_id, camera_id, spot_update, car_detected) \
         VALUES ($1, 'cam', true, false)",
    )
    .bind(gateway)
    .execute(&pool)
    .await?;
    for (id, camera, spot) in [
        (other_gateway, "cam", "scoped-spot"),
        (gateway, "unknown", "scoped-spot"),
        (gateway, "cam", "unknown"),
    ] {
        let error = sqlx::query(
            "INSERT INTO edge_camera_spots (gateway_id, camera_id, spot_id) VALUES ($1, $2, $3)",
        )
        .bind(id)
        .bind(camera)
        .bind(spot)
        .execute(&pool)
        .await
        .unwrap_err();
        assert_database_code(error, "23503");
    }
    sqlx::query(
        "INSERT INTO edge_camera_spots (gateway_id, camera_id, spot_id) \
         VALUES ($1, 'cam', 'scoped-spot')",
    )
    .bind(gateway)
    .execute(&pool)
    .await?;
    let duplicate = sqlx::query(
        "INSERT INTO edge_camera_spots (gateway_id, camera_id, spot_id) \
         VALUES ($1, 'cam', 'scoped-spot')",
    )
    .bind(gateway)
    .execute(&pool)
    .await
    .unwrap_err();
    assert_database_code(duplicate, "23505");
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn edge_registry_reversal_preserves_spots_and_allows_reapplication(
    pool: PgPool,
) -> Result<(), sqlx::Error> {
    // Isolate this migration from later receipt migrations referencing the registry.
    let mut tx = pool.begin().await?;
    sqlx::raw_sql(
        "CREATE SCHEMA registry_reversal; SET LOCAL search_path TO registry_reversal; \
         CREATE TABLE spots (id TEXT PRIMARY KEY); \
         INSERT INTO spots (id) VALUES ('preserved-spot')",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::raw_sql(include_str!(
        "../migrations/20261012010000_edge_gateways.up.sql"
    ))
    .execute(&mut *tx)
    .await?;
    sqlx::raw_sql(include_str!(
        "../migrations/20261012010000_edge_gateways.down.sql"
    ))
    .execute(&mut *tx)
    .await?;
    let removed: bool = sqlx::query_scalar(
        "SELECT to_regclass('edge_gateways') IS NULL \
         AND to_regclass('edge_camera_grants') IS NULL \
         AND to_regclass('edge_camera_spots') IS NULL",
    )
    .fetch_one(&mut *tx)
    .await?;
    assert!(
        removed,
        "reversal must remove only the new authority tables"
    );
    let spots: Vec<String> = sqlx::query_scalar("SELECT id FROM spots")
        .fetch_all(&mut *tx)
        .await?;
    assert_eq!(spots, ["preserved-spot"]);
    sqlx::raw_sql(include_str!(
        "../migrations/20261012010000_edge_gateways.up.sql"
    ))
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO edge_gateways (id, name, credential_digest) VALUES ($1, 'Reapplied', $2)",
    )
    .bind(Uuid::new_v4())
    .bind(vec![9_u8; 32])
    .execute(&mut *tx)
    .await?;
    tx.rollback().await?;
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn r02b_projection_storage_exists_and_enforces_single_forecast(pool: PgPool) {
    let exists: bool = sqlx::query_scalar("SELECT to_regclass('edge_path_state') IS NOT NULL AND to_regclass('edge_forecast_state') IS NOT NULL")
        .fetch_one(&pool).await.unwrap();
    assert!(exists, "durable projections must survive process restart");
    sqlx::query("INSERT INTO edge_forecast_state (singleton, payload) VALUES (true, '{}'::jsonb)")
        .execute(&pool)
        .await
        .unwrap();
    let error = sqlx::query(
        "INSERT INTO edge_forecast_state (singleton, payload) VALUES (false, '{}'::jsonb)",
    )
    .execute(&pool)
    .await
    .unwrap_err();
    assert_database_code(error, "23514");
}
