// ==============================================================================
// Copyright (C) 2026 Guilherme Pedroza
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.
// ==============================================================================

mod auth;
mod auth_limits;
#[cfg(test)]
mod auth_resource_tests;
mod config_store;
mod contracts;
mod edge_auth;
mod edge_cli;
mod edge_delivery;
mod edge_operations;
mod edge_state;
mod lpr;
mod pathfinding;
#[cfg(test)]
mod postgres_tests;
#[cfg(test)]
mod reservation_concurrency_tests;
mod reservation_policy;
mod reservation_store;
#[cfg(test)]
mod reservation_tests;
mod reservations;
mod security;
mod sessions;
mod site_config;
mod site_graph;
mod site_reconciliation;
#[cfg(test)]
mod site_reconciliation_tests;
mod state;
mod users;
mod vehicles;
mod ws;
mod ws_origin;
mod ws_tickets;

use axum::{
    Router,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post, put},
};
use sqlx::postgres::PgPoolOptions;
use state::{SharedState, init_state};
use tokio::net::TcpListener;
use tower_http::{
    cors::{Any, CorsLayer},
    limit::RequestBodyLimitLayer,
    trace::TraceLayer,
};

use crate::ws::messages::ServerToAppMsg;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let args: Vec<String> = std::env::args().collect();
    if args.get(1).is_some_and(|arg| arg == "--export-contracts") {
        if args.len() != 3 {
            eprintln!("Usage: --export-contracts OUTPUT_DIRECTORY");
            std::process::exit(2);
        }
        if let Err(error) = contracts::export(std::path::Path::new(&args[2])) {
            eprintln!("Contract export failed: {error}");
            std::process::exit(1);
        }
        return;
    }
    dotenvy::from_path("../.env").ok();
    let db_url = std::env::var("DATABASE_URL").expect("DATABASE_URL missing in .env");

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&db_url)
        .await
        .expect("Failed to connect to database");

    sqlx::migrate!()
        .run(&pool)
        .await
        .expect("Failed to run database migrations");

    match edge_cli::dispatch(&pool, &args).await {
        Ok(true) => return,
        Ok(false) => {}
        Err(error) => {
            tracing::error!(%error, cause = ?std::error::Error::source(&error), "Gateway administration failed");
            std::process::exit(1);
        }
    }
    if args.len() >= 4 && args[1] == "--create-admin" {
        let email = &args[2];
        let plain_password = &args[3];

        let hashed = crate::security::hash_password(plain_password)
            .expect("Falha ao gerar o hash da senha via Argon2");

        sqlx::query!(
            "INSERT INTO users (id, name, email, password_hash, role) VALUES ($1, 'Admin', $2, $3, 'admin')",
            uuid::Uuid::new_v4(),
            email,
            hashed
        )
        .execute(&pool)
        .await
        .expect("Falha ao salvar administrador no banco. Ele já existe?");

        tracing::info!(
            "Administrador '{}' criado com sucesso! Servidor sendo finalizado.",
            email
        );
        return;
    }

    let plate_pepper =
        std::env::var("PLATE_SECRET_PEPPER").expect("PLATE_SECRET_PEPPER missing in .env");
    let jwt_secret = std::env::var("JWT_SECRET").expect("JWT_SECRET missing in .env");
    let site_data_dir = std::env::var("SITE_DATA_DIR")
        .expect("SITE_DATA_DIR required: directory containing config.json and spots_3d.json");
    let site = site_config::SiteConfig::load(site_data_dir)
        .expect("Failed to load installation maps from SITE_DATA_DIR");
    let lpr_setting = std::env::var("LPR_ENABLED");
    let lpr_mode = match &lpr_setting {
        Ok(value) => lpr::LprMode::parse(Some(value)),
        Err(std::env::VarError::NotPresent) => lpr::LprMode::parse(None),
        Err(_) => Err("LPR_ENABLED must be UTF-8"),
    }
    .expect("Invalid optional LPR configuration");
    let ws_origins = std::env::var("WS_ALLOWED_ORIGINS")
        .expect("WS_ALLOWED_ORIGINS missing: configure exact HTTP(S) frontend origins");
    let ws_origin_policy = ws_origin::WsOriginPolicy::parse(&ws_origins).expect(
        "WS_ALLOWED_ORIGINS must be a comma-separated list of exact canonical HTTP(S) origins",
    );
    let parking_state: SharedState = init_state(pool, jwt_secret, plate_pepper, site, lpr_mode)
        .await
        .expect("Failed to reconcile installation map");

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .route("/health", get(health_check))
        .route("/ws/edge", get(ws::ws_edge_handler))
        .route("/ws/app", get(ws_tickets::app_handler))
        .route("/ws/dashboard", get(ws_tickets::dashboard_handler))
        .route(
            "/reservations",
            post(reservations::create_reservation).get(reservations::get_reservations),
        )
        .route(
            "/reservations/{id}/cancel",
            put(reservations::cancel_reservation),
        )
        .route(
            "/reservations/{id}/confirm",
            post(reservations::confirm_occupancy),
        )
        .route(
            "/reservations/{id}/extend",
            put(reservations::extend_reservation),
        )
        .route("/reservations/recommend", get(reservations::recommend_spot))
        .route("/spots/{id}/status", put(reservations::update_spot_status))
        .merge(auth_routes(parking_state.clone()))
        .merge(ws_tickets::routes())
        .merge(vehicles::routes(lpr_mode))
        .route("/users/{id}", get(users::get_user))
        .route("/config", post(save_config))
        .with_state(parking_state.clone())
        .layer(axum::extract::Extension(ws_origin_policy))
        .layer(cors)
        .layer(TraceLayer::new_for_http().make_span_with(
            |request: &axum::http::Request<axum::body::Body>| {
                tracing::info_span!(
                    "http_request",
                    method = %request.method(),
                    path = %request_path_for_trace(request.uri()),
                )
            },
        ));

    let listener = TcpListener::bind("0.0.0.0:8000").await.unwrap();
    tracing::info!("Server listening on {}", listener.local_addr().unwrap());

    ws_tickets::spawn_cleanup(parking_state.pool.clone());
    let state_for_bg_task = parking_state;

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));

        loop {
            interval.tick().await;

            let expired_ids = sqlx::query_scalar::<_, uuid::Uuid>(
                "SELECT id FROM reservations WHERE status = 'active' AND expires_at < NOW()",
            )
            .fetch_all(&state_for_bg_task.pool)
            .await;
            let ids = match expired_ids {
                Ok(ids) => ids,
                Err(error) => {
                    tracing::error!("Failed to select expired reservations: {}", error);
                    continue;
                }
            };

            for id in ids {
                let transition = reservation_store::finish(
                    &state_for_bg_task.pool,
                    id,
                    reservation_store::FinishReason::Expired,
                )
                .await;
                match transition {
                    Ok(Some(record)) => {
                        let expired_msg = ServerToAppMsg::ReservationExpired {
                            spot_id: record.spot_id.clone(),
                        };
                        if let Ok(json) = serde_json::to_string(&expired_msg)
                            && let Some(user_id) = record.user_id
                            && let Some(user_tx) = state_for_bg_task.user_sessions.get(&user_id)
                        {
                            let _ = user_tx.try_send(json);
                        }
                        if let Some(status) = record.spot_status {
                            let update_msg = ServerToAppMsg::SpotUpdate {
                                spot_id: record.spot_id,
                                status: status.to_string(),
                            };
                            if let Ok(json) = serde_json::to_string(&update_msg) {
                                let _ = state_for_bg_task.tx.send(json);
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(error) => tracing::error!("Failed to expire reservation {}: {}", id, error),
                }
            }
        }
    });

    axum::serve(listener, app).await.unwrap();
}

