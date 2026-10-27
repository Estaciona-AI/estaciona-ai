// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

mod api_contract;
mod auth_limits;
mod config_store;
mod contracts;
mod csrf;
mod edge_auth;
mod edge_cli;
mod edge_delivery;
mod edge_operations;
mod edge_state;
mod freshness;
mod identity;
pub mod installation;
mod lpr;
mod pathfinding;
mod personal_data;
#[cfg(test)]
mod postgres_tests;
#[cfg(test)]
mod reservation_concurrency_tests;
mod reservation_policy;
mod reservation_store;
#[cfg(test)]
mod reservation_tests;
mod reservations;
mod runtime_config;
mod secret_file;
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
mod ws_admission;
mod ws_origin;
mod ws_tickets;

use axum::{
    Router,
    extract::State,
    http::{HeaderMap, Method, StatusCode, header},
    routing::{get, post, put},
};
use sqlx::postgres::PgPoolOptions;
use state::SharedState;
use tokio::net::TcpListener;
use tower_http::{
    cors::{AllowOrigin, CorsLayer},
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
    let installation_request = match installation::cli::parse(&args) {
        Ok(request) => request,
        Err(usage) => {
            eprintln!("{usage}");
            std::process::exit(2);
        }
    };
    // File-only validation does not capture environment settings or load dotenv.
    let installation_request = match installation_request {
        Some(command @ installation::cli::Command::Validate { .. }) => {
            std::process::exit(installation::cli::run(command).await);
        }
        request => request,
    };
    dotenvy::from_path("../.env").ok();
    if let Some(command) = installation_request {
        std::process::exit(installation::cli::run(command).await);
    }
    let migrate_request = args.get(1).is_some_and(|arg| arg == "--migrate");
    if migrate_request && args.len() != 2 {
        eprintln!("Usage: --migrate");
        std::process::exit(2);
    }
    let administrative =
        migrate_request || args.get(1).is_some_and(|arg| arg.starts_with("--edge-"));
    // Unknown arguments must not start the server as if they were absent.
    if args.len() > 1 && !administrative {
        if args[1] == "--create-admin" {
            eprintln!(
                "Administrators are a role at the identity provider: use tools/identity/provision.py"
            );
        } else {
            eprintln!(
                "Unknown arguments; see --migrate, --edge-*, --installation-* and --export-contracts"
            );
        }
        std::process::exit(2);
    }
    let (identity, database, runtime) = if administrative {
        let site_data_dir = match std::env::var("SITE_DATA_DIR") {
            Ok(value) if !value.is_empty() => value,
            _ => {
                eprintln!("SITE_DATA_DIR is required: directory containing installation.json");
                std::process::exit(2);
            }
        };
        let descriptor = match installation::InstallationDescriptor::load(
            &std::path::Path::new(&site_data_dir).join("installation.json"),
        ) {
            Ok(value) => value,
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(2);
            }
        };
        let database = match runtime_config::database_from_env() {
            Ok(value) => value,
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
        };
        (descriptor.identity(), database, None)
    } else {
        let config = match runtime_config::RuntimeConfig::from_env() {
            Ok(config) => config,
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(2);
            }
        };
        let personal_data_keys = match personal_data::Keyring::load(&config.personal_data_keys_file)
        {
            Ok(keys) => keys,
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(2);
            }
        };
        let package = match installation::InstallationPackage::load(&config.site_data_dir) {
            Ok(package) => package,
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(2);
            }
        };
        if let Err(message) = config
            .lpr_mode
            .ensure_allowed(package.identity().environment())
        {
            eprintln!("{message}");
            std::process::exit(2);
        }
        (
            package.identity(),
            config.database.clone(),
            Some((config, package, personal_data_keys)),
        )
    };
    let pool = match PgPoolOptions::new()
        .max_connections(5)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect_with(database)
        .await
    {
        Ok(pool) => pool,
        Err(_) => {
            eprintln!("Installation database connection failed");
            std::process::exit(1);
        }
    };
    if migrate_request {
        let result = installation::startup::migrate(&pool, &identity).await;
        pool.close().await;
        if let Err(error) = result {
            eprintln!("{error}");
            std::process::exit(1);
        }
        println!("Installation database migrated");
        return;
    }
    let verified = match installation::startup::prepare(pool.clone(), identity).await {
        Ok(verified) => verified,
        Err(error) => {
            eprintln!("{error}");
            pool.close().await;
            std::process::exit(1);
        }
    };

    match edge_cli::dispatch(verified.pool(), &args).await {
        Ok(true) => return,
        Ok(false) => {}
        Err(error) => {
            tracing::error!(%error, cause = ?std::error::Error::source(&error), "Gateway administration failed");
            std::process::exit(1);
        }
    }
    let (config, package, personal_data_keys) =
        runtime.expect("administrative commands return before runtime initialization");
    let ws_origin_policy = config.ws_origins;
    let lpr_mode = config.lpr_mode;
    freshness::configure(config.observation_stale_after_secs);
    // Accounts are created and signed in at the identity provider only. Its
    // metadata loads in the background: an outage there blocks sign-in only.
    let identity = match identity::IdentityProvider::new(config.identity) {
        Ok(identity) => identity,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    };
    let parking_state: SharedState = state::init_state_with(
        verified,
        config.jwt_secret,
        config.plate_pepper,
        personal_data_keys,
        identity,
        package,
        lpr_mode,
        config.ws_limits,
    )
    .await
    .expect("Failed to reconcile installation map");
    freshness::spawn_sweeper(parking_state.clone());

    tracing::info!(organization_id = %parking_state.installation.identity().organization_id(),
        installation_id = %parking_state.installation.identity().installation_id(),
        environment = ?parking_state.installation.identity().environment(),
        "Installation runtime ready");

    // Credentials travel in a cookie, so only this installation's apps get CORS.
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::list(ws_origin_policy.header_values()))
        .allow_credentials(true)
        .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
        .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION])
        .expose_headers([
            axum::http::HeaderName::from_static("x-request-id"),
            axum::http::HeaderName::from_static("x-estaciona-api-version"),
            axum::http::HeaderName::from_static("retry-after"),
        ]);

    let api = Router::new()
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
        .route("/auth/logout", post(sessions::logout))
        .merge(ws_tickets::routes())
        .merge(vehicles::routes(lpr_mode))
        .route("/users/{id}", get(users::get_user))
        .route("/me", get(users::me))
        .route("/me/eligibility", put(users::update_eligibility))
        .route("/config", post(save_config));
    let versioned = api
        .clone()
        .route("/capabilities", get(api_contract::capabilities))
        .route("/contracts/{*path}", get(api_contract::contract))
        .fallback(|| async { StatusCode::NOT_FOUND })
        .layer(axum::middleware::from_fn(api_contract::versioned_response));
    let app = Router::new()
        .route("/health", get(health_check))
        .route("/ws/edge", get(ws::ws_edge_handler))
        .route("/ws/app", get(ws_tickets::app_handler))
        .route("/ws/dashboard", get(ws_tickets::dashboard_handler))
        .nest("/api/v1", versioned)
        .merge(api)
        // Browser navigation and the provider's back-channel, outside the API.
        .merge(identity::routes(parking_state.clone()))
        .with_state(parking_state.clone())
        .layer(axum::middleware::from_fn_with_state(
            ws_origin_policy.clone(),
            csrf::guard,
        ))
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

    ws_tickets::spawn_cleanup(parking_state.installation.pool().clone());
    let discovery = parking_state.clone();
    tokio::spawn(async move { discovery.identity.discover_until_ready().await });
    let state_for_bg_task = parking_state;

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));

        loop {
            interval.tick().await;

            let expired_ids = sqlx::query_scalar::<_, uuid::Uuid>(
                "SELECT id FROM reservations WHERE status = 'active' AND expires_at < NOW()",
            )
            .fetch_all(state_for_bg_task.installation.pool())
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
                    state_for_bg_task.installation.pool(),
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
                            && let Some(user_tx) =
                                state_for_bg_task.installation.user_sessions.get(&user_id)
                        {
                            let _ = user_tx.try_send(json);
                        }
                        if let Some(status) = record.spot_status {
                            let update_msg = ServerToAppMsg::SpotUpdate {
                                spot_id: record.spot_id,
                                status: status.to_string(),
                            };
                            if let Ok(json) = serde_json::to_string(&update_msg) {
                                let _ = state_for_bg_task.installation.tx.send(json);
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

async fn health_check() -> &'static str {
    "OK"
}

async fn save_config(
    State(state): State<SharedState>,
    headers: HeaderMap,
    axum::Json(payload): axum::Json<serde_json::Value>,
) -> Result<&'static str, (axum::http::StatusCode, String)> {
    crate::security::require_admin(&headers, &state.jwt_secret, state.installation.pool()).await?;
    state
        .installation
        .config_store
        .save(payload)
        .await
        .map_err(|error| {
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
