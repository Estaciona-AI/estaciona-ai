use std::sync::{Arc, Mutex};

use axum::{
    Json,
    body::to_bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::{Duration, Utc};
use dashmap::DashMap;
use sqlx::PgPool;
use tokio::sync::{RwLock, broadcast};
use uuid::Uuid;

use crate::{
    pathfinding::ParkingGraph,
    reservations::{self, CreateReservation, UpdateSpotStatus},
    state::{AppState, SharedState},
};

struct Fixture {
    state: SharedState,
    owner: Uuid,
    other: Uuid,
    admin: Uuid,
    spot_id: String,
}

impl Fixture {
    async fn new(pool: PgPool) -> Result<Self, sqlx::Error> {
        let owner = Uuid::new_v4();
        let other = Uuid::new_v4();
        let admin = Uuid::new_v4();
        for (id, role) in [(owner, "user"), (other, "user"), (admin, "admin")] {
            sqlx::query(
                "INSERT INTO users (id, name, email, password_hash, role) \
                 VALUES ($1, 'Reservation Fixture', $2, 'unused-test-hash', $3)",
            )
            .bind(id)
            .bind(format!("{id}@example.invalid"))
            .bind(role)
            .execute(&pool)
            .await?;
        }

        let spot_id = "R01-01".to_owned();
        sqlx::query("INSERT INTO spots (id, status, x, y) VALUES ($1, 'free', 0, 0)")
            .bind(&spot_id)
            .execute(&pool)
            .await?;

        let (tx, _) = broadcast::channel(100);
        let state = Arc::new(AppState {
            lpr_mode: Default::default(),
            pool,
            auth_limits: Default::default(),
            tx,
            user_sessions: DashMap::new(),
            graph: RwLock::new(ParkingGraph::new()),
            jwt_secret: "r01-disposable-jwt-secret".to_owned(),
            plate_pepper: "r01-disposable-plate-pepper".to_owned(),
            forecast_writer: Default::default(),
            last_trend_prediction: Mutex::new(None),
            config_store: crate::config_store::LocalConfigStore::new(
                std::env::temp_dir().join(format!("r01-reservation-{owner}")),
            ),
        });

        Ok(Self {
            state,
            owner,
            other,
            admin,
            spot_id,
        })
    }

    async fn observe_occupied(
        &self,
    ) -> Result<crate::edge_delivery::Observation, Box<dyn std::error::Error>> {
        let gateway_id = Uuid::new_v4();
        let key = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        sqlx::query("INSERT INTO edge_gateways (id,name,credential_digest) VALUES ($1,'reservation-fixture',$2)")
            .bind(gateway_id).bind(crate::edge_auth::credential_digest(&key).as_slice())
            .execute(&self.state.pool).await?;
        sqlx::query("INSERT INTO edge_camera_grants (gateway_id,camera_id,spot_update) VALUES ($1,'fixture-camera',true)")
            .bind(gateway_id).execute(&self.state.pool).await?;
        sqlx::query("INSERT INTO edge_camera_spots (gateway_id,camera_id,spot_id) VALUES ($1,'fixture-camera',$2)")
            .bind(gateway_id).bind(&self.spot_id).execute(&self.state.pool).await?;
        let principal = crate::edge_auth::authenticate(&self.state.pool, &key)
            .await?
            .expect("registered edge fixture");
        Ok(crate::edge_delivery::apply_legacy(
            &self.state.pool,
            &principal,
            "fixture-camera",
            &self.spot_id,
            "occupied",
            1.0,
        )
        .await?)
    }

    async fn headers(&self, user_id: Uuid, role: &str) -> HeaderMap {
        let token = crate::sessions::issue(&self.state.pool, user_id, role, &self.state.jwt_secret)
            .await
            .expect("fixture JWT must be valid");
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {token}")
                .parse()
                .expect("valid bearer header"),
        );
        headers
    }

    async fn create(&self, user_id: Uuid) -> Response {
        reservations::create_reservation(
            State(self.state.clone()),
            self.headers(user_id, "user").await,
            Json(CreateReservation {
                spot_id: self.spot_id.clone(),
                expires_at: Utc::now() + Duration::minutes(15),
            }),
        )
        .await
        .into_response()
    }

    async fn create_for_owner(&self) -> Uuid {
        let response = self.create(self.owner).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 16_384)
            .await
            .expect("reservation response body");
        let value: serde_json::Value =
            serde_json::from_slice(&body).expect("reservation response JSON");
        Uuid::parse_str(value["id"].as_str().expect("reservation response ID"))
            .expect("reservation UUID")
    }

    async fn update_spot(&self, status: &str) {
        let response = reservations::update_spot_status(
            State(self.state.clone()),
            Path(self.spot_id.clone()),
            self.headers(self.admin, "admin").await,
            Json(UpdateSpotStatus {
                status: status.to_owned(),
            }),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::OK);
    }

    async fn spot_status(&self) -> Result<String, sqlx::Error> {
        sqlx::query_scalar("SELECT status FROM spots WHERE id = $1")
            .bind(&self.spot_id)
            .fetch_one(&self.state.pool)
            .await
    }

    async fn reservation_status(&self, id: Uuid) -> Result<String, sqlx::Error> {
        sqlx::query_scalar("SELECT status FROM reservations WHERE id = $1")
            .bind(id)
            .fetch_one(&self.state.pool)
            .await
    }

    async fn active_count(&self) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM reservations WHERE spot_id = $1 AND status = 'active'",
        )
        .bind(&self.spot_id)
        .fetch_one(&self.state.pool)
        .await
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn r03_demoted_admin_cannot_mutate_a_spot(pool: PgPool) -> Result<(), sqlx::Error> {
    let fixture = Fixture::new(pool).await?;
    let headers = fixture.headers(fixture.admin, "admin").await;
    sqlx::query("UPDATE users SET role = 'user' WHERE id = $1")
        .bind(fixture.admin)
        .execute(&fixture.state.pool)
        .await?;
    let response = reservations::update_spot_status(
        State(fixture.state.clone()),
        Path(fixture.spot_id.clone()),
        headers,
        Json(UpdateSpotStatus {
            status: "occupied".to_owned(),
        }),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(fixture.spot_status().await?, "free");
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn r03_deleted_user_cannot_create_a_reservation(pool: PgPool) -> Result<(), sqlx::Error> {
    let fixture = Fixture::new(pool).await?;
    let headers = fixture.headers(fixture.owner, "user").await;
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(fixture.owner)
        .execute(&fixture.state.pool)
        .await?;
    let response = reservations::create_reservation(
        State(fixture.state.clone()),
        headers,
        Json(CreateReservation {
            spot_id: fixture.spot_id.clone(),
            expires_at: Utc::now() + Duration::minutes(15),
        }),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(fixture.spot_status().await?, "free");
    assert_eq!(fixture.active_count().await?, 0);
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn r03_authorization_database_failure_denies_admin_mutation(
    pool: PgPool,
) -> Result<(), sqlx::Error> {
    let fixture = Fixture::new(pool).await?;
    sqlx::query("ALTER TABLE users RENAME TO unavailable_users")
        .execute(&fixture.state.pool)
        .await?;
    let response = reservations::update_spot_status(
        State(fixture.state.clone()),
        Path(fixture.spot_id.clone()),
        fixture.headers(fixture.admin, "admin").await,
        Json(UpdateSpotStatus {
            status: "occupied".to_owned(),
        }),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(fixture.spot_status().await?, "free");
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn rest_rejects_second_active_reservation_after_spot_becomes_free(
    pool: PgPool,
) -> Result<(), sqlx::Error> {
    let fixture = Fixture::new(pool).await?;
    let first = fixture.create_for_owner().await;
    fixture.update_spot("occupied").await;
    fixture.update_spot("free").await;

    let second = fixture.create(fixture.other).await;
    assert_eq!(
        (
            second.status(),
            fixture.spot_status().await?,
            fixture.active_count().await?,
            fixture.reservation_status(first).await?,
        ),
        (
            StatusCode::CONFLICT,
            "free".to_owned(),
            1,
            "active".to_owned(),
        ),
        "another claim must be rejected without changing the spot or its active reservation",
    );
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn database_rejects_two_active_reservations_for_one_spot(
    pool: PgPool,
) -> Result<(), sqlx::Error> {
    let fixture = Fixture::new(pool).await?;
    fixture.create_for_owner().await;

    let result = sqlx::query(
        "INSERT INTO reservations (id, user_id, spot_id, expires_at) \
         VALUES ($1, $2, $3, NOW() + INTERVAL '15 minutes')",
    )
    .bind(Uuid::new_v4())
    .bind(fixture.other)
    .bind(&fixture.spot_id)
    .execute(&fixture.state.pool)
    .await;

    let error = result.expect_err("the database must enforce one active reservation per spot");
    assert_eq!(
        error
            .as_database_error()
            .and_then(|database_error| database_error.code())
            .as_deref(),
        Some("23505"),
    );
    assert_eq!(fixture.active_count().await?, 1);
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn rest_cancel_denies_a_different_owner_without_releasing_the_spot(
    pool: PgPool,
) -> Result<(), sqlx::Error> {
    let fixture = Fixture::new(pool).await?;
    let reservation_id = fixture.create_for_owner().await;

    let response = reservations::cancel_reservation(
        State(fixture.state.clone()),
        Path(reservation_id),
        fixture.headers(fixture.other, "user").await,
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(fixture.reservation_status(reservation_id).await?, "active");
    assert_eq!(fixture.spot_status().await?, "reserved");
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn rest_cancel_rolls_back_when_releasing_the_spot_fails(
    pool: PgPool,
) -> Result<(), sqlx::Error> {
    let fixture = Fixture::new(pool).await?;
    let reservation_id = fixture.create_for_owner().await;
    sqlx::raw_sql(
        "CREATE FUNCTION reject_test_spot_release() RETURNS TRIGGER LANGUAGE plpgsql AS $$
             BEGIN
                 IF NEW.status = 'free' THEN
                     RAISE EXCEPTION 'injected test release failure';
                 END IF;
                 RETURN NEW;
             END;
         $$;
         CREATE TRIGGER reject_test_spot_release
             BEFORE UPDATE ON spots
             FOR EACH ROW EXECUTE FUNCTION reject_test_spot_release();",
    )
    .execute(&fixture.state.pool)
    .await?;

    let response = reservations::cancel_reservation(
        State(fixture.state.clone()),
        Path(reservation_id),
        fixture.headers(fixture.owner, "user").await,
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(fixture.reservation_status(reservation_id).await?, "active");
    assert_eq!(fixture.spot_status().await?, "reserved");
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn unconfirmed_occupancy_does_not_overwrite_a_newer_free_observation(
    pool: PgPool,
) -> Result<(), sqlx::Error> {
    let fixture = Fixture::new(pool).await?;
    let id = fixture.create_for_owner().await;
    fixture.update_spot("occupied").await;
    fixture.update_spot("free").await;

    let outcome = crate::reservation_store::finish(
        &fixture.state.pool,
        id,
        crate::reservation_store::FinishReason::UnconfirmedOccupancy,
    )
    .await?;
    assert!(outcome.is_none());
    assert_eq!(fixture.spot_status().await?, "free");
    assert_eq!(fixture.reservation_status(id).await?, "active");
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn unconfirmed_occupancy_finishes_only_the_active_reservation(
    pool: PgPool,
) -> Result<(), sqlx::Error> {
    let fixture = Fixture::new(pool).await?;
    let id = fixture.create_for_owner().await;
    fixture.update_spot("occupied").await;

    let outcome = crate::reservation_store::finish(
        &fixture.state.pool,
        id,
        crate::reservation_store::FinishReason::UnconfirmedOccupancy,
    )
    .await?
    .expect("active reservation should finish while occupied");
    assert!(outcome.spot_status.is_none());
    assert_eq!(fixture.spot_status().await?, "occupied");
    assert_eq!(fixture.reservation_status(id).await?, "cancelled");
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn failed_reservation_insert_rolls_back_the_spot_claim(
    pool: PgPool,
) -> Result<(), sqlx::Error> {
    let fixture = Fixture::new(pool).await?;
    let outcome = crate::reservation_store::create(
        &fixture.state.pool,
        &fixture.spot_id,
        Uuid::new_v4(), // Missing user: a persistence failure, not spot contention.
        Utc::now() + Duration::minutes(15),
    )
    .await;
    assert!(matches!(
        outcome,
        Err(crate::reservation_store::CreateError::Database(_))
    ));
    assert_eq!(fixture.spot_status().await?, "free");
    assert_eq!(fixture.active_count().await?, 0);
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn occupied_observation_rolls_back_if_capturing_its_reservation_fails(
    pool: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new(pool).await?;
    fixture.create_for_owner().await;
    // Simulate a persistence failure after the spot write, not a valid empty lookup.
    sqlx::query("DROP TABLE reservations")
        .execute(&fixture.state.pool)
        .await?;

    let result = fixture.observe_occupied().await;
    assert!(result.is_err());
    assert_eq!(
        fixture.spot_status().await?,
        "reserved",
        "the observation and its reservation capture must commit together",
    );
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn occupied_observation_captures_the_reservation_that_owns_its_spot(
    pool: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new(pool).await?;
    let id = fixture.create_for_owner().await;
    let result = fixture.observe_occupied().await?;
    assert_eq!(result.outcome, crate::edge_delivery::Outcome::Applied);
    assert_eq!(result.unconfirmed_reservation, Some(id));
    assert_eq!(fixture.spot_status().await?, "occupied");
    assert_eq!(fixture.reservation_status(id).await?, "active");
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn retired_spot_rejects_admin_mutation_without_releasing_history(pool: PgPool) {
    let f = Fixture::new(pool).await.unwrap();
    sqlx::query("UPDATE spots SET is_active=FALSE,status='occupied' WHERE id=$1")
        .bind(&f.spot_id)
        .execute(&f.state.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO user_occupancy_history (user_id,spot_id) VALUES ($1,$2)")
        .bind(f.owner)
        .bind(&f.spot_id)
        .execute(&f.state.pool)
        .await
        .unwrap();
    let mut updates = f.state.tx.subscribe();
    let response = reservations::update_spot_status(
        State(f.state.clone()),
        Path(f.spot_id.clone()),
        f.headers(f.admin, "admin").await,
        Json(UpdateSpotStatus {
            status: "free".into(),
        }),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(updates.try_recv().is_err());
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM user_occupancy_history WHERE spot_id=$1 AND released_at IS NULL"
        )
        .bind(&f.spot_id)
        .fetch_one(&f.state.pool)
        .await
        .unwrap(),
        1
    );
}

async fn assert_recommendation_ignores_retired(pool: PgPool, branch: &str) {
    let f = Fixture::new(pool).await.unwrap();
    let retired = if branch == "priority" { "A-01" } else { "B-07" };
    sqlx::query("INSERT INTO spots (id,status,x,y,is_active) VALUES ($1,'free',1,0,FALSE),('C-01','free',10,0,TRUE)").bind(retired).execute(&f.state.pool).await.unwrap();
    if branch == "priority" {
        sqlx::query("UPDATE users SET pcd_status=TRUE WHERE id=$1")
            .bind(f.owner)
            .execute(&f.state.pool)
            .await
            .unwrap();
    }
    if branch == "history" {
        for _ in 0..3 {
            sqlx::query("INSERT INTO user_occupancy_history (user_id,spot_id) VALUES ($1,$2)")
                .bind(f.owner)
                .bind(retired)
                .execute(&f.state.pool)
                .await
                .unwrap();
        }
    }
    let mut graph = ParkingGraph::new();
    let cam = graph.add_node("cam-01", 0.0, 0.0).unwrap();
    let old = graph.add_node(retired, 1.0, 0.0).unwrap();
    let current = graph.add_node("C-01", 10.0, 0.0).unwrap();
    graph.add_edge(cam, old, 1, true);
    graph.add_edge(cam, current, 10, true);
    *f.state.graph.write().await = graph;
    let response = reservations::recommend_spot(
        State(f.state.clone()),
        f.headers(f.owner, "user").await,
        axum::extract::Query(reservations::RecommendQuery { user_id: f.owner }),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 16384).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["recommended_spot"], "C-01");
}
#[sqlx::test(migrations = "./migrations")]
async fn priority_recommendation_excludes_retired_spots(pool: PgPool) {
    assert_recommendation_ignores_retired(pool, "priority").await;
}
#[sqlx::test(migrations = "./migrations")]
async fn historical_recommendation_excludes_retired_spots(pool: PgPool) {
    assert_recommendation_ignores_retired(pool, "history").await;
}
#[sqlx::test(migrations = "./migrations")]
async fn closest_recommendation_excludes_retired_spots(pool: PgPool) {
    assert_recommendation_ignores_retired(pool, "closest").await;
}

async fn recommendation_fixture(pool: PgPool) -> Fixture {
    let f = Fixture::new(pool).await.unwrap();
    let mut graph = ParkingGraph::new();
    let camera = graph.add_node("cam-01", 0.0, 0.0).unwrap();
    let spot = graph.add_node(&f.spot_id, 1.0, 0.0).unwrap();
    graph.add_edge(camera, spot, 1, true);
    *f.state.graph.write().await = graph;
    f
}

async fn recommendation_response(f: &Fixture) -> Response {
    reservations::recommend_spot(
        State(f.state.clone()),
        f.headers(f.owner, "user").await,
        axum::extract::Query(reservations::RecommendQuery { user_id: f.owner }),
    )
    .await
    .into_response()
}

async fn assert_recommendation_unavailable(f: &Fixture) {
    let mut updates = f.state.tx.subscribe();
    let response = recommendation_response(f).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = to_bytes(response.into_body(), 16384).await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    for internal_detail in [
        "injected",
        "SELECT",
        "Lotado",
        "pcd_status",
        "user_occupancy_history",
        "r08_",
    ] {
        assert!(
            !text.contains(internal_detail),
            "database details must stay private: {text}"
        );
    }
    assert_eq!(f.spot_status().await.unwrap(), "free");
    assert_eq!(f.active_count().await.unwrap(), 0);
    assert!(updates.try_recv().is_err());
}

#[sqlx::test(migrations = "./migrations")]
async fn r08_priority_query_failure_does_not_fall_back_to_an_ordinary_spot(pool: PgPool) {
    let f = recommendation_fixture(pool).await;
    // Break the priority query at prepare time, while the role lookup still works.
    sqlx::query("ALTER TABLE users RENAME COLUMN pcd_status TO r08_unavailable")
        .execute(&f.state.pool)
        .await
        .unwrap();
    assert_recommendation_unavailable(&f).await;
}

#[sqlx::test(migrations = "./migrations")]
async fn r08_history_count_failure_does_not_fall_back_to_the_closest_spot(pool: PgPool) {
    let f = recommendation_fixture(pool).await;
    sqlx::query("ALTER TABLE user_occupancy_history RENAME TO r08_unavailable")
        .execute(&f.state.pool)
        .await
        .unwrap();
    assert_recommendation_unavailable(&f).await;
}

#[sqlx::test(migrations = "./migrations")]
async fn r08_favorite_lookup_failure_does_not_fall_back_to_the_closest_spot(pool: PgPool) {
    let f = recommendation_fixture(pool).await;
    for _ in 0..3 {
        sqlx::query("INSERT INTO user_occupancy_history (user_id,spot_id) VALUES ($1,$2)")
            .bind(f.owner)
            .bind(&f.spot_id)
            .execute(&f.state.pool)
            .await
            .unwrap();
    }
    // COUNT(*) still works; only the subsequent favorite lookup loses its column.
    sqlx::query("ALTER TABLE user_occupancy_history RENAME COLUMN spot_id TO r08_unavailable")
        .execute(&f.state.pool)
        .await
        .unwrap();
    assert_recommendation_unavailable(&f).await;
}

#[sqlx::test(migrations = "./migrations")]
async fn r08_free_spots_query_failure_is_unavailable_instead_of_full(pool: PgPool) {
    let f = recommendation_fixture(pool).await;
    // Earlier priority reads and later verification reads must remain healthy.
    // Inject only the final ordinary-spots SELECT, using a disposable SQL view.
    sqlx::raw_sql(
        "ALTER TABLE spots RENAME TO r08_original_spots;
         CREATE FUNCTION r08_free_read_allowed() RETURNS BOOLEAN
         LANGUAGE plpgsql VOLATILE AS $$ BEGIN
             IF strpos(current_query(), 'AND id NOT IN') > 0 THEN
                 RAISE EXCEPTION 'injected R08 free spots failure';
             END IF;
             RETURN TRUE;
         END $$;
         CREATE VIEW spots AS SELECT * FROM r08_original_spots
         WHERE r08_free_read_allowed();",
    )
    .execute(&f.state.pool)
    .await
    .unwrap();
    assert_recommendation_unavailable(&f).await;
}

#[sqlx::test(migrations = "./migrations")]
async fn r08_successful_empty_lookup_still_reports_full(pool: PgPool) {
    let f = recommendation_fixture(pool).await;
    sqlx::query("UPDATE spots SET status='occupied'")
        .execute(&f.state.pool)
        .await
        .unwrap();
    let response = recommendation_response(&f).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = to_bytes(response.into_body(), 16384).await.unwrap();
    assert_eq!(body.as_ref(), "Estacionamento Lotado".as_bytes());
}

async fn assert_recommended_spot(f: &Fixture, expected: &str) {
    let response = recommendation_response(f).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 16384).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["recommended_spot"], expected);
    assert_eq!(value["route"], serde_json::json!(["cam-01", expected]));
}

async fn add_preferred_spot(f: &Fixture, spot_id: &str) {
    sqlx::query("INSERT INTO spots (id,status,x,y) VALUES ($1,'free',10,0)")
        .bind(spot_id)
        .execute(&f.state.pool)
        .await
        .unwrap();
    let mut graph = ParkingGraph::new();
    let camera = graph.add_node("cam-01", 0.0, 0.0).unwrap();
    let ordinary = graph.add_node(&f.spot_id, 1.0, 0.0).unwrap();
    let preferred = graph.add_node(spot_id, 10.0, 0.0).unwrap();
    graph.add_edge(camera, ordinary, 1, true);
    graph.add_edge(camera, preferred, 10, true);
    *f.state.graph.write().await = graph;
}

#[sqlx::test(migrations = "./migrations")]
async fn r08_valid_priority_still_takes_precedence_over_distance(pool: PgPool) {
    let f = recommendation_fixture(pool).await;
    add_preferred_spot(&f, "A-01").await;
    sqlx::query("UPDATE users SET pcd_status=TRUE WHERE id=$1")
        .bind(f.owner)
        .execute(&f.state.pool)
        .await
        .unwrap();
    assert_recommended_spot(&f, "A-01").await;
}

#[sqlx::test(migrations = "./migrations")]
async fn r08_valid_history_still_takes_precedence_over_distance(pool: PgPool) {
    let f = recommendation_fixture(pool).await;
    add_preferred_spot(&f, "B-07").await;
    for _ in 0..3 {
        sqlx::query("INSERT INTO user_occupancy_history (user_id,spot_id) VALUES ($1,'B-07')")
            .bind(f.owner)
            .execute(&f.state.pool)
            .await
            .unwrap();
    }
    assert_recommended_spot(&f, "B-07").await;
}

#[sqlx::test(migrations = "./migrations")]
async fn r08_valid_empty_history_still_uses_distance(pool: PgPool) {
    let f = recommendation_fixture(pool).await;
    add_preferred_spot(&f, "B-07").await;
    assert_recommended_spot(&f, "R01-01").await;
}
