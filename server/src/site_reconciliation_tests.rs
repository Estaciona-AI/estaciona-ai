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

