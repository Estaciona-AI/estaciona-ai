// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;

use axum::{
    Json,
    body::to_bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::{Datelike, Duration, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    pathfinding::ParkingGraph,
    reservations::{self, CreateReservation, UpdateSpotStatus},
    state::{AppState, SharedState},
};

pub(crate) struct Fixture {
    pub(crate) state: SharedState,
    pub(crate) owner: Uuid,
    pub(crate) other: Uuid,
    pub(crate) admin: Uuid,
    pub(crate) spot_id: String,
}

impl Fixture {
    pub(crate) async fn new(pool: PgPool) -> Result<Self, sqlx::Error> {
        let verified = crate::installation::test_support::provision(&pool).await;
        let owner = Uuid::new_v4();
        let other = Uuid::new_v4();
        let admin = Uuid::new_v4();
        for (id, role) in [(owner, "user"), (other, "user"), (admin, "admin")] {
            sqlx::query("INSERT INTO users (id, role) VALUES ($1, $2)")
                .bind(id)
                .bind(role)
                .execute(&pool)
                .await?;
        }

        let spot_id = "R01-01".to_owned();
        sqlx::query("INSERT INTO spots (id, status, x, y, observation_timestamp) VALUES ($1, 'free', 0, 0, NOW() - INTERVAL '30 seconds')")
            .bind(&spot_id)
            .execute(&pool)
            .await?;

        let state = Arc::new(AppState {
            installation: crate::installation::InstallationContext::for_unit_test(
                verified,
                ParkingGraph::new(),
                std::env::temp_dir().join(format!("r01-reservation-{owner}")),
            ),
            auth_limits: Default::default(),
            jwt_secret: "r01-disposable-jwt-secret".to_owned(),
            plate_pepper: "r01-disposable-plate-pepper".to_owned(),
            personal_data: crate::personal_data::PersonalDataCipher::for_unit_test(),
            identity: crate::identity::IdentityProvider::for_unit_test(),
            ws_admission: Default::default(),
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
            .execute(self.state.installation.pool()).await?;
        sqlx::query("INSERT INTO edge_camera_grants (gateway_id,camera_id,spot_update) VALUES ($1,'fixture-camera',true)")
            .bind(gateway_id).execute(self.state.installation.pool()).await?;
        sqlx::query("INSERT INTO edge_camera_spots (gateway_id,camera_id,spot_id) VALUES ($1,'fixture-camera',$2)")
            .bind(gateway_id).bind(&self.spot_id).execute(self.state.installation.pool()).await?;
        let principal = crate::edge_auth::authenticate(self.state.installation.pool(), &key)
            .await?
            .expect("registered edge fixture");
        Ok(crate::edge_delivery::apply_legacy(
            self.state.installation.pool(),
            &principal,
            "fixture-camera",
            &self.spot_id,
            "occupied",
            1.0,
            Utc::now(),
        )
        .await?)
    }

    pub(crate) async fn headers(&self, user_id: Uuid, role: &str) -> HeaderMap {
        let token = crate::sessions::issue(
            self.state.installation.pool(),
            user_id,
            role,
            &self.state.jwt_secret,
        )
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
                expires_at: Some(Utc::now() + Duration::minutes(15)),
            }),
        )
        .await
        .into_response()
    }

    pub(crate) async fn create_for_owner(&self) -> Uuid {
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
            .fetch_one(self.state.installation.pool())
            .await
    }

    async fn reservation_status(&self, id: Uuid) -> Result<String, sqlx::Error> {
        sqlx::query_scalar("SELECT status FROM reservations WHERE id = $1")
            .bind(id)
            .fetch_one(self.state.installation.pool())
            .await
    }

    async fn active_count(&self) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM reservations WHERE spot_id = $1 AND status = 'active'",
        )
        .bind(&self.spot_id)
        .fetch_one(self.state.installation.pool())
        .await
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn r03_demoted_admin_cannot_mutate_a_spot(pool: PgPool) -> Result<(), sqlx::Error> {
    let fixture = Fixture::new(pool).await?;
    let headers = fixture.headers(fixture.admin, "admin").await;
    sqlx::query("UPDATE users SET role = 'user' WHERE id = $1")
        .bind(fixture.admin)
        .execute(fixture.state.installation.pool())
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
        .execute(fixture.state.installation.pool())
        .await?;
    let response = reservations::create_reservation(
        State(fixture.state.clone()),
        headers,
        Json(CreateReservation {
            spot_id: fixture.spot_id.clone(),
            expires_at: Some(Utc::now() + Duration::minutes(15)),
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
        .execute(fixture.state.installation.pool())
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
    .execute(fixture.state.installation.pool())
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
    .execute(fixture.state.installation.pool())
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
        fixture.state.installation.pool(),
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
        fixture.state.installation.pool(),
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
        fixture.state.installation.pool(),
        &fixture.spot_id,
        Uuid::new_v4(), // Missing user: a persistence failure, not spot contention.
        &crate::reservation_policy::ReservationPolicy::default(),
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
        .execute(fixture.state.installation.pool())
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
        .execute(f.state.installation.pool())
        .await
        .unwrap();
    sqlx::query("INSERT INTO user_occupancy_history (user_id,spot_id) VALUES ($1,$2)")
        .bind(f.owner)
        .bind(&f.spot_id)
        .execute(f.state.installation.pool())
        .await
        .unwrap();
    let mut updates = f.state.installation.tx.subscribe();
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
        .fetch_one(f.state.installation.pool())
        .await
        .unwrap(),
        1
    );
}

async fn assert_recommendation_ignores_retired(pool: PgPool, branch: &str) {
    let f = Fixture::new(pool).await.unwrap();
    let retired = if branch == "priority" {
        "KEPT-7"
    } else {
        "B-07"
    };
    sqlx::query("INSERT INTO spots (id,status,x,y,is_active,observation_timestamp,reserved_for) VALUES ($1,'free',1,0,FALSE,NOW() - INTERVAL '30 seconds',$2),('C-01','free',10,0,TRUE,NOW() - INTERVAL '30 seconds',$2)").bind(retired).bind((branch == "priority").then_some("pcd")).execute(f.state.installation.pool()).await.unwrap();
    if branch == "priority" {
        let pcd = f
            .state
            .personal_data
            .encrypt(
                crate::personal_data::Field::UserPcdStatus,
                f.owner,
                &Some(true),
            )
            .unwrap();
        sqlx::query("UPDATE users SET pcd_status_enc=$2 WHERE id=$1")
            .bind(f.owner)
            .bind(pcd)
            .execute(f.state.installation.pool())
            .await
            .unwrap();
    }
    if branch == "history" {
        for _ in 0..3 {
            sqlx::query("INSERT INTO user_occupancy_history (user_id,spot_id) VALUES ($1,$2)")
                .bind(f.owner)
                .bind(retired)
                .execute(f.state.installation.pool())
                .await
                .unwrap();
        }
    }
    let mut graph = ParkingGraph::new();
    let cam = graph.add_node("cam-01", 0.0, 0.0).unwrap();
    graph.mark_entrance("cam-01");
    let old = graph.add_node(retired, 1.0, 0.0).unwrap();
    let current = graph.add_node("C-01", 10.0, 0.0).unwrap();
    graph.add_edge(cam, old, 1, true);
    graph.add_edge(cam, current, 10, true);
    *f.state.installation.graph.write().await = graph;
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
    graph.mark_entrance("cam-01");
    let spot = graph.add_node(&f.spot_id, 1.0, 0.0).unwrap();
    graph.add_edge(camera, spot, 1, true);
    *f.state.installation.graph.write().await = graph;
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
    let mut updates = f.state.installation.tx.subscribe();
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
    sqlx::query("ALTER TABLE users RENAME COLUMN pcd_status_enc TO r08_unavailable")
        .execute(f.state.installation.pool())
        .await
        .unwrap();
    assert_recommendation_unavailable(&f).await;
}

#[sqlx::test(migrations = "./migrations")]
async fn r08_history_count_failure_does_not_fall_back_to_the_closest_spot(pool: PgPool) {
    let f = recommendation_fixture(pool).await;
    sqlx::query("ALTER TABLE user_occupancy_history RENAME TO r08_unavailable")
        .execute(f.state.installation.pool())
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
            .execute(f.state.installation.pool())
            .await
            .unwrap();
    }
    // COUNT(*) still works; only the subsequent favorite lookup loses its column.
    sqlx::query("ALTER TABLE user_occupancy_history RENAME COLUMN spot_id TO r08_unavailable")
        .execute(f.state.installation.pool())
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
             IF strpos(current_query(), 'AND reserved_for IS NULL') > 0 THEN
                 RAISE EXCEPTION 'injected R08 free spots failure';
             END IF;
             RETURN TRUE;
         END $$;
         CREATE VIEW spots AS SELECT * FROM r08_original_spots
         WHERE r08_free_read_allowed();",
    )
    .execute(f.state.installation.pool())
    .await
    .unwrap();
    assert_recommendation_unavailable(&f).await;
}

