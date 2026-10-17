use chrono::{Duration, Utc};
use sqlx::Acquire;
use uuid::Uuid;

use crate::{
    edge_auth::{self, AuthError, CameraOperation, GlobalOperation, Principal},
    reservation_store::{self, CreateError},
    state::SharedState,
    ws::messages::{EdgeToServerMsg, ServerToAppMsg},
};

pub async fn car_detected(
    state: &SharedState,
    principal: &Principal,
    plate: &str,
    camera_id: &str,
) -> Result<bool, AuthError> {
    let mut tx = edge_auth::begin_authorized(&state.pool, principal).await?;
    if !edge_auth::camera_allowed(&mut tx, principal, camera_id, CameraOperation::CarDetected)
        .await?
    {
        tx.rollback().await?;
        return Ok(false);
    }
    let hashed_plate = crate::security::hash_plate(plate, &state.plate_pepper);
    let user = sqlx::query_scalar::<_, Uuid>("SELECT id FROM users WHERE plate = $1")
        .bind(hashed_plate)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(user_id) = user else {
        tx.rollback().await?;
        return Ok(true);
    };
    let reserved = sqlx::query_scalar::<_, String>(
        "SELECT spot_id FROM reservations WHERE user_id = $1 AND status = 'active' LIMIT 1",
    )
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await?;
    let (spot_id, route, newly_reserved) = if let Some(spot) = reserved {
        let granted: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM edge_camera_spots AS grants JOIN spots ON spots.id=grants.spot_id WHERE gateway_id = $1 AND spot_id = $2 AND spots.is_active)",
        )
        .bind(principal.gateway_id)
        .bind(&spot)
        .fetch_one(&mut *tx)
        .await?;
        if !granted {
            tx.rollback().await?;
            return Ok(true);
        }
        let route = state.graph.read().await.calculate_route(camera_id, &spot);
        (spot, route, false)
    } else {
        let free: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM spots WHERE status = 'free' AND is_active AND EXISTS( \
             SELECT 1 FROM edge_camera_spots WHERE gateway_id = $1 AND spot_id = spots.id)",
        )
        .bind(principal.gateway_id)
        .fetch_all(&mut *tx)
        .await?;
        let candidates = {
            let graph = state.graph.read().await;
            let mut candidates: Vec<_> = free
                .into_iter()
                .filter_map(|spot| {
                    Some((
                        graph.calculate_cost(camera_id, &spot)?,
                        spot.clone(),
                        graph.calculate_route(camera_id, &spot)?,
                    ))
                })
                .collect();
            candidates.sort_by_key(|(cost, _, _)| *cost);
            candidates
        };
        let mut claimed = None;
        for (_, spot, route) in candidates {
            // A failed candidate may abort its SQL transaction. A savepoint keeps
            // the authority lease intact while the next candidate is considered.
            let mut attempt = tx.begin().await?;
            match reservation_store::create_in_transaction(
                &mut attempt,
                &spot,
                user_id,
                Utc::now() + Duration::minutes(15),
            )
            .await
            {
                Ok(_) => {
                    attempt.commit().await?;
                    claimed = Some((spot, Some(route), true));
                    break;
                }
                Err(CreateError::Unavailable) => attempt.rollback().await?,
                Err(CreateError::Database(error)) => return Err(error.into()),
            }
        }
        let Some(claimed) = claimed else {
            tx.rollback().await?;
            return Ok(true);
        };
        claimed
    };
    // Revocation/policy replacement waits for this authority lease until commit.
    // Only committed actions may publish occupancy or private navigation.
    tx.commit().await?;
    if newly_reserved {
        let update = ServerToAppMsg::SpotUpdate {
            spot_id: spot_id.clone(),
            status: "reserved".into(),
        };
        if let Ok(json) = serde_json::to_string(&update) {
            let _ = state.tx.send(json);
        }
    }
    if let Some(session) = state.user_sessions.get(&user_id) {
        let navigation = match route {
            Some(route) => ServerToAppMsg::NavigationStart { spot_id, route },
            None => ServerToAppMsg::NavigationUnavailable {
                spot_id,
                reason: "route_unavailable".into(),
            },
        };
        if let Ok(json) = serde_json::to_string(&navigation) {
            let _ = session.send(json);
        }
    }
    Ok(true)
}
pub async fn path_update(
    state: &SharedState,
    principal: &Principal,
    from: &str,
    to: &str,
    active: bool,
) -> Result<bool, AuthError> {
    let mut tx = edge_auth::begin_authorized(&state.pool, principal).await?;
    if !edge_auth::global_allowed(&mut tx, principal, GlobalOperation::PathUpdate).await? {
        tx.rollback().await?;
        return Ok(false);
    }
    // Acquire the graph before committing authorization; cancellation/DB failure
    // cannot leave a graph edit behind. The edit after commit has no await.
    let mut graph = state.graph.write().await;
    tx.commit().await?;
    graph.set_edge_status(from, to, active);
    Ok(true)
}

