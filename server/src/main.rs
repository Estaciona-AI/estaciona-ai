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
mod pathfinding;
mod reservations;
mod security;
mod site_config;
mod state;
mod users;
mod ws;

use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect},
    routing::{get, post, put},
};
use sqlx::postgres::PgPoolOptions;
use state::{SharedState, init_state};
use tokio::net::TcpListener;
use tower_http::{
    cors::{Any, CorsLayer},
    services::ServeDir,
    trace::TraceLayer,
};

use crate::ws::messages::ServerToAppMsg;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    dotenvy::from_path("../.env").ok();
    let db_url = std::env::var("DATABASE_URL").expect("DATABASE_URL missing in .env");
    let plate_pepper =
        std::env::var("PLATE_SECRET_PEPPER").expect("PLATE_SECRET_PEPPER missing in .env");
    let jwt_secret = std::env::var("JWT_SECRET").expect("JWT_SECRET missing in .env");
    let edge_api_key = std::env::var("EDGE_API_KEY").expect("EDGE_API_KEY missing in .env");

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&db_url)
        .await
        .expect("Failed to connect to database");

    sqlx::migrate!()
        .run(&pool)
        .await
        .expect("Failed to run database migrations");

    let args: Vec<String> = std::env::args().collect();
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

    let site_data_dir = std::env::var("SITE_DATA_DIR")
        .expect("SITE_DATA_DIR required: directory containing config.json and spots_3d.json");
    let site = site_config::SiteConfig::load(site_data_dir)
        .expect("Failed to load installation maps from SITE_DATA_DIR");
    let parking_state: SharedState =
        init_state(pool, jwt_secret, plate_pepper, edge_api_key, site).await;

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .route("/health", get(health_check))
        .route("/", get(serve_index))
        .route("/app", get(serve_app))
        .route("/login", get(serve_login))
        .route("/dashboard", get(serve_dashboard))
        .route("/ws/edge", get(ws::ws_edge_handler))
        .route("/ws/app", get(ws::ws_app_handler))
        .route("/ws/dashboard", get(ws::ws_dashboard_handler))
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
        .route("/users", post(users::create_user))
        .route("/users/{id}", get(users::get_user))
        .route("/login", post(auth::login_dashboard))
        .route("/config", post(save_config))
        .with_state(parking_state.clone())
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .fallback_service(ServeDir::new("../web"));

    let listener = TcpListener::bind("0.0.0.0:8000").await.unwrap();
    tracing::info!("Server listening on {}", listener.local_addr().unwrap());

    let state_for_bg_task = parking_state;

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));

        loop {
            interval.tick().await;

            let expired_records = sqlx::query!(
                "UPDATE reservations
                SET status = 'expired'
                WHERE status = 'active' AND expires_at < NOW()
                RETURNING spot_id, user_id"
            )
            .fetch_all(&state_for_bg_task.pool)
            .await;

            if let Ok(records) = expired_records {
                for record in records {
                    let released = sqlx::query(
                        "UPDATE spots SET status = 'free', last_updated = NOW() \
                         WHERE id = $1 AND status = 'reserved'",
                    )
                    .bind(&record.spot_id)
                    .execute(&state_for_bg_task.pool)
                    .await
                    .is_ok_and(|result| result.rows_affected() > 0);

                    let expired_msg = ServerToAppMsg::ReservationExpired {
                        spot_id: record.spot_id.clone(),
                    };
                    if let Ok(json_str) = serde_json::to_string(&expired_msg)
                        && let Some(user_id) = record.user_id
                        && let Some(user_tx) = state_for_bg_task.user_sessions.get(&user_id)
                    {
                        let _ = user_tx.send(json_str);
                    }
                    if released {
                        let update_msg = ServerToAppMsg::SpotUpdate {
                            spot_id: record.spot_id,
                            status: "free".to_string(),
                        };
                        if let Ok(json_str) = serde_json::to_string(&update_msg) {
                            let _ = state_for_bg_task.tx.send(json_str);
                        }
                    }
                }
            }
        }
    });

    axum::serve(listener, app).await.unwrap();
}

async fn health_check() -> &'static str {
    "OK"
}

async fn serve_dashboard(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let mut is_admin = false;
    if let Some(cookie) = headers.get(axum::http::header::COOKIE)
        && let Ok(cookie_str) = cookie.to_str()
        && let Some(token_part) = cookie_str
            .split(';')
            .find(|s| s.trim().starts_with("estaciona_token="))
    {
        let token = token_part.trim().trim_start_matches("estaciona_token=");
        if let Ok(claims) = crate::security::verify_jwt(token, &state.jwt_secret)
            && claims.role == "admin"
        {
            is_admin = true;
        }
    }

    if is_admin {
        match std::fs::read_to_string("../web/dashboard.html") {
            Ok(content) => Ok(Html(content).into_response()),
            Err(_) => Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "File not found".to_string(),
            )),
        }
    } else {
        let redirect = if headers.get(axum::http::header::COOKIE).is_some() {
            "/app"
        } else {
            "/login"
        };
        Ok(Redirect::to(redirect).into_response())
    }
}

async fn serve_app() -> Result<impl IntoResponse, (StatusCode, String)> {
    match std::fs::read_to_string("../web/app.html") {
        Ok(content) => Ok(Html(content).into_response()),
        Err(_) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "File not found".to_string(),
        )),
    }
}

async fn serve_login() -> Result<impl IntoResponse, (StatusCode, String)> {
    match std::fs::read_to_string("../web/login.html") {
        Ok(content) => Ok(Html(content).into_response()),
        Err(_) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "File not found".to_string(),
        )),
    }
}

async fn serve_index() -> Result<impl IntoResponse, (StatusCode, String)> {
    match std::fs::read_to_string("../web/index.html") {
        Ok(content) => Ok(Html(content).into_response()),
        Err(_) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "File not found".to_string(),
        )),
    }
}

async fn save_config(
    State(state): State<SharedState>,
    headers: HeaderMap,
    axum::Json(payload): axum::Json<serde_json::Value>,
) -> Result<&'static str, (axum::http::StatusCode, String)> {
    crate::security::require_admin(&headers, &state.jwt_secret)?;
    site_config::save_parking_config(&state.site_data_dir, &payload).map_err(|error| {
        let status = if error.kind() == std::io::ErrorKind::InvalidData {
            StatusCode::BAD_REQUEST
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        };
        (status, error.to_string())
    })?;

    Ok(r#"{"status": "ok"}"#)
}