#[sqlx::test(migrations = "./migrations")]
async fn r08_successful_empty_lookup_still_reports_full(pool: PgPool) {
    let f = recommendation_fixture(pool).await;
    sqlx::query("UPDATE spots SET status='occupied'")
        .execute(f.state.installation.pool())
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
    sqlx::query("INSERT INTO spots (id,status,x,y,observation_timestamp) VALUES ($1,'free',10,0,NOW() - INTERVAL '30 seconds')")
        .bind(spot_id)
        .execute(f.state.installation.pool())
        .await
        .unwrap();
    let mut graph = ParkingGraph::new();
    let camera = graph.add_node("cam-01", 0.0, 0.0).unwrap();
    graph.mark_entrance("cam-01");
    let ordinary = graph.add_node(&f.spot_id, 1.0, 0.0).unwrap();
    let preferred = graph.add_node(spot_id, 10.0, 0.0).unwrap();
    graph.add_edge(camera, ordinary, 1, true);
    graph.add_edge(camera, preferred, 10, true);
    *f.state.installation.graph.write().await = graph;
}

#[sqlx::test(migrations = "./migrations")]
async fn r08_valid_priority_still_takes_precedence_over_distance(pool: PgPool) {
    let f = recommendation_fixture(pool).await;
    add_preferred_spot(&f, "KEPT-1").await;
    sqlx::query("UPDATE spots SET reserved_for = 'pcd' WHERE id = 'KEPT-1'")
        .execute(f.state.installation.pool())
        .await
        .unwrap();
    declare(&f, f.owner, None, Some(true)).await;
    assert_recommended_spot(&f, "KEPT-1").await;
}