pub async fn forecast(
    state: &SharedState,
    principal: &Principal,
    prediction: EdgeToServerMsg,
) -> Result<bool, AuthError> {
    let mut tx = edge_auth::begin_authorized(&state.pool, principal).await?;
    if !edge_auth::global_allowed(&mut tx, principal, GlobalOperation::TrendPrediction).await? {
        tx.rollback().await?;
        return Ok(false);
    }
    let EdgeToServerMsg::TrendPrediction {
        timestamp,
        avg_stay_duration_mins,
        stay_duration_distribution,
        max_capacity,
        model_health,
        next_24h_occupancy,
    } = prediction
    else {
        return Ok(false);
    };
    let frame = ServerToAppMsg::TrendPrediction {
        timestamp,
        avg_stay_duration_mins,
        stay_duration_distribution,
        max_capacity,
        model_health,
        next_24h_occupancy,
    };
    let json = serde_json::to_string(&frame)
        .map_err(|_| sqlx::Error::Protocol("Invalid forecast payload".into()))?;
    tx.commit().await?;
    // There is no await between committed authorization, cache replacement and broadcast.
    let mut cache = state
        .last_trend_prediction
        .lock()
        .map_err(|_| sqlx::Error::Protocol("Forecast cache unavailable".into()))?;
    *cache = Some(json.clone());
    drop(cache);
    let _ = state.tx.send(json);
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{auth_limits::AuthLimits, edge_auth, pathfinding::ParkingGraph, state::AppState};
    use chrono::{Duration, Utc};
    use dashmap::DashMap;
    use sha2::{Digest, Sha256};
    use sqlx::PgPool;
    use std::sync::{Arc, Mutex};
    use tokio::sync::{RwLock, broadcast, mpsc};
    use uuid::Uuid;

    struct Fixture {
        state: SharedState,
        principal: Principal,
        user: Uuid,
        nav: mpsc::UnboundedReceiver<String>,
        updates: broadcast::Receiver<String>,
    }

    async fn fixture(pool: PgPool) -> Fixture {
        for (id, x) in [("outside", 1.0f64), ("allowed", 10.0)] {
            sqlx::query("INSERT INTO spots (id,status,x,y) VALUES ($1,'free',$2,0)")
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
        sqlx::query("INSERT INTO users (id,name,email,password_hash,plate) VALUES ($1,'fixture',$2,'unused',$3)")
            .bind(user).bind(format!("{user}@example.invalid"))
            .bind(crate::security::hash_plate("ABC1D23", "fixture-pepper"))
            .execute(&pool).await.unwrap();
        let mut graph = ParkingGraph::new();
        let camera = graph.add_node("cam-01", 0.0, 0.0);
        let outside = graph.add_node("outside", 1.0, 0.0);
        let allowed = graph.add_node("allowed", 10.0, 0.0);
        graph.add_edge(camera, outside, 1, true);
        graph.add_edge(camera, allowed, 10, true);
        let (tx, updates) = broadcast::channel(100);
        let (nav_tx, nav) = mpsc::unbounded_channel();
        let sessions = DashMap::new();
        sessions.insert(user, nav_tx);
        let state = Arc::new(AppState {
            pool,
            auth_limits: AuthLimits::default(),
            tx,
            user_sessions: sessions,
            graph: RwLock::new(graph),
            jwt_secret: "unused".into(),
            plate_pepper: "fixture-pepper".into(),
            last_trend_prediction: Mutex::new(None),
            site_data_dir: std::env::temp_dir(),
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
                .fetch_all(&f.state.pool)
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
            &f.state.pool,
            "outside",
            f.user,
            Utc::now() + Duration::minutes(15),
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
                .fetch_all(&f.state.pool)
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
            .execute(&f.state.pool)
            .await
            .unwrap();
        car_detected(&f.state, &f.principal, "ABC1D23", "cam-01")
            .await
            .unwrap();
        assert!(f.nav.try_recv().is_err());
        assert!(f.updates.try_recv().is_err());
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM reservations")
            .fetch_one(&f.state.pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn arrival_rejects_revoked_principal_before_reservation_or_navigation(pool: PgPool) {
        let mut f = fixture(pool).await;
        sqlx::query("UPDATE edge_gateways SET revoked_at=NOW() WHERE id=$1")
            .bind(f.principal.gateway_id)
            .execute(&f.state.pool)
            .await
            .unwrap();
        assert!(matches!(
            car_detected(&f.state, &f.principal, "ABC1D23", "cam-01").await,
            Err(AuthError::Unauthenticated)
        ));
        assert!(f.nav.try_recv().is_err());
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM reservations")
            .fetch_one(&f.state.pool)
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
                .graph
                .read()
                .await
                .calculate_route("cam-01", "allowed"),
            Some(vec!["cam-01".into(), "allowed".into()])
        );
        sqlx::query("UPDATE edge_gateways SET path_update=true WHERE id=$1")
            .bind(f.principal.gateway_id)
            .execute(&f.state.pool)
            .await
            .unwrap();
        assert!(
            path_update(&f.state, &f.principal, "cam-01", "allowed", false)
                .await
                .unwrap()
        );
        assert!(
            f.state
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
            .execute(&f.state.pool)
            .await
            .unwrap();
        assert!(
            !path_update(&f.state, &f.principal, "cam-01", "allowed", false)
                .await
                .unwrap()
        );
        assert!(
            f.state
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
            .execute(&f.state.pool)
            .await
            .unwrap();
        assert!(matches!(
            path_update(&f.state, &f.principal, "cam-01", "allowed", false).await,
            Err(AuthError::Unauthenticated)
        ));
        assert!(
            f.state
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
        assert!(f.state.last_trend_prediction.lock().unwrap().is_none());
        assert!(f.updates.try_recv().is_err());
        sqlx::query("UPDATE edge_gateways SET trend_prediction=true WHERE id=$1")
            .bind(f.principal.gateway_id)
            .execute(&f.state.pool)
            .await
            .unwrap();
        assert!(
            forecast(&f.state, &f.principal, prediction())
                .await
                .unwrap()
        );
        let cached = f
            .state
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
            .execute(&f.state.pool)
            .await
            .unwrap();
        assert!(
            !forecast(&f.state, &f.principal, prediction())
                .await
                .unwrap()
        );
        assert_eq!(
            f.state.last_trend_prediction.lock().unwrap().as_deref(),
            Some(cached.as_str())
        );
        assert!(f.updates.try_recv().is_err());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn revoked_gateway_cannot_replace_forecast_or_broadcast(pool: PgPool) {
        let mut f = fixture(pool).await;
        sqlx::query("UPDATE edge_gateways SET trend_prediction=true,revoked_at=NOW() WHERE id=$1")
            .bind(f.principal.gateway_id)
            .execute(&f.state.pool)
            .await
            .unwrap();
        assert!(matches!(
            forecast(&f.state, &f.principal, prediction()).await,
            Err(AuthError::Unauthenticated)
        ));
        assert!(f.state.last_trend_prediction.lock().unwrap().is_none());
        assert!(f.updates.try_recv().is_err());
    }

    async fn assert_reserved_navigation_unavailable(pool: PgPool, missing: &str) {
        let mut f = fixture(pool).await;
        let reservation = reservation_store::create(
            &f.state.pool,
            "allowed",
            f.user,
            Utc::now() + Duration::minutes(15),
        )
        .await
        .unwrap();
        let mut graph = ParkingGraph::new();
        let camera = (missing != "camera").then(|| graph.add_node("cam-01", 0.0, 0.0));
        let target = (missing != "destination").then(|| graph.add_node("allowed", 10.0, 0.0));
        if let (Some(camera), Some(target)) = (camera, target) {
            graph.add_edge(camera, target, 10, true);
            graph.set_edge_status("cam-01", "allowed", false);
        }
        *f.state.graph.write().await = graph;
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
                .fetch_all(&f.state.pool)
                .await
                .unwrap();
        assert_eq!(
            reservations,
            vec![(reservation.id, f.user, "allowed".into(), "active".into())]
        );
        let statuses: Vec<(String, String)> =
            sqlx::query_as("SELECT id,status FROM spots ORDER BY id")
                .fetch_all(&f.state.pool)
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
        let camera = graph.add_node("cam-01", 0.0, 0.0);
        let target = graph.add_node("allowed", 10.0, 0.0);
        graph.add_edge(camera, target, 10, true);
        *f.state.graph.write().await = graph;
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
            .fetch_all(&f.state.pool)
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
            .execute(&f.state.pool)
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
                .fetch_one(&f.state.pool)
                .await
                .unwrap(),
            0
        );
    }
}
