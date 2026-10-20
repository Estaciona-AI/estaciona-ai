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

// These characterize the independence already provided by the composed context.
// No multi-tenant listener or shared application schema is introduced for testing.
struct SiteFixture {
    directory: std::path::PathBuf,
    identity: super::InstallationIdentity,
}

impl SiteFixture {
    fn new(index: u128, hold_seconds: i32) -> Self {
        let directory =
            std::env::temp_dir().join(format!("estaciona-context-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let descriptor = serde_json::json!({
            "schema_version": 1,
            "organization": {"id": uuid::Uuid::from_u128(100+index), "slug": "synthetic-org", "name": "Test organization"},
            "installation": {"id": uuid::Uuid::from_u128(index), "slug": "synthetic-site", "name": "Test installation"},
            "environment": "test",
        });
        std::fs::write(directory.join("installation.json"), descriptor.to_string()).unwrap();
        std::fs::write(
            directory.join("config.json"),
            serde_json::json!([
                {"name": "Synthetic lot", "path": [{"x":0,"z":0},{"x":1,"z":0}]}
            ])
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            directory.join("spots_3d.json"),
            serde_json::json!([
                {"id":"shared-spot","center3D":{"x":2,"y":0,"z":1}}
            ])
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            directory.join("reservation_policy.json"),
            serde_json::json!({"hold_seconds":hold_seconds}).to_string(),
        )
        .unwrap();
        let identity = super::InstallationDescriptor::load(&directory.join("installation.json"))
            .unwrap()
            .identity();
        Self {
            directory,
            identity,
        }
    }

    async fn initialize(&self, pool: &PgPool) -> crate::state::SharedState {
        super::cli::execute(pool, &self.identity, &super::cli::Operation::Init)
            .await
            .unwrap();
        self.restart(pool).await
    }

    async fn restart(&self, pool: &PgPool) -> crate::state::SharedState {
        let verified = startup::prepare(pool.clone(), self.identity).await.unwrap();
        crate::state::init_state(
            verified,
            "synthetic-jwt".into(),
            "synthetic-pepper".into(),
            crate::site_config::SiteConfig::load(&self.directory).unwrap(),
            Default::default(),
        )
        .await
        .unwrap()
    }
}

impl Drop for SiteFixture {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.directory) {
            eprintln!("Synthetic site fixture cleanup failed: {error}");
        }
    }
}

struct SeparateDatabase {
    pool: PgPool,
    admin: PgPool,
    name: String,
    cleaned: bool,
}

impl SeparateDatabase {
    async fn create(admin: &PgPool) -> Self {
        // Identifier contains only a fixed prefix and a generated hexadecimal UUID.
        let name = format!("estaciona_context_{}", uuid::Uuid::new_v4().simple());
        sqlx::QueryBuilder::<sqlx::Postgres>::new("CREATE DATABASE ")
            .push(name.clone())
            .build()
            .execute(admin)
            .await
            .unwrap();
        let options = admin.connect_options().as_ref().clone().database(&name);
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .unwrap();
        Self {
            pool,
            admin: admin.clone(),
            name,
            cleaned: false,
        }
    }

    async fn cleanup(mut self) {
        self.pool.close().await;
        sqlx::QueryBuilder::<sqlx::Postgres>::new("DROP DATABASE ")
            .push(self.name.clone())
            .push(" WITH (FORCE)")
            .build()
            .execute(&self.admin)
            .await
            .unwrap();
        self.cleaned = true;
    }
}

