// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

use super::{
    startup,
    store::{self, BindMode, StoreError},
    store_tests::identity,
};
use sqlx::PgPool;

#[sqlx::test(migrations = "./migrations")]
async fn verified_identity_reaches_runtime_and_uses_its_database(pool: PgPool) {
    let identity = identity(2, "test");
    store::bind(&pool, &identity, BindMode::EmptyDatabase)
        .await
        .unwrap();
    let verified = startup::prepare(pool.clone(), identity).await.unwrap();
    assert_eq!(verified.identity(), identity);
    let site = crate::site_config::SiteConfig {
        directory: std::env::temp_dir(),
        parkings: vec![crate::site_config::ParkingLotConfig {
            path: vec![
                crate::site_config::PathNode { x: 0.0, z: 0.0 },
                crate::site_config::PathNode { x: 1.0, z: 0.0 },
            ],
        }],
        spots: vec![crate::site_config::Spot3DDef {
            id: "verified-spot".into(),
            center_3d: crate::site_config::Coords3D {
                x: 2.0,
                y: 0.0,
                z: 1.0,
            },
        }],
    };
    let state = crate::state::init_state(
        verified,
        "test-jwt".into(),
        "test-pepper".into(),
        site,
        Default::default(),
    )
    .await
    .unwrap();
    assert_eq!(state.installation.identity(), identity);
    assert_eq!(state.installation.reservation_policy.hold_seconds, 900);
    assert!(
        state
            .installation
            .last_trend_prediction
            .lock()
            .unwrap()
            .is_none()
    );
    assert!(state.installation.user_sessions.is_empty());
    let mut updates = state.installation.tx.subscribe();
    state
        .installation
        .tx
        .send("synthetic-update".into())
        .unwrap();
    assert_eq!(updates.recv().await.unwrap(), "synthetic-update");
    assert!(
        state
            .installation
            .graph
            .read()
            .await
            .calculate_route("cam-01", "verified-spot")
            .is_some()
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM spots WHERE id='verified-spot'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn unbound_or_mismatched_database_cannot_produce_verified_context(pool: PgPool) {
    let expected = identity(2, "test");
    assert!(matches!(
        startup::prepare(pool.clone(), expected).await,
        Err(StoreError::Unbound)
    ));
    store::bind(&pool, &expected, BindMode::EmptyDatabase)
        .await
        .unwrap();
    assert!(matches!(
        startup::prepare(pool.clone(), identity(3, "test")).await,
        Err(StoreError::IdentityMismatch)
    ));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM site_map_revisions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}