#[sqlx::test(migrations = "./migrations")]
async fn r08_valid_history_still_takes_precedence_over_distance(pool: PgPool) {
    let f = recommendation_fixture(pool).await;
    add_preferred_spot(&f, "B-07").await;
    for _ in 0..3 {
        sqlx::query("INSERT INTO user_occupancy_history (user_id,spot_id) VALUES ($1,'B-07')")
            .bind(f.owner)
            .execute(f.state.installation.pool())
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

#[sqlx::test(migrations = "./migrations")]
async fn server_ignores_client_expiry_and_uses_its_hold_period(
    pool: PgPool,
) -> Result<(), sqlx::Error> {
    let fixture = Fixture::new(pool).await?;
    let response = reservations::create_reservation(
        State(fixture.state.clone()),
        fixture.headers(fixture.owner, "user").await,
        Json(CreateReservation {
            spot_id: fixture.spot_id.clone(),
            expires_at: Some(
                chrono::DateTime::parse_from_rfc3339("2100-01-01T00:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
            ),
        }),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::OK);
    let (created, expires): (chrono::DateTime<Utc>, chrono::DateTime<Utc>) = sqlx::query_as(
        "SELECT created_at,expires_at FROM reservations WHERE spot_id=$1 AND status='active'",
    )
    .bind(&fixture.spot_id)
    .fetch_one(fixture.state.installation.pool())
    .await?;
    assert!((expires - created).num_seconds() <= 900);
    assert!((expires - created).num_seconds() >= 899);
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn concurrent_same_user_cannot_reserve_two_distinct_spots(
    pool: PgPool,
) -> Result<(), sqlx::Error> {
    let fixture = Fixture::new(pool).await?;
    sqlx::query("INSERT INTO spots (id,status,x,y,observation_timestamp) VALUES ('policy-second','free',0,0,NOW() - INTERVAL '30 seconds')")
        .execute(fixture.state.installation.pool())
        .await?;
    let first = reservations::create_reservation(
        State(fixture.state.clone()),
        fixture.headers(fixture.owner, "user").await,
        Json(CreateReservation {
            spot_id: fixture.spot_id.clone(),
            expires_at: Some(Utc::now() + Duration::minutes(15)),
        }),
    );
    let second = reservations::create_reservation(
        State(fixture.state.clone()),
        fixture.headers(fixture.owner, "user").await,
        Json(CreateReservation {
            spot_id: "policy-second".into(),
            expires_at: Some(Utc::now() + Duration::minutes(15)),
        }),
    );
    let (first, second) = tokio::join!(first, second);
    let mut statuses = [
        first.into_response().status().as_u16(),
        second.into_response().status().as_u16(),
    ];
    statuses.sort();
    assert_eq!(statuses, [200, 409]);
    let (active,free): (i64,i64) = sqlx::query_as("SELECT (SELECT count(*) FROM reservations WHERE user_id=$1 AND status='active'),(SELECT count(*) FROM spots WHERE id IN ($2,'policy-second') AND status='free')").bind(fixture.owner).bind(&fixture.spot_id).fetch_one(fixture.state.installation.pool()).await?;
    assert_eq!((active, free), (1, 1));
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn extension_cannot_revive_expired_reservation(pool: PgPool) -> Result<(), sqlx::Error> {
    let fixture = Fixture::new(pool).await?;
    let id = fixture.create_for_owner().await;
    sqlx::query("UPDATE reservations SET expires_at=NOW()-INTERVAL '1 second' WHERE id=$1")
        .bind(id)
        .execute(fixture.state.installation.pool())
        .await?;
    let response = reservations::extend_reservation(
        State(fixture.state.clone()),
        Path(id),
        fixture.headers(fixture.owner, "user").await,
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn policy_can_allow_two_reservations_without_a_global_user_constraint(
    pool: PgPool,
) -> Result<(), sqlx::Error> {
    let fixture = Fixture::new(pool).await?;
    sqlx::query("INSERT INTO spots (id,status,x,y,observation_timestamp) VALUES ('policy-second','free',0,0,NOW() - INTERVAL '30 seconds'),('policy-third','free',0,0,NOW() - INTERVAL '30 seconds')").execute(fixture.state.installation.pool()).await?;
    let policy = crate::reservation_policy::ReservationPolicy {
        max_active_per_user: 2,
        ..Default::default()
    };
    for spot in [&fixture.spot_id, "policy-second"] {
        crate::reservation_store::create(
            fixture.state.installation.pool(),
            spot,
            fixture.owner,
            &policy,
        )
        .await
        .expect("policy allows two active reservations");
    }
    assert!(matches!(
        crate::reservation_store::create(
            fixture.state.installation.pool(),
            "policy-third",
            fixture.owner,
            &policy
        )
        .await,
        Err(crate::reservation_store::CreateError::UserLimit)
    ));
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn renewal_preserves_occupied_status_and_caps_total_lifetime(
    pool: PgPool,
) -> Result<(), sqlx::Error> {
    let fixture = Fixture::new(pool).await?;
    let id = fixture.create_for_owner().await;
    fixture.update_spot("occupied").await;
    sqlx::query("UPDATE reservations SET created_at=NOW()-INTERVAL '29 minutes 50 seconds',expires_at=NOW()+INTERVAL '5 seconds' WHERE id=$1").bind(id).execute(fixture.state.installation.pool()).await?;
    let headers = fixture.headers(fixture.owner, "user").await;
    let response =
        reservations::extend_reservation(State(fixture.state.clone()), Path(id), headers)
            .await
            .into_response();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(fixture.spot_status().await?, "occupied");
    let lifetime: f64 = sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM expires_at-created_at)::float8 FROM reservations WHERE id=$1",
    )
    .bind(id)
    .fetch_one(fixture.state.installation.pool())
    .await?;
    assert_eq!(lifetime, 1800.0);
    let response = reservations::extend_reservation(
        State(fixture.state.clone()),
        Path(id),
        fixture.headers(fixture.owner, "user").await,
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn confirmation_before_observation_preserves_active_reservation(
    pool: PgPool,
) -> Result<(), sqlx::Error> {
    let fixture = Fixture::new(pool).await?;
    let id = fixture.create_for_owner().await;
    let response = reservations::confirm_occupancy(
        State(fixture.state.clone()),
        Path(id),
        fixture.headers(fixture.owner, "user").await,
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(fixture.reservation_status(id).await?, "active");
    assert_eq!(fixture.spot_status().await?, "reserved");
    let history: i64 =
        sqlx::query_scalar("SELECT count(*) FROM user_occupancy_history WHERE spot_id=$1")
            .bind(&fixture.spot_id)
            .fetch_one(fixture.state.installation.pool())
            .await?;
    assert_eq!(history, 0);
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn confirmation_rejects_expired_reservation_even_when_occupied(
    pool: PgPool,
) -> Result<(), sqlx::Error> {
    let fixture = Fixture::new(pool).await?;
    let id = fixture.create_for_owner().await;
    fixture.update_spot("occupied").await;
    sqlx::query("UPDATE reservations SET expires_at=NOW()-INTERVAL '1 second' WHERE id=$1")
        .bind(id)
        .execute(fixture.state.installation.pool())
        .await?;
    let response = reservations::confirm_occupancy(
        State(fixture.state.clone()),
        Path(id),
        fixture.headers(fixture.owner, "user").await,
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(fixture.reservation_status(id).await?, "active");
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn confirmation_records_history_once_after_observation(
    pool: PgPool,
) -> Result<(), sqlx::Error> {
    let fixture = Fixture::new(pool).await?;
    let id = fixture.create_for_owner().await;
    fixture.update_spot("occupied").await;
    for expected in [StatusCode::OK, StatusCode::NOT_FOUND] {
        let response = reservations::confirm_occupancy(
            State(fixture.state.clone()),
            Path(id),
            fixture.headers(fixture.owner, "user").await,
        )
        .await
        .into_response();
        assert_eq!(response.status(), expected);
    }
    assert_eq!(fixture.reservation_status(id).await?, "completed");
    assert_eq!(fixture.spot_status().await?, "occupied");
    let history: i64 =
        sqlx::query_scalar("SELECT count(*) FROM user_occupancy_history WHERE spot_id=$1")
            .bind(&fixture.spot_id)
            .fetch_one(fixture.state.installation.pool())
            .await?;
    assert_eq!(history, 1);
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn confirmation_rolls_back_when_history_insert_fails(
    pool: PgPool,
) -> Result<(), sqlx::Error> {
    let fixture = Fixture::new(pool).await?;
    let id = fixture.create_for_owner().await;
    fixture.update_spot("occupied").await;
    sqlx::raw_sql("CREATE FUNCTION reject_confirmation_history() RETURNS TRIGGER LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected history failure'; END; $$; CREATE TRIGGER reject_confirmation_history BEFORE INSERT ON user_occupancy_history FOR EACH ROW EXECUTE FUNCTION reject_confirmation_history();").execute(fixture.state.installation.pool()).await?;
    let response = reservations::confirm_occupancy(
        State(fixture.state.clone()),
        Path(id),
        fixture.headers(fixture.owner, "user").await,
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(fixture.reservation_status(id).await?, "active");
    assert_eq!(fixture.spot_status().await?, "occupied");
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn confirmation_and_cancellation_have_one_terminal_winner(
    pool: PgPool,
) -> Result<(), sqlx::Error> {
    let fixture = Fixture::new(pool).await?;
    let id = fixture.create_for_owner().await;
    fixture.update_spot("occupied").await;
    let (confirmation, cancellation) = tokio::join!(
        crate::reservation_store::confirm(fixture.state.installation.pool(), id, fixture.owner),
        crate::reservation_store::finish(
            fixture.state.installation.pool(),
            id,
            crate::reservation_store::FinishReason::Cancel {
                user_id: fixture.owner
            }
        )
    );
    let cancelled = cancellation?;
    let terminal = fixture.reservation_status(id).await?;
    let history: i64 =
        sqlx::query_scalar("SELECT count(*) FROM user_occupancy_history WHERE spot_id=$1")
            .bind(&fixture.spot_id)
            .fetch_one(fixture.state.installation.pool())
            .await?;
    if confirmation.is_ok() {
        assert_eq!(terminal, "completed");
        assert!(cancelled.is_none());
        assert_eq!(history, 1);
    } else {
        assert!(cancelled.is_some());
        assert_eq!(terminal, "cancelled");
        assert_eq!(history, 0);
    }
    assert_eq!(fixture.spot_status().await?, "occupied");
    Ok(())
}

#[sqlx::test(migrations = "./migrations")]
async fn admin_spot_status_rejects_values_outside_the_wire_contract(
    pool: PgPool,
) -> Result<(), sqlx::Error> {
    let fixture = Fixture::new(pool).await?;
    let response = reservations::update_spot_status(
        State(fixture.state.clone()),
        Path(fixture.spot_id.clone()),
        fixture.headers(fixture.admin, "admin").await,
        Json(UpdateSpotStatus {
            status: "hovering".into(),
        }),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(fixture.spot_status().await?, "free");
    Ok(())
}

async fn declare(f: &Fixture, user: Uuid, born: Option<chrono::NaiveDate>, pcd: Option<bool>) {
    use crate::personal_data::Field;
    let cipher = &f.state.personal_data;
    sqlx::query("UPDATE users SET date_of_birth_enc=$2, pcd_status_enc=$3 WHERE id=$1")
        .bind(user)
        .bind(cipher.encrypt(Field::UserDateOfBirth, user, &born).unwrap())
        .bind(cipher.encrypt(Field::UserPcdStatus, user, &pcd).unwrap())
        .execute(f.state.installation.pool())
        .await
        .unwrap();
}

async fn recommended_for(f: &Fixture, user: Uuid) -> (StatusCode, serde_json::Value) {
    let role = if user == f.admin { "admin" } else { "user" };
    let response = reservations::recommend_spot(
        State(f.state.clone()),
        f.headers(user, role).await,
        axum::extract::Query(reservations::RecommendQuery { user_id: user }),
    )
    .await
    .into_response();
    let status = response.status();
    let body = to_bytes(response.into_body(), 16384).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap_or_default())
}

#[sqlx::test(migrations = "./migrations")]
async fn encrypted_eligibility_selects_reserved_spots(pool: PgPool) {
    let f = Fixture::new(pool).await.unwrap();
    // The map keeps these spots; their names carry no meaning.
    sqlx::query("INSERT INTO spots (id,status,x,y,observation_timestamp,reserved_for) VALUES ('Z-PCD','free',5,0,NOW() - INTERVAL '30 seconds','pcd'),('Z-SENIOR','free',5,0,NOW() - INTERVAL '30 seconds','elderly')")
        .execute(f.state.installation.pool())
        .await
        .unwrap();
    let today = Utc::now().date_naive();
    let sixty = chrono::NaiveDate::from_ymd_opt(today.year() - 60, today.month(), today.day())
        .unwrap_or_else(|| chrono::NaiveDate::from_ymd_opt(today.year() - 60, 2, 28).unwrap());
    declare(&f, f.owner, Some(sixty), None).await;
    declare(&f, f.other, Some(sixty + Duration::days(1)), Some(true)).await;
    let mut graph = ParkingGraph::new();
    let camera = graph.add_node("cam-01", 0.0, 0.0).unwrap();
    graph.mark_entrance("cam-01");
    for spot in [f.spot_id.as_str(), "Z-PCD", "Z-SENIOR"] {
        let node = graph.add_node(spot, 1.0, 0.0).unwrap();
        graph.add_edge(camera, node, 1, true);
    }
    *f.state.installation.graph.write().await = graph;

    assert_eq!(
        recommended_for(&f, f.owner).await.1["recommended_spot"],
        "Z-SENIOR"
    );
    // One day short of 60 is not elderly; the disability declaration still applies.
    assert_eq!(
        recommended_for(&f, f.other).await.1["recommended_spot"],
        "Z-PCD"
    );
    assert_eq!(
        recommended_for(&f, f.admin).await.1["recommended_spot"],
        f.spot_id
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn an_envelope_moved_from_another_user_does_not_grant_priority(pool: PgPool) {
    let f = Fixture::new(pool).await.unwrap();
    declare(&f, f.other, None, Some(true)).await;
    sqlx::query("UPDATE users SET pcd_status_enc=(SELECT pcd_status_enc FROM users WHERE id=$2) WHERE id=$1")
        .bind(f.owner)
        .bind(f.other)
        .execute(f.state.installation.pool())
        .await
        .unwrap();
    let (status, body) = recommended_for(&f, f.owner).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body.is_null());
    assert_eq!(f.active_count().await.unwrap(), 0);
}

async fn observed(f: &Fixture, spot_id: &str, age: Option<i32>) {
    sqlx::query(
        "UPDATE spots SET observation_timestamp = NOW() - make_interval(secs => $2) WHERE id = $1",
    )
    .bind(spot_id)
    .bind(age.map(f64::from))
    .execute(f.state.installation.pool())
    .await
    .unwrap();
}

#[sqlx::test(migrations = "./migrations")]
async fn a_spot_without_a_recent_observation_cannot_be_reserved(pool: PgPool) {
    let f = Fixture::new(pool).await.unwrap();
    // Never seen by a camera, then seen too long ago: both unknown.
    for age in [
        None,
        Some(crate::freshness::DEFAULT_STALE_AFTER_SECS as i32 + 1),
    ] {
        observed(&f, &f.spot_id, age).await;
        let response = f.create(f.owner).await;
        assert_eq!(response.status(), StatusCode::CONFLICT, "age {age:?}");
        let status: String = sqlx::query_scalar("SELECT status FROM spots WHERE id = $1")
            .bind(&f.spot_id)
            .fetch_one(f.state.installation.pool())
            .await
            .unwrap();
        assert_eq!(status, "free", "a refused claim leaves the spot untouched");
    }
    observed(&f, &f.spot_id, Some(5)).await;
    assert_eq!(f.create(f.owner).await.status(), StatusCode::OK);
}

#[sqlx::test(migrations = "./migrations")]
async fn recommendation_skips_a_nearer_spot_whose_camera_went_quiet(pool: PgPool) {
    let f = recommendation_fixture(pool).await;
    sqlx::query(
        "INSERT INTO spots (id,status,x,y,observation_timestamp) \
         VALUES ('R08-FAR','free',10,0,NOW() - INTERVAL '5 seconds')",
    )
    .execute(f.state.installation.pool())
    .await
    .unwrap();
    let mut graph = ParkingGraph::new();
    let camera = graph.add_node("cam-01", 0.0, 0.0).unwrap();
    graph.mark_entrance("cam-01");
    let near = graph.add_node(&f.spot_id, 1.0, 0.0).unwrap();
    let far = graph.add_node("R08-FAR", 10.0, 0.0).unwrap();
    graph.add_edge(camera, near, 1, true);
    graph.add_edge(camera, far, 10, true);
    *f.state.installation.graph.write().await = graph;
    observed(&f, &f.spot_id, Some(600)).await;
    let response = recommendation_response(&f).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 16384).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["recommended_spot"], "R08-FAR");
}

#[sqlx::test(migrations = "./migrations")]
async fn a_spot_going_quiet_is_announced_unknown_once(pool: PgPool) {
    let f = Fixture::new(pool).await.unwrap();
    let mut updates = f.state.installation.tx.subscribe();
    let mut announced = std::collections::HashSet::new();
    let unknown = format!(
        r#"{{"type":"SPOT_UPDATE","spot_id":"{}","status":"unknown"}}"#,
        f.spot_id
    );

    observed(&f, &f.spot_id, Some(5)).await;
    assert!(
        crate::freshness::sweep(&f.state, &mut announced)
            .await
            .unwrap()
            .is_empty()
    );

    observed(&f, &f.spot_id, Some(600)).await;
    assert_eq!(
        crate::freshness::sweep(&f.state, &mut announced)
            .await
            .unwrap(),
        std::slice::from_ref(&f.spot_id)
    );
    assert_eq!(updates.recv().await.unwrap(), unknown);
    // Still quiet: nothing new to say.
    assert!(
        crate::freshness::sweep(&f.state, &mut announced)
            .await
            .unwrap()
            .is_empty()
    );

    // The camera returns, then goes quiet again: announced again.
    observed(&f, &f.spot_id, Some(5)).await;
    assert!(
        crate::freshness::sweep(&f.state, &mut announced)
            .await
            .unwrap()
            .is_empty()
    );
    observed(&f, &f.spot_id, Some(600)).await;
    assert_eq!(
        crate::freshness::sweep(&f.state, &mut announced)
            .await
            .unwrap(),
        std::slice::from_ref(&f.spot_id)
    );
    assert_eq!(updates.recv().await.unwrap(), unknown);
}

#[sqlx::test(migrations = "./migrations")]
async fn snapshots_show_unknown_but_keep_reservations(pool: PgPool) {
    let f = Fixture::new(pool).await.unwrap();
    f.create_for_owner().await;
    sqlx::query(
        "INSERT INTO spots (id,status,x,y,observation_timestamp) VALUES \
         ('R08-NEW','free',0,0,NULL),('R08-OLD','occupied',0,0,NOW() - INTERVAL '10 minutes'), \
         ('R08-SEEN','occupied',0,0,NOW() - INTERVAL '5 seconds')",
    )
    .execute(f.state.installation.pool())
    .await
    .unwrap();
    // The reserved spot's camera also went quiet: the reservation still shows.
    observed(&f, &f.spot_id, Some(600)).await;
    let rows: Vec<(String, String)> = sqlx::query_as(crate::freshness::SNAPSHOT_SQL)
        .bind(crate::freshness::stale_after_secs())
        .fetch_all(f.state.installation.pool())
        .await
        .unwrap();
    let rows: std::collections::BTreeMap<_, _> = rows.into_iter().collect();
    assert_eq!(rows["R08-NEW"], "unknown");
    assert_eq!(rows["R08-OLD"], "unknown");
    assert_eq!(rows["R08-SEEN"], "occupied");
    assert_eq!(rows[&f.spot_id], "reserved");
}

#[sqlx::test(migrations = "./migrations")]
async fn closest_spot_is_measured_from_the_nearest_of_two_entrances(pool: PgPool) {
    let f = Fixture::new(pool).await.unwrap();
    sqlx::query(
        "INSERT INTO spots (id,status,x,y,observation_timestamp) \
         VALUES ('SOUTH-1','free',100,0,NOW() - INTERVAL '30 seconds')",
    )
    .execute(f.state.installation.pool())
    .await
    .unwrap();
    let mut graph = ParkingGraph::new();
    let north = graph.add_node("cam-01", 0.0, 0.0).unwrap();
    let south = graph.add_node("entrance-1", 100.0, 0.0).unwrap();
    graph.mark_entrance("cam-01");
    graph.mark_entrance("entrance-1");
    // The fixture spot is far from the north entrance, SOUTH-1 next to the south one.
    let far = graph.add_node(&f.spot_id, 0.0, 50.0).unwrap();
    let near = graph.add_node("SOUTH-1", 100.0, 1.0).unwrap();
    graph.add_edge(north, far, 50, true);
    graph.add_edge(south, near, 1, true);
    *f.state.installation.graph.write().await = graph;
    let (status, body) = recommended_for(&f, f.owner).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["recommended_spot"], "SOUTH-1");
    assert_eq!(body["route"], serde_json::json!(["entrance-1", "SOUTH-1"]));
}

#[sqlx::test(migrations = "./migrations")]
async fn spots_kept_by_the_map_are_never_offered_to_everyone(pool: PgPool) {
    let f = Fixture::new(pool).await.unwrap();
    // Named like an ordinary spot, kept for PcD by the map; nearer than the fixture spot.
    sqlx::query(
        "INSERT INTO spots (id,status,x,y,observation_timestamp,reserved_for) \
         VALUES ('B-02','free',1,0,NOW() - INTERVAL '30 seconds','pcd')",
    )
    .execute(f.state.installation.pool())
    .await
    .unwrap();
    let mut graph = ParkingGraph::new();
    let camera = graph.add_node("cam-01", 0.0, 0.0).unwrap();
    graph.mark_entrance("cam-01");
    for (spot, cost) in [("B-02", 1), (f.spot_id.as_str(), 5)] {
        let node = graph.add_node(spot, 1.0, 0.0).unwrap();
        graph.add_edge(camera, node, cost, true);
    }
    *f.state.installation.graph.write().await = graph;
    assert_eq!(
        recommended_for(&f, f.owner).await.1["recommended_spot"],
        f.spot_id
    );
    declare(&f, f.other, None, Some(true)).await;
    assert_eq!(
        recommended_for(&f, f.other).await.1["recommended_spot"],
        "B-02"
    );
}