impl Drop for SeparateDatabase {
    fn drop(&mut self) {
        if self.cleaned {
            return;
        }
        // Best-effort panic cleanup; the enclosing disposable container is removed as well.
        let pool = self.pool.clone();
        let admin = self.admin.clone();
        let name = self.name.clone();
        tokio::spawn(async move {
            pool.close().await;
            if let Err(error) = sqlx::QueryBuilder::<sqlx::Postgres>::new("DROP DATABASE ")
                .push(name)
                .push(" WITH (FORCE)")
                .build()
                .execute(&admin)
                .await
            {
                eprintln!("Synthetic database cleanup failed: {error}");
            }
        });
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn graph_cache_channels_connections_and_files_belong_to_one_context(pool: PgPool) {
    let second = SeparateDatabase::create(&pool).await;
    let a_site = SiteFixture::new(2, 300);
    let b_site = SiteFixture::new(3, 600);
    let a = a_site.initialize(&pool).await;
    let b = b_site.initialize(&second.pool).await;
    assert_ne!(a.installation.identity(), b.installation.identity());
    assert_eq!(a.installation.reservation_policy.hold_seconds, 300);
    assert_eq!(b.installation.reservation_policy.hold_seconds, 600);

    let mut a_updates = a.installation.tx.subscribe();
    let mut b_updates = b.installation.tx.subscribe();
    a.installation.tx.send("only-a".into()).unwrap();
    assert_eq!(a_updates.try_recv().unwrap(), "only-a");
    assert!(matches!(
        b_updates.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
    a.installation
        .graph
        .write()
        .await
        .set_edge_status("cam-01", "path_node_1", false);
    assert!(
        a.installation
            .graph
            .read()
            .await
            .calculate_route("cam-01", "shared-spot")
            .is_none()
    );
    assert!(
        b.installation
            .graph
            .read()
            .await
            .calculate_route("cam-01", "shared-spot")
            .is_some()
    );
    *a.installation.last_trend_prediction.lock().unwrap() = Some("only-a".into());
    assert!(
        b.installation
            .last_trend_prediction
            .lock()
            .unwrap()
            .is_none()
    );

    let user = uuid::Uuid::from_u128(42);
    let (a_session, mut a_messages, _) = crate::ws::private_session::channel();
    let (b_session, mut b_messages, _) = crate::ws::private_session::channel();
    a.installation.user_sessions.insert(user, a_session.clone());
    b.installation.user_sessions.insert(user, b_session.clone());
    a.installation
        .user_sessions
        .get(&user)
        .unwrap()
        .try_send("private-a".into())
        .unwrap();
    assert_eq!(a_messages.try_recv().unwrap(), "private-a");
    assert!(matches!(
        b_messages.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    a.installation
        .user_sessions
        .remove_if(&user, |_, current| current.same_channel(&a_session));
    assert!(a.installation.user_sessions.is_empty());
    b.installation
        .user_sessions
        .get(&user)
        .unwrap()
        .try_send("private-b".into())
        .unwrap();
    assert_eq!(b_messages.try_recv().unwrap(), "private-b");

    let b_before = std::fs::read(b_site.directory.join("config.json")).unwrap();
    let replacement = serde_json::json!([{"name":"Only A","path":[{"x":0,"z":0},{"x":5,"z":0}]}]);
    a.installation
        .config_store
        .save(replacement.clone())
        .await
        .unwrap();
    let a_after: serde_json::Value =
        serde_json::from_slice(&std::fs::read(a_site.directory.join("config.json")).unwrap())
            .unwrap();
    assert_eq!(a_after, replacement);
    assert_eq!(
        std::fs::read(b_site.directory.join("config.json")).unwrap(),
        b_before
    );
    second.cleanup().await;
}

#[sqlx::test(migrations = "./migrations")]
async fn reservation_handler_selects_its_context_database_with_repeated_local_ids(pool: PgPool) {
    use axum::{
        Json,
        extract::State,
        http::{HeaderMap, StatusCode, header},
        response::IntoResponse,
    };
    let second = SeparateDatabase::create(&pool).await;
    let a_site = SiteFixture::new(2, 300);
    let b_site = SiteFixture::new(3, 600);
    let a = a_site.initialize(&pool).await;
    let b = b_site.initialize(&second.pool).await;
    let user = uuid::Uuid::from_u128(42);
    let gateway = uuid::Uuid::from_u128(43);
    for state in [&a, &b] {
        sqlx::query(
            "INSERT INTO users(id,email,password_hash) VALUES ($1,'same@example.invalid','unused')",
        )
        .bind(user)
        .execute(state.installation.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO edge_gateways(id,name,credential_digest) VALUES ($1,'same-gateway',$2)",
        )
        .bind(gateway)
        .bind(vec![1u8; 32])
        .execute(state.installation.pool())
        .await
        .unwrap();
        sqlx::query("INSERT INTO edge_camera_grants(gateway_id,camera_id,spot_update) VALUES ($1,'same-camera',true)")
            .bind(gateway).execute(state.installation.pool()).await.unwrap();
        sqlx::query("INSERT INTO edge_camera_spots(gateway_id,camera_id,spot_id) VALUES ($1,'same-camera','shared-spot')")
            .bind(gateway).execute(state.installation.pool()).await.unwrap();
    }
    for (index, state) in [&a, &b].into_iter().enumerate() {
        let token =
            crate::sessions::issue(state.installation.pool(), user, "user", &state.jwt_secret)
                .await
                .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        let response = crate::reservations::create_reservation(
            State(state.clone()),
            headers,
            Json(crate::reservations::CreateReservation {
                spot_id: "shared-spot".into(),
                expires_at: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        if index == 0 {
            let count: i64 = sqlx::query_scalar("SELECT count(*) FROM reservations")
                .fetch_one(b.installation.pool())
                .await
                .unwrap();
            assert_eq!(count, 0);
            let status: String =
                sqlx::query_scalar("SELECT status FROM spots WHERE id='shared-spot'")
                    .fetch_one(b.installation.pool())
                    .await
                    .unwrap();
            assert_eq!(status, "free");
        }
    }
    for (state, seconds) in [(&a, 300.0f64), (&b, 600.0)] {
        let duration: f64 = sqlx::query_scalar(
            "SELECT EXTRACT(EPOCH FROM (expires_at-created_at))::float8 FROM reservations",
        )
        .fetch_one(state.installation.pool())
        .await
        .unwrap();
        assert!((duration - seconds).abs() < 1.0);
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM reservations")
            .fetch_one(state.installation.pool())
            .await
            .unwrap();
        assert_eq!(count, 1);
    }
    second.cleanup().await;
}

#[sqlx::test(migrations = "./migrations")]
async fn restart_restores_forecast_and_paths_from_each_context_database(pool: PgPool) {
    let second = SeparateDatabase::create(&pool).await;
    let a_site = SiteFixture::new(2, 300);
    let b_site = SiteFixture::new(3, 600);
    let a = a_site.initialize(&pool).await;
    let b = b_site.initialize(&second.pool).await;
    let forecast = |capacity| {
        serde_json::json!({
            "type":"TREND_PREDICTION", "timestamp":"2026-10-20T00:00:00Z",
            "avg_stay_duration_mins":30.0, "stay_duration_distribution":[],
            "max_capacity":capacity, "model_health":{"r2_score":0.0,"mae":0.0,"rmse":0.0,"inference_time_ms":1.0}, "next_24h_occupancy":[],
        })
    };
    for (state, capacity) in [(&a, 100), (&b, 200)] {
        sqlx::query("INSERT INTO edge_forecast_state(singleton,payload) VALUES (true,$1)")
            .bind(sqlx::types::Json(forecast(capacity)))
            .execute(state.installation.pool())
            .await
            .unwrap();
    }
    let revision = a.installation.graph.read().await.revision();
    sqlx::query("INSERT INTO edge_path_state(map_revision,from_node,to_node,is_active) VALUES ($1,'cam-01','path_node_1',false)")
        .bind(revision).execute(a.installation.pool()).await.unwrap();
    drop(a);
    drop(b);
    let a = a_site.restart(&pool).await;
    let b = b_site.restart(&second.pool).await;
    for (state, capacity) in [(&a, 100), (&b, 200)] {
        let cached: serde_json::Value = serde_json::from_str(
            state
                .installation
                .last_trend_prediction
                .lock()
                .unwrap()
                .as_deref()
                .unwrap(),
        )
        .unwrap();
        serde_json::from_value::<crate::ws::messages::ServerToAppMsg>(cached.clone()).unwrap();
        assert_eq!(cached, forecast(capacity));
        assert!(matches!(
            state.installation.tx.subscribe().try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
    }
    assert!(
        a.installation
            .graph
            .read()
            .await
            .calculate_route("cam-01", "shared-spot")
            .is_none()
    );
    assert!(
        b.installation
            .graph
            .read()
            .await
            .calculate_route("cam-01", "shared-spot")
            .is_some()
    );
    second.cleanup().await;
}