fn request_path_for_trace(uri: &axum::http::Uri) -> &str {
    uri.path()
}

fn auth_routes(state: SharedState) -> Router<SharedState> {
    Router::new()
        .route("/auth/logout", post(sessions::logout))
        .route(
            "/login",
            post(auth::login_dashboard)
                .route_layer(DefaultBodyLimit::max(auth_limits::AUTH_BODY_LIMIT))
                .route_layer(RequestBodyLimitLayer::new(auth_limits::AUTH_BODY_LIMIT))
                .route_layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    auth_limits::guard_login,
                )),
        )
        .route(
            "/users",
            post(users::create_user)
                .route_layer(DefaultBodyLimit::max(auth_limits::AUTH_BODY_LIMIT))
                .route_layer(RequestBodyLimitLayer::new(auth_limits::AUTH_BODY_LIMIT))
                .route_layer(axum::middleware::from_fn_with_state(
                    state,
                    auth_limits::guard_register,
                )),
        )
}

async fn health_check() -> &'static str {
    "OK"
}

async fn save_config(
    State(state): State<SharedState>,
    headers: HeaderMap,
    axum::Json(payload): axum::Json<serde_json::Value>,
) -> Result<&'static str, (axum::http::StatusCode, String)> {
    crate::security::require_admin(&headers, &state.jwt_secret, &state.pool).await?;
    state.config_store.save(payload).await.map_err(|error| {
        let error = match error {
            config_store::ConfigWriteError::Busy => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Configuration write already in progress".to_owned(),
                );
            }
            config_store::ConfigWriteError::Storage(error) => error,
        };
        if error.kind() == std::io::ErrorKind::InvalidData {
            (StatusCode::BAD_REQUEST, error.to_string())
        } else {
            tracing::error!(%error, "Failed to persist parking configuration");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to save configuration".to_owned(),
            )
        }
    })?;

    Ok(r#"{"status": "ok"}"#)
}

#[cfg(test)]
mod tracing_tests {
    use super::request_path_for_trace;

    #[test]
    fn websocket_ticket_query_is_excluded_from_trace_path() {
        let uri = axum::http::Uri::from_static("/ws/app?ticket=secret-value");
        assert_eq!(request_path_for_trace(&uri), "/ws/app");
    }
}
