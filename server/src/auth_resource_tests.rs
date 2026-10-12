use std::{
    net::SocketAddr,
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

use argon2::{Argon2, PasswordHash, PasswordVerifier};
use axum::{
    Json, Router,
    body::{Bytes, to_bytes},
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use dashmap::DashMap;
use sqlx::{PgPool, postgres::PgPoolOptions};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{RwLock, broadcast, oneshot},
    task::JoinHandle,
};
use uuid::Uuid;

use crate::{
    auth::{self, LoginRequest},
    pathfinding::ParkingGraph,
    security,
    state::{AppState, SharedState},
    users::{self, CreateUser},
};

fn fixture(pool: PgPool) -> SharedState {
    let (tx, _) = broadcast::channel(100);
    Arc::new(AppState {
        pool,
        tx,
        user_sessions: DashMap::new(),
        graph: RwLock::new(ParkingGraph::new()),
        jwt_secret: "r04-disposable-jwt-secret".to_owned(),
        plate_pepper: "r04-disposable-plate-pepper".to_owned(),
        edge_api_key: "r04-disposable-edge-key".to_owned(),
        auth_limits: Default::default(),
        last_trend_prediction: Mutex::new(None),
        site_data_dir: std::env::temp_dir().join(format!("r04-auth-{}", Uuid::new_v4())),
    })
}

async fn closed_pool() -> PgPool {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
        .expect("syntactically valid fixture URL");
    pool.close().await;
    pool
}

fn registration() -> CreateUser {
    CreateUser {
        name: "Teste Árvore".to_owned(),
        email: "fixture@example.invalid".to_owned(),
        password: "fixture-password".to_owned(),
        date_of_birth: None,
        pcd_status: None,
        plate: None,
    }
}

#[tokio::test]
async fn invalid_login_inputs_are_rejected_before_database_access() {
    let state = fixture(closed_pool().await);
    for (email, password) in [
        ("".to_owned(), "valid".to_owned()),
        ("é".repeat(256), "valid".to_owned()),
        ("fixture@example.invalid".to_owned(), "".to_owned()),
        ("fixture@example.invalid".to_owned(), "a".repeat(1025)),
        ("fixture@example.invalid".to_owned(), "é".repeat(513)),
    ] {
        let response =
            auth::login_dashboard(State(state.clone()), Json(LoginRequest { email, password }))
                .await
                .into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}

#[tokio::test]
async fn invalid_registration_inputs_are_rejected_before_database_access() {
    let state = fixture(closed_pool().await);
    for (field, value) in [
        ("name", "".to_owned()),
        ("name", "é".repeat(256)),
        ("email", "".to_owned()),
        ("email", "é".repeat(256)),
        ("password", "".to_owned()),
        ("password", "a".repeat(1025)),
        ("password", "é".repeat(513)),
        ("plate", "".to_owned()),
        ("plate", "a".repeat(33)),
        ("plate", "é".repeat(17)),
    ] {
        let mut payload = registration();
        match field {
            "name" => payload.name = value,
            "email" => payload.email = value,
            "password" => payload.password = value,
            "plate" => payload.plate = Some(value),
            _ => unreachable!("fixture field"),
        }
        let response = users::create_user(State(state.clone()), Json(payload))
            .await
            .into_response();
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "invalid {field}"
        );
    }
}

async fn json_body(response: Response) -> serde_json::Value {
    let bytes = to_bytes(response.into_body(), 65_536)
        .await
        .expect("bounded fixture response");
    serde_json::from_slice(&bytes).expect("JSON response contract")
}

#[sqlx::test(migrations = "./migrations")]
async fn registration_boundaries_preserve_hash_login_and_cookie_contracts(
    pool: PgPool,
) -> Result<(), sqlx::Error> {
    let state = fixture(pool.clone());
    for upper_boundary in [false, true] {
        let mut payload = registration();
        payload.name = if upper_boundary {
            "Á".repeat(255)
        } else {
            "Á".to_owned()
        };
        payload.email = if upper_boundary {
            "é".repeat(255)
        } else {
            "é".to_owned()
        };
        payload.password = if upper_boundary {
            "é".repeat(512)
        } else {
            "p".to_owned()
        };
        payload.plate = Some(if upper_boundary {
            "á".repeat(16)
        } else {
            "X".to_owned()
        });
        let (name, email, password, plate) = (
            payload.name.clone(),
            payload.email.clone(),
            payload.password.clone(),
            payload.plate.clone().expect("plate fixture"),
        );
        let response = users::create_user(State(state.clone()), Json(payload))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::CREATED);
        let body = json_body(response).await;
        let id = Uuid::parse_str(body["id"].as_str().expect("registration UUID"))
            .expect("valid registration UUID");
        assert_eq!(body["name"], name);
        assert_eq!(body["email"], email);
        assert_eq!(body["role"], "user");
        assert!(body.get("password_hash").is_none());

        let (hash, stored_plate): (String, String) =
            sqlx::query_as("SELECT password_hash, plate FROM users WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await?;
        assert!(hash.starts_with("$argon2id$"));
        let parsed = PasswordHash::new(&hash).expect("stored Argon2 PHC string");
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .expect("stored hash must verify independently of the login handler");
        assert_ne!(stored_plate, plate);

        if !upper_boundary {
            continue;
        }
        let response = auth::login_dashboard(
            State(state.clone()),
            Json(LoginRequest {
                email: email.clone(),
                password,
            }),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .expect("login cookie")
            .to_str()
            .expect("ASCII cookie")
            .to_owned();
        let body = json_body(response).await;
        assert_eq!(body["id"], id.to_string());
        assert_eq!(body["role"], "user");
        assert_eq!(body["name"], name);
        let token = body["token"].as_str().expect("JWT response field");
        let claims = security::verify_jwt(token, &state.jwt_secret).expect("signed login JWT");
        assert_eq!(claims.sub, id.to_string());
        assert_eq!(claims.role, "user");
        assert!(cookie.starts_with(&format!("estaciona_token={token};")));
        for attribute in [
            "Path=/",
            "HttpOnly",
            "Secure",
            "SameSite=Lax",
            "Max-Age=86400",
        ] {
            assert!(
                cookie.split("; ").any(|part| part == attribute),
                "cookie {attribute}"
            );
        }
        let response = auth::login_dashboard(
            State(state.clone()),
            Json(LoginRequest {
                email,
                password: "wrong".to_owned(),
            }),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(response.headers().get(header::SET_COOKIE).is_none());
    }
    Ok(())
}

struct HeldKdf {
    release: mpsc::Sender<()>,
    task: JoinHandle<()>,
}

impl HeldKdf {
    async fn start(state: SharedState) -> Self {
        let beginning = Instant::now();
        let (release, released) = mpsc::channel();
        let (started, running) = oneshot::channel();
        let task = tokio::spawn(async move {
            state
                .auth_limits
                .run_blocking(move || {
                    let _ = started.send(());
                    // Dropping the sender on test failure also releases the worker.
                    let _ = released.recv_timeout(Duration::from_secs(10));
                })
                .await
                .expect("fixture owns the only KDF slot");
        });
        let held = Self { release, task };
        tokio::time::timeout(Duration::from_secs(2), running)
            .await
            .expect("KDF fixture starts promptly")
            .expect("KDF worker started");
        assert!(
            beginning.elapsed() < Duration::from_secs(2),
            "starting the blocking worker must not block the async runtime"
        );
        held
    }

    async fn finish(self) {
        let _ = self.release.send(());
        self.task.await.expect("fixture worker must finish");
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn known_email_and_plate_conflicts_bypass_a_busy_kdf(
    pool: PgPool,
) -> Result<(), sqlx::Error> {
    let state = fixture(pool.clone());
    let held = HeldKdf::start(state.clone()).await;
    sqlx::query("INSERT INTO users (id, name, email, password_hash, plate) VALUES ($1, 'Existing', $2, 'unused-fixture-hash', $3)")
        .bind(Uuid::new_v4())
        .bind("existing@example.invalid")
        .bind(security::hash_plate("ABC1234", &state.plate_pepper))
        .execute(&pool).await?;
    for duplicate_plate in [false, true] {
        let mut payload = registration();
        if duplicate_plate {
            payload.plate = Some("ABC1234".to_owned());
        } else {
            payload.email = "existing@example.invalid".to_owned();
        }
        let response = tokio::time::timeout(
            Duration::from_secs(1),
            users::create_user(State(state.clone()), Json(payload)),
        )
        .await
        .expect("preflight must not wait for the KDF slot")
        .into_response();
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }
    held.finish().await;
    Ok(())
}

struct AbortTask<T>(JoinHandle<T>);

impl<T> Drop for AbortTask<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn concurrent_email_and_plate_inserts_still_return_conflict(
    pool: PgPool,
) -> Result<(), sqlx::Error> {
    let state = fixture(pool.clone());
    for duplicate_plate in [false, true] {
        let suffix = Uuid::new_v4();
        let mut payload = registration();
        payload.email = format!("contender-{suffix}@example.invalid");
        payload.plate = duplicate_plate.then(|| "RAC1234".to_owned());
        let email = payload.email.clone();
        let plate = payload
            .plate
            .as_deref()
            .map(|value| security::hash_plate(value, &state.plate_pepper));
        let mut locker = pool.begin().await?;
        sqlx::query("LOCK TABLE users IN SHARE MODE")
            .execute(&mut *locker)
            .await?;
        let candidate_state = state.clone();
        let mut contender = AbortTask(tokio::spawn(async move {
            users::create_user(State(candidate_state), Json(payload))
                .await
                .into_response()
        }));
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let waiting: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname = current_database() AND wait_event_type = 'Lock' AND query LIKE '%INSERT INTO users%')",
                ).fetch_one(&pool).await?;
                if waiting { return Ok::<(), sqlx::Error>(()); }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.expect("registration must pass preflight and reach blocked INSERT")?;
        let winner_email = if duplicate_plate {
            format!("winner-{suffix}@example.invalid")
        } else {
            email
        };
        sqlx::query("INSERT INTO users (id, name, email, password_hash, plate) VALUES ($1, 'Race Winner', $2, 'unused-fixture-hash', $3)")
            .bind(Uuid::new_v4()).bind(winner_email).bind(plate)
            .execute(&mut *locker).await?;
        locker.commit().await?;
        let response = tokio::time::timeout(Duration::from_secs(2), &mut contender.0)
            .await
            .expect("INSERT finishes after releasing the lock")
            .expect("registration worker");
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }
    Ok(())
}

struct HttpServer {
    address: SocketAddr,
    task: JoinHandle<()>,
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl HttpServer {
    async fn start(state: SharedState) -> Self {
        let router: Router = crate::auth_routes(state.clone())
            .route("/health", get(crate::health_check))
            .route("/ws/edge", get(crate::ws::ws_edge_handler))
            .route(
                "/r04-unrelated",
                post(|body: Bytes| async move { body.len().to_string() }),
            )
            .with_state(state);
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("ephemeral test listener");
        let address = listener.local_addr().expect("bound test address");
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("test HTTP server");
        });
        Self { address, task }
    }

    async fn exchange(&self, request: &[u8]) -> Vec<u8> {
        tokio::time::timeout(Duration::from_secs(2), async {
            let mut stream = TcpStream::connect(self.address).await.expect("test client");
            stream.write_all(request).await.expect("write test request");
            let mut response = Vec::new();
            stream
                .read_to_end(&mut response)
                .await
                .expect("read HTTP response");
            response
        })
        .await
        .expect("HTTP remains responsive")
    }

    async fn edge_handshake_status(&self) -> u16 {
        tokio::time::timeout(Duration::from_secs(2), async {
            let mut stream = TcpStream::connect(self.address).await.expect("WS test client");
            stream.write_all(b"GET /ws/edge HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nAuthorization: Bearer r04-disposable-edge-key\r\n\r\n")
                .await.expect("write WS handshake");
            let mut response = Vec::new();
            let mut buffer = [0; 1024];
            while !response.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let count = stream.read(&mut buffer).await.expect("read WS handshake");
                assert!(count > 0, "handshake response must precede EOF");
                response.extend_from_slice(&buffer[..count]);
                assert!(response.len() <= 8192, "bounded handshake headers");
            }
            response_status(&response)
        }).await.expect("WS remains responsive")
    }
}

fn response_status(response: &[u8]) -> u16 {
    String::from_utf8_lossy(response)
        .lines()
        .next()
        .expect("HTTP status line")
        .split_whitespace()
        .nth(1)
        .expect("HTTP status")
        .parse()
        .expect("numeric HTTP status")
}

fn post_request(path: &str, body: &[u8], chunked: bool) -> Vec<u8> {
    let framing = if chunked {
        "Transfer-Encoding: chunked".to_owned()
    } else {
        format!("Content-Length: {}", body.len())
    };
    let mut request = format!("POST {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Type: application/json\r\n{framing}\r\n\r\n").into_bytes();
    if chunked {
        request.extend_from_slice(format!("{:x}\r\n", body.len()).as_bytes());
        request.extend_from_slice(body);
        request.extend_from_slice(b"\r\n0\r\n\r\n");
    } else {
        request.extend_from_slice(body);
    }
    request
}

#[sqlx::test(migrations = "./migrations")]
async fn auth_body_limits_reject_content_length_and_chunked_without_limiting_other_routes(
    pool: PgPool,
) {
    let server = HttpServer::start(fixture(pool)).await;
    let oversized = format!(
        "{{\"email\":\"fixture@example.invalid\",\"password\":\"{}\"}}",
        "a".repeat(16_385)
    );
    for path in ["/login", "/users"] {
        for chunked in [false, true] {
            let response = server
                .exchange(&post_request(path, oversized.as_bytes(), chunked))
                .await;
            assert_eq!(response_status(&response), 413, "{path}, chunked={chunked}");
        }
    }
    let body = vec![b'a'; 32_768];
    let response = server
        .exchange(&post_request("/r04-unrelated", &body, false))
        .await;
    assert_eq!(response_status(&response), 200);
    assert!(
        response.ends_with(b"32768"),
        "unrelated route receives the complete body"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn unsupported_auth_methods_preserve_405_for_oversized_bodies(pool: PgPool) {
    let server = HttpServer::start(fixture(pool)).await;
    let body = vec![b'a'; 16_385];
    for path in ["/login", "/users"] {
        for method in ["GET", "PUT"] {
            let mut request = format!(
                "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
                body.len(),
            ).into_bytes();
            request.extend_from_slice(&body);
            let response = server.exchange(&request).await;
            assert_eq!(response_status(&response), 405, "{method} {path}");
        }
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn health_and_edge_websocket_remain_responsive_while_kdf_is_busy(pool: PgPool) {
    let state = fixture(pool);
    let server = HttpServer::start(state.clone()).await;
    let held = HeldKdf::start(state).await;
    let response = server
        .exchange(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await;
    assert_eq!(response_status(&response), 200);
    assert_eq!(server.edge_handshake_status().await, 101);
    held.finish().await;
}
