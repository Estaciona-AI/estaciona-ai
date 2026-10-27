// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

use crate::{
    edge_auth::{self, AuthError, Principal},
    state::SharedState,
    ws::messages::EdgeToServerMsg,
};

pub use crate::lpr::car_detected;

pub async fn path_update(
    state: &SharedState,
    principal: &Principal,
    from: &str,
    to: &str,
    active: bool,
) -> Result<bool, AuthError> {
    legacy_projection(
        state,
        principal,
        EdgeToServerMsg::PathUpdate {
            from_node: from.into(),
            to_node: to.into(),
            is_active: active,
        },
    )
    .await
}

pub async fn forecast(
    state: &SharedState,
    principal: &Principal,
    prediction: EdgeToServerMsg,
) -> Result<bool, AuthError> {
    legacy_projection(state, principal, prediction).await
}

async fn legacy_projection(
    state: &SharedState,
    principal: &Principal,
    message: EdgeToServerMsg,
) -> Result<bool, AuthError> {
    let mut tx = edge_auth::begin_authorized(state.installation.pool(), principal).await?;
    let effect = crate::edge_state::prepare(&mut tx, state, principal, message).await?;
    let allowed = effect.outcome().0 != crate::edge_delivery::Outcome::Rejected;
    tx.commit().await?;
    effect.publish(state);
    Ok(allowed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reservation_store;
    use crate::{auth_limits::AuthLimits, edge_auth, pathfinding::ParkingGraph, state::AppState};
    use dashmap::DashMap;
    use sha2::{Digest, Sha256};
    use sqlx::PgPool;
    use std::sync::Arc;
    use tokio::sync::{broadcast, mpsc};
    use uuid::Uuid;

    struct Fixture {
        state: SharedState,
        principal: Principal,
        user: Uuid,
        nav: mpsc::Receiver<String>,
        updates: broadcast::Receiver<String>,
    }

    async fn fixture(pool: PgPool) -> Fixture {
        let verified = crate::installation::test_support::provision(&pool).await;
        for (id, x) in [("outside", 1.0f64), ("allowed", 10.0)] {
            sqlx::query("INSERT INTO spots (id,status,x,y,observation_timestamp) VALUES ($1,'free',$2,0,NOW() - INTERVAL '30 seconds')")
                .bind(id)
                .bind(x)
                .execute(&pool)
                .await
                .unwrap();
        }
        let gateway = Uuid::new_v4();
        let key = "a".repeat(64);
        sqlx::query(
            "INSERT INTO edge_gateways (id,name,credential_digest) VALUES ($1,'fixture',$2)",
        )
        .bind(gateway)
        .bind(Sha256::digest(key.as_bytes()).to_vec())
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO edge_camera_grants (gateway_id,camera_id,car_detected) VALUES ($1,'cam-01',true)")
            .bind(gateway).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO edge_camera_spots (gateway_id,camera_id,spot_id) VALUES ($1,'cam-01','allowed')")
            .bind(gateway).execute(&pool).await.unwrap();
        let principal = edge_auth::authenticate(&pool, &key).await.unwrap().unwrap();
        let user = Uuid::new_v4();
        sqlx::query("INSERT INTO users (id) VALUES ($1)")
            .bind(user)
            .execute(&pool)
            .await
            .unwrap();
        let mut binding_tx = pool.begin().await.unwrap();
        let binding = crate::vehicles::request_binding(
            &mut binding_tx,
            user,
            &crate::security::hash_plate("ABC1D23", "fixture-pepper"),
            None,
        )
        .await
        .unwrap();
        sqlx::query("UPDATE vehicle_bindings SET status='approved' WHERE id=$1")
            .bind(binding.id)
            .execute(&mut *binding_tx)
            .await
            .unwrap();
        binding_tx.commit().await.unwrap();
        let mut graph = ParkingGraph::new();
        let camera = graph.add_node("cam-01", 0.0, 0.0).unwrap();
        let outside = graph.add_node("outside", 1.0, 0.0).unwrap();
        let allowed = graph.add_node("allowed", 10.0, 0.0).unwrap();
        graph.add_edge(camera, outside, 1, true);
        graph.add_edge(camera, allowed, 10, true);
        let (tx, updates) = broadcast::channel(100);
        let (nav_tx, nav, _) = crate::ws::private_session::channel();
        let sessions = DashMap::new();
        sessions.insert(user, nav_tx);
        let mut installation = crate::installation::InstallationContext::for_unit_test(
            verified,
            graph,
            std::env::temp_dir(),
        );
        installation.lpr_mode = crate::lpr::LprMode::Enabled;
        installation.tx = tx;
        installation.user_sessions = sessions;
        let state = Arc::new(AppState {
            installation,
            auth_limits: AuthLimits::default(),
            jwt_secret: "unused".into(),
            plate_pepper: "fixture-pepper".into(),
            personal_data: crate::personal_data::PersonalDataCipher::for_unit_test(),
            identity: crate::identity::IdentityProvider::for_unit_test(),
            ws_admission: Default::default(),
        });
        Fixture {
            state,
            principal,
            user,
            nav,
            updates,
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn automatic_arrival_chooses_granted_spot_even_when_foreign_spot_is_closer(pool: PgPool) {
        let mut f = fixture(pool).await;
        car_detected(&f.state, &f.principal, "ABC1D23", "cam-01")
            .await
            .unwrap();
        let spots: Vec<(String, String)> =
            sqlx::query_as("SELECT id,status FROM spots ORDER BY id")
                .fetch_all(f.state.installation.pool())
                .await
                .unwrap();
        assert_eq!(
            spots,
            [
                ("allowed".into(), "reserved".into()),
                ("outside".into(), "free".into())
            ]
        );
        let navigation: serde_json::Value =
            serde_json::from_str(&f.nav.try_recv().unwrap()).unwrap();
        assert_eq!(
            navigation,
            serde_json::json!({"type":"NAVIGATION_START","spot_id":"allowed","route":["cam-01","allowed"]})
        );
        let update: serde_json::Value =
            serde_json::from_str(&f.updates.try_recv().unwrap()).unwrap();
        assert_eq!(
            update,
            serde_json::json!({"type":"SPOT_UPDATE","spot_id":"allowed","status":"reserved"})
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn arrival_does_not_navigate_or_create_another_reservation_for_foreign_existing_target(
        pool: PgPool,
    ) {
        let mut f = fixture(pool).await;
        crate::reservation_store::create(
            f.state.installation.pool(),
            "outside",
            f.user,
            &crate::reservation_policy::ReservationPolicy::default(),
        )
        .await
        .unwrap();
        car_detected(&f.state, &f.principal, "ABC1D23", "cam-01")
            .await
            .unwrap();
        assert!(f.nav.try_recv().is_err());
        assert!(f.updates.try_recv().is_err());
        let targets: Vec<String> =
            sqlx::query_scalar("SELECT spot_id FROM reservations WHERE status='active'")
                .fetch_all(f.state.installation.pool())
                .await
                .unwrap();
        assert_eq!(targets, ["outside"]);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn arrival_denies_forged_camera_and_removed_operation_without_effects(pool: PgPool) {
        let mut f = fixture(pool).await;
        car_detected(&f.state, &f.principal, "ABC1D23", "other-camera")
            .await
            .unwrap();
        sqlx::query("UPDATE edge_camera_grants SET car_detected=false WHERE gateway_id=$1")
            .bind(f.principal.gateway_id)
            .execute(f.state.installation.pool())
            .await
            .unwrap();
        car_detected(&f.state, &f.principal, "ABC1D23", "cam-01")
            .await
            .unwrap();
        assert!(f.nav.try_recv().is_err());
        assert!(f.updates.try_recv().is_err());
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM reservations")
            .fetch_one(f.state.installation.pool())
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn arrival_rejects_revoked_principal_before_reservation_or_navigation(pool: PgPool) {
        let mut f = fixture(pool).await;
        sqlx::query("UPDATE edge_gateways SET revoked_at=NOW() WHERE id=$1")
            .bind(f.principal.gateway_id)
            .execute(f.state.installation.pool())
            .await
            .unwrap();
        assert!(matches!(
            car_detected(&f.state, &f.principal, "ABC1D23", "cam-01").await,
            Err(AuthError::Unauthenticated)
        ));
        assert!(f.nav.try_recv().is_err());
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM reservations")
            .fetch_one(f.state.installation.pool())
            .await
            .unwrap();
        assert_eq!(count, 0);
    }
    #[sqlx::test(migrations = "./migrations")]
    async fn path_permission_is_required_and_changes_are_rechecked(pool: PgPool) {
        let f = fixture(pool).await;
        assert!(
            !path_update(&f.state, &f.principal, "cam-01", "allowed", false)
                .await
                .unwrap()
        );
        assert_eq!(
            f.state
                .installation
                .graph
                .read()
                .await
                .calculate_route("cam-01", "allowed"),
            Some(vec!["cam-01".into(), "allowed".into()])
        );
        sqlx::query("UPDATE edge_gateways SET path_update=true WHERE id=$1")
            .bind(f.principal.gateway_id)
            .execute(f.state.installation.pool())
            .await
            .unwrap();
        assert!(
            path_update(&f.state, &f.principal, "cam-01", "allowed", false)
                .await
                .unwrap()
        );
        assert!(
            f.state
                .installation
                .graph
                .read()
                .await
                .calculate_route("cam-01", "allowed")
                .is_none()
        );
        assert!(
            path_update(&f.state, &f.principal, "cam-01", "allowed", true)
                .await
                .unwrap()
        );
        sqlx::query("UPDATE edge_gateways SET path_update=false WHERE id=$1")
            .bind(f.principal.gateway_id)
            .execute(f.state.installation.pool())
            .await
            .unwrap();
        assert!(
            !path_update(&f.state, &f.principal, "cam-01", "allowed", false)
                .await
                .unwrap()
        );
        assert!(
            f.state
                .installation
                .graph
                .read()
                .await
                .calculate_route("cam-01", "allowed")
                .is_some()
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn revoked_gateway_cannot_alter_navigation_graph(pool: PgPool) {
        let f = fixture(pool).await;
        sqlx::query("UPDATE edge_gateways SET path_update=true,revoked_at=NOW() WHERE id=$1")
            .bind(f.principal.gateway_id)
            .execute(f.state.installation.pool())
            .await
            .unwrap();
        assert!(matches!(
            path_update(&f.state, &f.principal, "cam-01", "allowed", false).await,
            Err(AuthError::Unauthenticated)
        ));
        assert!(
            f.state
                .installation
                .graph
                .read()
                .await
                .calculate_route("cam-01", "allowed")
                .is_some()
        );
    }
    fn prediction() -> EdgeToServerMsg {
        serde_json::from_value(serde_json::json!({
            "type":"TREND_PREDICTION", "timestamp":"2026-10-13T18:00:00Z",
            "avg_stay_duration_mins":85.0,"stay_duration_distribution":[1,2],"max_capacity":400,
            "model_health":{"r2_score":0.95,"mae":2.0,"rmse":3.0,"inference_time_ms":50.0},
            "next_24h_occupancy":[{"timestamp":"2026-10-13T19:00:00Z","occupancy":200}]
        }))
        .unwrap()
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn forecast_permission_protects_cache_and_broadcast_before_and_after_grant_removal(
        pool: PgPool,
    ) {
        let mut f = fixture(pool).await;
        assert!(
            !forecast(&f.state, &f.principal, prediction())
                .await
                .unwrap()
        );
        assert!(
            f.state
                .installation
                .last_trend_prediction
                .lock()
                .unwrap()
                .is_none()
        );
        assert!(f.updates.try_recv().is_err());
        sqlx::query("UPDATE edge_gateways SET trend_prediction=true WHERE id=$1")
            .bind(f.principal.gateway_id)
            .execute(f.state.installation.pool())
            .await
            .unwrap();
        assert!(
            forecast(&f.state, &f.principal, prediction())
                .await
                .unwrap()
        );
        let cached = f
            .state
            .installation
            .last_trend_prediction
            .lock()
            .unwrap()
            .clone()
            .unwrap();
        let json: serde_json::Value = serde_json::from_str(&cached).unwrap();
        assert_eq!(json["type"], "TREND_PREDICTION");
        assert_eq!(json["max_capacity"], 400);
        assert_eq!(json["next_24h_occupancy"][0]["occupancy"], 200);
        assert_eq!(f.updates.try_recv().unwrap(), cached);
        sqlx::query("UPDATE edge_gateways SET trend_prediction=false WHERE id=$1")
            .bind(f.principal.gateway_id)
            .execute(f.state.installation.pool())
            .await
            .unwrap();
        assert!(
            !forecast(&f.state, &f.principal, prediction())
                .await
                .unwrap()
        );
        assert_eq!(
            f.state
                .installation
                .last_trend_prediction
                .lock()
                .unwrap()
                .as_deref(),
            Some(cached.as_str())
        );
        assert!(f.updates.try_recv().is_err());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn revoked_gateway_cannot_replace_forecast_or_broadcast(pool: PgPool) {
        let mut f = fixture(pool).await;
        sqlx::query("UPDATE edge_gateways SET trend_prediction=true,revoked_at=NOW() WHERE id=$1")
            .bind(f.principal.gateway_id)
            .execute(f.state.installation.pool())
            .await
            .unwrap();
        assert!(matches!(
            forecast(&f.state, &f.principal, prediction()).await,
            Err(AuthError::Unauthenticated)
        ));
        assert!(
            f.state
                .installation
                .last_trend_prediction
                .lock()
                .unwrap()
                .is_none()
        );
        assert!(f.updates.try_recv().is_err());
    }

    async fn assert_reserved_navigation_unavailable(pool: PgPool, missing: &str) {
        let mut f = fixture(pool).await;
        let reservation = reservation_store::create(
            f.state.installation.pool(),
            "allowed",
            f.user,
            &crate::reservation_policy::ReservationPolicy::default(),
        )
        .await
        .unwrap();
        let mut graph = ParkingGraph::new();
        let camera = (missing != "camera").then(|| graph.add_node("cam-01", 0.0, 0.0).unwrap());
        let target =
            (missing != "destination").then(|| graph.add_node("allowed", 10.0, 0.0).unwrap());
        if let (Some(camera), Some(target)) = (camera, target) {
            graph.add_edge(camera, target, 10, true);
            graph.set_edge_status("cam-01", "allowed", false);
        }
        *f.state.installation.graph.write().await = graph;
        assert!(
            car_detected(&f.state, &f.principal, "ABC1D23", "cam-01")
                .await
                .unwrap()
        );
        let notice: serde_json::Value = serde_json::from_str(&f.nav.try_recv().unwrap()).unwrap();
        assert_eq!(
            notice,
            serde_json::json!({
                "type":"NAVIGATION_UNAVAILABLE", "spot_id":"allowed", "reason":"route_unavailable"
            })
        );
        assert!(f.nav.try_recv().is_err());
        assert!(f.updates.try_recv().is_err());
        let reservations: Vec<(Uuid, Uuid, String, String)> =
            sqlx::query_as("SELECT id,user_id,spot_id,status FROM reservations ORDER BY id")
                .fetch_all(f.state.installation.pool())
                .await
                .unwrap();
        assert_eq!(
            reservations,
            vec![(reservation.id, f.user, "allowed".into(), "active".into())]
        );
        let statuses: Vec<(String, String)> =
            sqlx::query_as("SELECT id,status FROM spots ORDER BY id")
                .fetch_all(f.state.installation.pool())
                .await
                .unwrap();
        assert_eq!(
            statuses,
            vec![
                ("allowed".into(), "reserved".into()),
                ("outside".into(), "free".into())
            ]
        );
        // Restoring the route resumes navigation to the very same reservation.
        let mut graph = ParkingGraph::new();
        let camera = graph.add_node("cam-01", 0.0, 0.0).unwrap();
        let target = graph.add_node("allowed", 10.0, 0.0).unwrap();
        graph.add_edge(camera, target, 10, true);
        *f.state.installation.graph.write().await = graph;
        car_detected(&f.state, &f.principal, "ABC1D23", "cam-01")
            .await
            .unwrap();
        let notice: serde_json::Value = serde_json::from_str(&f.nav.try_recv().unwrap()).unwrap();
        assert_eq!(
            notice,
            serde_json::json!({"type":"NAVIGATION_START","spot_id":"allowed","route":["cam-01","allowed"]})
        );
        assert!(f.updates.try_recv().is_err());
        let ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM reservations")
            .fetch_all(f.state.installation.pool())
            .await
            .unwrap();
        assert_eq!(ids, vec![reservation.id]);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn reserved_arrival_reports_blocked_route_and_preserves_reservation(pool: PgPool) {
        assert_reserved_navigation_unavailable(pool, "blocked").await;
    }
    #[sqlx::test(migrations = "./migrations")]
    async fn reserved_arrival_reports_missing_camera_without_fabricating_route(pool: PgPool) {
        assert_reserved_navigation_unavailable(pool, "camera").await;
    }
    #[sqlx::test(migrations = "./migrations")]
    async fn reserved_arrival_reports_missing_destination_without_fabricating_route(pool: PgPool) {
        assert_reserved_navigation_unavailable(pool, "destination").await;
    }
    #[sqlx::test(migrations = "./migrations")]
    async fn retired_granted_spot_is_not_a_destination_for_automatic_arrival(pool: PgPool) {
        let mut f = fixture(pool).await;
        sqlx::query("UPDATE spots SET is_active=FALSE WHERE id='allowed'")
            .execute(f.state.installation.pool())
            .await
            .unwrap();
        assert!(
            car_detected(&f.state, &f.principal, "ABC1D23", "cam-01")
                .await
                .unwrap()
        );
        assert!(f.nav.try_recv().is_err());
        assert!(f.updates.try_recv().is_err());
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM reservations")
                .fetch_one(f.state.installation.pool())
                .await
                .unwrap(),
            0
        );
    }

    async fn assert_arrival_database_failure(pool: PgPool, stage: &str) {
        let mut f = fixture(pool).await;
        let existing = matches!(stage, "reservation" | "grant");
        if existing {
            crate::reservation_store::create(
                f.state.installation.pool(),
                "allowed",
                f.user,
                &crate::reservation_policy::ReservationPolicy::default(),
            )
            .await
            .unwrap();
        }
        let (table, column) = match stage {
            "user" => ("vehicles", "plate_hash"),
            "reservation" => ("reservations", "user_id"),
            "grant" => ("edge_camera_spots", "spot_id"),
            "free" => ("spots", "is_active"),
            _ => panic!("unknown arrival failure stage"),
        };
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "ALTER TABLE {table} RENAME COLUMN {column} TO r08_unavailable"
        )))
        .execute(f.state.installation.pool())
        .await
        .unwrap();
        let result = car_detected(&f.state, &f.principal, "ABC1D23", "cam-01").await;
        match result {
            Err(AuthError::Database(error)) => assert_eq!(
                error
                    .as_database_error()
                    .and_then(|error| error.code())
                    .as_deref(),
                Some("42703"),
                "the intended lookup must fail, not authentication",
            ),
            other => panic!("expected a typed database error, got {other:?}"),
        }
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "ALTER TABLE {table} RENAME COLUMN r08_unavailable TO {column}"
        )))
        .execute(f.state.installation.pool())
        .await
        .unwrap();
        let reservations: Vec<(String, String)> =
            sqlx::query_as("SELECT spot_id,status FROM reservations ORDER BY spot_id")
                .fetch_all(f.state.installation.pool())
                .await
                .unwrap();
        let expected = if existing {
            vec![("allowed".into(), "active".into())]
        } else {
            vec![]
        };
        assert_eq!(reservations, expected);
        let spots: Vec<(String, String)> =
            sqlx::query_as("SELECT id,status FROM spots ORDER BY id")
                .fetch_all(f.state.installation.pool())
                .await
                .unwrap();
        assert_eq!(
            spots,
            [
                (
                    "allowed".into(),
                    if existing { "reserved" } else { "free" }.into()
                ),
                ("outside".into(), "free".into()),
            ]
        );
        assert!(
            f.nav.try_recv().is_err(),
            "unknown data must not produce navigation"
        );
        assert!(
            f.updates.try_recv().is_err(),
            "uncommitted data must not be broadcast"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn r08_arrival_user_lookup_failure_has_no_effects(pool: PgPool) {
        assert_arrival_database_failure(pool, "user").await;
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn r08_arrival_reservation_lookup_failure_preserves_existing_claim(pool: PgPool) {
        assert_arrival_database_failure(pool, "reservation").await;
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn r08_arrival_grant_lookup_failure_preserves_existing_claim(pool: PgPool) {
        assert_arrival_database_failure(pool, "grant").await;
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn r08_arrival_free_spots_lookup_failure_has_no_effects(pool: PgPool) {
        assert_arrival_database_failure(pool, "free").await;
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn r09_full_private_queue_does_not_undo_committed_arrival_or_its_broadcast(pool: PgPool) {
        let mut f = fixture(pool).await;
        let session = f
            .state
            .installation
            .user_sessions
            .get(&f.user)
            .unwrap()
            .clone();
        for _ in 0..32 {
            session.try_send("queued".into()).unwrap();
        }
        assert!(
            car_detected(&f.state, &f.principal, "ABC1D23", "cam-01")
                .await
                .unwrap()
        );
        assert_eq!(
            session.try_send("late".into()),
            Err(crate::ws::private_session::PrivateSendError::Closed)
        );
        let claims: Vec<(String, String)> =
            sqlx::query_as("SELECT spot_id,status FROM reservations WHERE user_id=$1")
                .bind(f.user)
                .fetch_all(f.state.installation.pool())
                .await
                .unwrap();
        assert_eq!(claims, [("allowed".into(), "active".into())]);
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT status FROM spots WHERE id='allowed'")
                .fetch_one(f.state.installation.pool())
                .await
                .unwrap(),
            "reserved"
        );
        let update: serde_json::Value =
            serde_json::from_str(&f.updates.try_recv().unwrap()).unwrap();
        assert_eq!(
            update,
            serde_json::json!({"type":"SPOT_UPDATE","spot_id":"allowed","status":"reserved"})
        );
        for _ in 0..32 {
            assert_eq!(f.nav.try_recv().unwrap(), "queued");
        }
        assert!(
            f.nav.try_recv().is_err(),
            "navigation must not grow an already full queue"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn r12_disabled_lpr_is_inert_even_for_an_ungranted_camera(pool: PgPool) {
        let mut f = fixture(pool).await;
        Arc::get_mut(&mut f.state).unwrap().installation.lpr_mode = crate::lpr::LprMode::Disabled;
        for camera in ["cam-01", "ungranted-camera"] {
            assert!(
                car_detected(&f.state, &f.principal, "ABC1D23", camera)
                    .await
                    .unwrap()
            );
        }
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM reservations")
                .fetch_one(f.state.installation.pool())
                .await
                .unwrap(),
            0
        );
        assert!(f.nav.try_recv().is_err());
        assert!(f.updates.try_recv().is_err());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn r12_pending_vehicle_does_not_identify_the_declaring_account(pool: PgPool) {
        let mut f = fixture(pool).await;
        sqlx::query("UPDATE vehicle_bindings SET status='pending'")
            .execute(f.state.installation.pool())
            .await
            .unwrap();
        assert!(
            car_detected(&f.state, &f.principal, "ABC1D23", "cam-01")
                .await
                .unwrap()
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM reservations")
                .fetch_one(f.state.installation.pool())
                .await
                .unwrap(),
            0
        );
        assert!(f.nav.try_recv().is_err());
        assert!(f.updates.try_recv().is_err());
    }
    fn restart_graph() -> ParkingGraph {
        let mut graph = ParkingGraph::new();
        let camera = graph.add_node("cam-01", 0.0, 0.0).unwrap();
        let outside = graph.add_node("outside", 1.0, 0.0).unwrap();
        let allowed = graph.add_node("allowed", 10.0, 0.0).unwrap();
        graph.add_edge(camera, outside, 1, true);
        graph.add_edge(camera, allowed, 10, true);
        graph
    }

    async fn grant_projections(f: &Fixture) {
        sqlx::query("UPDATE edge_gateways SET path_update=true,trend_prediction=true WHERE id=$1")
            .bind(f.principal.gateway_id)
            .execute(f.state.installation.pool())
            .await
            .unwrap();
    }

    fn path_event(active: bool) -> serde_json::Value {
        serde_json::json!({"type":"PATH_UPDATE", "from_node":"cam-01", "to_node":"allowed", "is_active":active})
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn r02b_path_receipt_restart_and_map_revision_are_consistent(pool: PgPool) {
        let f = fixture(pool).await;
        grant_projections(&f).await;
        let id = Uuid::new_v4();
        let result =
            crate::edge_delivery::apply_with_state(&f.state, &f.principal, id, path_event(false))
                .await
                .unwrap();
        assert_eq!(result.ack.outcome, crate::edge_delivery::Outcome::Applied);
        let mut graph = restart_graph();
        crate::edge_state::restore(f.state.installation.pool(), &mut graph)
            .await
            .unwrap();
        assert!(graph.calculate_route("cam-01", "allowed").is_none());
        let mut changed = restart_graph();
        changed.add_node("new-map-node", 3.0, 4.0).unwrap();
        crate::edge_state::restore(f.state.installation.pool(), &mut changed)
            .await
            .unwrap();
        assert!(changed.calculate_route("cam-01", "allowed").is_some());
        crate::edge_delivery::apply_with_state(
            &f.state,
            &f.principal,
            Uuid::new_v4(),
            path_event(true),
        )
        .await
        .unwrap();
        sqlx::query("UPDATE edge_gateways SET path_update=false WHERE id=$1")
            .bind(f.principal.gateway_id)
            .execute(f.state.installation.pool())
            .await
            .unwrap();
        let duplicate =
            crate::edge_delivery::apply_with_state(&f.state, &f.principal, id, path_event(false))
                .await
                .unwrap();
        assert!(duplicate.ack.duplicate);
        assert!(
            f.state
                .installation
                .graph
                .read()
                .await
                .calculate_route("cam-01", "allowed")
                .is_some()
        );
        crate::edge_state::restore(f.state.installation.pool(), &mut graph)
            .await
            .unwrap();
        assert!(graph.calculate_route("cam-01", "allowed").is_some());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn r02b_receipt_failure_rolls_back_projection_before_publication(pool: PgPool) {
        let f = fixture(pool).await;
        grant_projections(&f).await;
        sqlx::raw_sql("CREATE FUNCTION fail_receipt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected receipt failure'; END $$; CREATE TRIGGER fail_receipt BEFORE UPDATE ON edge_event_receipts FOR EACH ROW EXECUTE FUNCTION fail_receipt();")
            .execute(f.state.installation.pool()).await.unwrap();
        let id = Uuid::new_v4();
        assert!(
            crate::edge_delivery::apply_with_state(&f.state, &f.principal, id, path_event(false))
                .await
                .is_err()
        );
        assert!(
            f.state
                .installation
                .graph
                .read()
                .await
                .calculate_route("cam-01", "allowed")
                .is_some()
        );
        let count: i64 = sqlx::query_scalar("SELECT (SELECT COUNT(*) FROM edge_path_state)+(SELECT COUNT(*) FROM edge_event_receipts)")
            .fetch_one(f.state.installation.pool()).await.unwrap();
        assert_eq!(count, 0);
        sqlx::query("DROP TRIGGER fail_receipt ON edge_event_receipts")
            .execute(f.state.installation.pool())
            .await
            .unwrap();
        assert_eq!(
            crate::edge_delivery::apply_with_state(&f.state, &f.principal, id, path_event(false))
                .await
                .unwrap()
                .ack
                .outcome,
            crate::edge_delivery::Outcome::Applied
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn r02b_forecast_survives_restart_without_republishing_duplicates(pool: PgPool) {
        let mut f = fixture(pool).await;
        grant_projections(&f).await;
        let id = Uuid::new_v4();
        let payload = serde_json::to_value(prediction()).unwrap();
        let result =
            crate::edge_delivery::apply_with_state(&f.state, &f.principal, id, payload.clone())
                .await
                .unwrap();
        assert_eq!(result.ack.outcome, crate::edge_delivery::Outcome::Applied);
        let first = f.updates.try_recv().unwrap();
        let restored =
            crate::edge_state::restore(f.state.installation.pool(), &mut restart_graph())
                .await
                .unwrap();
        assert_eq!(restored, Some(first));
        let replay = crate::edge_delivery::apply_with_state(&f.state, &f.principal, id, payload)
            .await
            .unwrap();
        assert!(replay.ack.duplicate);
        assert!(f.updates.try_recv().is_err());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn r02b_concurrent_paths_project_the_last_committed_value(pool: PgPool) {
        let f = fixture(pool).await;
        grant_projections(&f).await;
        let (a, b) = tokio::join!(
            crate::edge_delivery::apply_with_state(
                &f.state,
                &f.principal,
                Uuid::new_v4(),
                path_event(false)
            ),
            crate::edge_delivery::apply_with_state(
                &f.state,
                &f.principal,
                Uuid::new_v4(),
                path_event(true)
            )
        );
        assert_eq!(
            a.unwrap().ack.outcome,
            crate::edge_delivery::Outcome::Applied
        );
        assert_eq!(
            b.unwrap().ack.outcome,
            crate::edge_delivery::Outcome::Applied
        );
        let active: bool = sqlx::query_scalar("SELECT is_active FROM edge_path_state")
            .fetch_one(f.state.installation.pool())
            .await
            .unwrap();
        assert_eq!(
            f.state
                .installation
                .graph
                .read()
                .await
                .calculate_route("cam-01", "allowed")
                .is_some(),
            active
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn r02b_disabled_lpr_is_explicit_and_does_not_create_reservations(pool: PgPool) {
        let mut f = fixture(pool).await;
        Arc::get_mut(&mut f.state).unwrap().installation.lpr_mode = crate::lpr::LprMode::Disabled;
        let event = serde_json::json!({"type":"CAR_DETECTED", "plate":"ABC1D23", "camera_id":"cam-01", "timestamp":"2026-10-18T00:00:00Z"});
        let ack =
            crate::edge_delivery::apply_with_state(&f.state, &f.principal, Uuid::new_v4(), event)
                .await
                .unwrap()
                .ack;
        assert_eq!(ack.outcome, crate::edge_delivery::Outcome::Ignored);
        assert_eq!(ack.reason.as_deref(), Some("lpr_disabled"));
        let stored: String = sqlx::query_scalar("SELECT payload::text FROM edge_event_receipts")
            .fetch_one(f.state.installation.pool())
            .await
            .unwrap();
        assert!(
            !stored.contains("ABC1D23"),
            "inactive LPR must not archive raw plates"
        );
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM reservations")
            .fetch_one(f.state.installation.pool())
            .await
            .unwrap();
        assert_eq!(count, 0);
    }
    #[sqlx::test(migrations = "./migrations")]
    async fn r02b_forecast_publication_cannot_overtake_an_earlier_commit(pool: PgPool) {
        let f = fixture(pool).await;
        grant_projections(&f).await;
        let mut first_tx = edge_auth::begin_authorized(f.state.installation.pool(), &f.principal)
            .await
            .unwrap();
        let first = crate::edge_state::prepare(&mut first_tx, &f.state, &f.principal, prediction())
            .await
            .unwrap();
        first_tx.commit().await.unwrap();
        // Simulate an earlier writer being descheduled after the DB commits,
        // before it can publish its cache. The next writer must wait for publication.
        let mut second_payload = serde_json::to_value(prediction()).unwrap();
        second_payload["max_capacity"] = serde_json::json!(401);
        let mut second = Box::pin(crate::edge_delivery::apply_with_state(
            &f.state,
            &f.principal,
            Uuid::new_v4(),
            second_payload,
        ));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut second)
                .await
                .is_err(),
            "later forecast overtook committed but unpublished state"
        );
        first.publish(&f.state);
        assert_eq!(
            second.await.unwrap().ack.outcome,
            crate::edge_delivery::Outcome::Applied
        );
        let stored: serde_json::Value = serde_json::from_str(
            f.state
                .installation
                .last_trend_prediction
                .lock()
                .unwrap()
                .as_ref()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(stored["max_capacity"], 401);
    }
}
