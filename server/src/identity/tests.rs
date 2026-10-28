// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

//! The real handlers and database against an in-process OpenID provider that
//! signs with Ed25519, as the realm does.
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use axum::{
    Form, Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use openidconnect::reqwest;
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

use super::{IdentityConfig, IdentityProvider};
use crate::{pathfinding::ParkingGraph, state::AppState};

const CLIENT: &str = "campus";
const SECRET: &str = "campus-client-secret";
const APP: &str = "http://localhost:5173";

struct Grant {
    challenge: String,
    claims: Value,
}

struct Provider {
    issuer: String,
    pkcs8: Vec<u8>,
    public: Vec<u8>,
    grants: Mutex<HashMap<String, Grant>>,
}

impl Provider {
    fn sign(&self, claims: &Value) -> String {
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some("ed25519-1".into());
        jsonwebtoken::encode(&header, claims, &EncodingKey::from_ed_der(&self.pkcs8)).unwrap()
    }

    fn id_claims(&self, nonce: &str, roles: &[&str]) -> Value {
        let now = chrono::Utc::now().timestamp();
        json!({
            "iss": self.issuer, "sub": "subject-1", "aud": CLIENT, "azp": CLIENT,
            "iat": now, "exp": now + 300, "nonce": nonce,
            "preferred_username": "ea-7k3m9qx2vd", "name": "Maria Teste",
            "estaciona_roles": roles, "sid": "provider-session-1",
        })
    }

    fn logout(&self, changes: Value) -> String {
        let now = chrono::Utc::now().timestamp();
        let mut claims = json!({
            "iss": self.issuer, "aud": CLIENT, "iat": now, "exp": now + 120,
            "jti": Uuid::new_v4().to_string(), "sid": "provider-session-1",
            "events": {"http://schemas.openid.net/event/backchannel-logout": {}},
        });
        for (key, value) in changes.as_object().unwrap() {
            if value.is_null() {
                claims.as_object_mut().unwrap().remove(key);
            } else {
                claims[key] = value.clone();
            }
        }
        self.sign(&claims)
    }
}

async fn token(
    State(provider): State<Arc<Provider>>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let invalid = |error: &str| (StatusCode::BAD_REQUEST, Json(json!({"error": error})));
    let basic = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{CLIENT}:{SECRET}"))
    );
    if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some(basic.as_str()) {
        return Err(invalid("invalid_client"));
    }
    let grant = provider
        .grants
        .lock()
        .unwrap()
        .remove(form.get("code").map(String::as_str).unwrap_or_default())
        .ok_or_else(|| invalid("invalid_grant"))?;
    let verifier = form.get("code_verifier").cloned().unwrap_or_default();
    if URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())) != grant.challenge {
        return Err(invalid("invalid_grant"));
    }
    Ok(Json(json!({
        "access_token": "opaque-access-token", "token_type": "Bearer", "expires_in": 300,
        "id_token": provider.sign(&grant.claims),
    })))
}

async fn start_provider() -> Arc<Provider> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new())
        .unwrap()
        .as_ref()
        .to_vec();
    let public = Ed25519KeyPair::from_pkcs8(&pkcs8)
        .unwrap()
        .public_key()
        .as_ref()
        .to_vec();
    let provider = Arc::new(Provider {
        issuer: format!("{base}/realms/test"),
        pkcs8,
        public,
        grants: Mutex::new(HashMap::new()),
    });
    let metadata = json!({
        "issuer": provider.issuer,
        "authorization_endpoint": format!("{base}/auth"),
        "token_endpoint": format!("{base}/token"),
        "jwks_uri": format!("{base}/certs"),
        "end_session_endpoint": format!("{base}/logout"),
        "response_types_supported": ["code"],
        "subject_types_supported": ["public"],
        "id_token_signing_alg_values_supported": ["EdDSA"],
        "backchannel_logout_supported": true,
    });
    let keys = json!({"keys": [{
        "kty": "OKP", "crv": "Ed25519", "kid": "ed25519-1", "alg": "EdDSA", "use": "sig",
        "x": URL_SAFE_NO_PAD.encode(&provider.public),
    }]});
    let router = Router::new()
        .route(
            "/realms/test/.well-known/openid-configuration",
            get(move || async move { Json(metadata) }),
        )
        .route("/certs", get(move || async move { Json(keys) }))
        .route("/token", post(token))
        .with_state(provider.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    provider
}

struct Fixture {
    provider: Arc<Provider>,
    base: String,
    http: reqwest::Client,
}

async fn fixture(pool: PgPool) -> Fixture {
    let provider = start_provider().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://localhost:{}", listener.local_addr().unwrap().port());
    let config = IdentityConfig::new(
        &provider.issuer,
        CLIENT,
        SECRET.into(),
        &format!("{base}/auth/callback"),
        APP,
    )
    .unwrap();
    let identity = IdentityProvider::discover(config).await.unwrap();
    let state = Arc::new(AppState {
        installation: crate::installation::InstallationContext::for_unit_test(
            crate::installation::test_support::provision(&pool).await,
            ParkingGraph::new(),
            std::env::temp_dir(),
        ),
        auth_limits: Default::default(),
        jwt_secret: "identity-test-jwt-secret-0123456789".into(),
        plate_pepper: "identity-test-plate-pepper-0123456789".into(),
        personal_data: crate::personal_data::PersonalDataCipher::for_unit_test(),
        identity,
        ws_admission: Default::default(),
    });
    let policy = crate::ws_origin::WsOriginPolicy::parse(APP).unwrap();
    let router = Router::new()
        .route("/me", get(crate::users::me))
        .route(
            "/me/eligibility",
            axum::routing::put(crate::users::update_eligibility),
        )
        .route("/auth/logout", post(crate::sessions::logout))
        .merge(super::routes(state.clone()))
        .with_state(state.clone())
        .layer(axum::middleware::from_fn_with_state(
            policy,
            crate::csrf::guard,
        ));
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    Fixture {
        provider,
        base,
        http,
    }
}

fn cookie(response: &reqwest::Response, name: &str) -> Option<String> {
    response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find(|value| value.starts_with(&format!("{name}=")))
        .map(str::to_owned)
}

fn cookie_pair(set_cookie: &str) -> String {
    set_cookie.split(';').next().unwrap().to_owned()
}

struct Started {
    state: String,
    nonce: String,
    challenge: String,
    sign_in_cookie: String,
}

impl Fixture {
    async fn start(&self, return_to: &str) -> Started {
        let response = self
            .http
            .get(format!("{}/auth/login?return_to={return_to}", self.base))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 303);
        let location =
            reqwest::Url::parse(response.headers()["location"].to_str().unwrap()).unwrap();
        assert!(location.as_str().starts_with(&format!(
            "{}/auth?",
            self.provider.issuer.trim_end_matches("/realms/test")
        )));
        let query: HashMap<String, String> = location.query_pairs().into_owned().collect();
        assert_eq!(query["response_type"], "code");
        assert_eq!(query["client_id"], CLIENT);
        assert_eq!(
            query["redirect_uri"],
            format!("{}/auth/callback", self.base)
        );
        assert_eq!(query["code_challenge_method"], "S256");
        assert!(query["scope"].split(' ').any(|scope| scope == "openid"));
        let set_cookie = cookie(&response, "__Host-estaciona_signin").unwrap();
        for attribute in ["HttpOnly", "Secure", "SameSite=Lax", "Path=/"] {
            assert!(set_cookie.contains(attribute), "{set_cookie}");
        }
        Started {
            state: query["state"].clone(),
            nonce: query["nonce"].clone(),
            challenge: query["code_challenge"].clone(),
            sign_in_cookie: cookie_pair(&set_cookie),
        }
    }

    fn grant(&self, code: &str, started: &Started, claims: Value) {
        self.provider.grants.lock().unwrap().insert(
            code.into(),
            Grant {
                challenge: started.challenge.clone(),
                claims,
            },
        );
    }

    async fn callback(&self, code: &str, state: &str, cookie: &str) -> reqwest::Response {
        self.http
            .get(format!(
                "{}/auth/callback?code={code}&state={state}",
                self.base
            ))
            .header("cookie", cookie)
            .send()
            .await
            .unwrap()
    }

    /// Signs in with these roles and returns the session cookie pair.
    async fn sign_in(&self, roles: &[&str]) -> String {
        self.sign_in_with(roles, json!({})).await
    }

    /// Signs in with ID token claims changed, e.g. another provider session.
    async fn sign_in_with(&self, roles: &[&str], changes: Value) -> String {
        let started = self.start("/app.html").await;
        let code = Uuid::new_v4().to_string();
        let mut claims = self.provider.id_claims(&started.nonce, roles);
        for (key, value) in changes.as_object().unwrap() {
            claims[key] = value.clone();
        }
        self.grant(&code, &started, claims);
        let response = self
            .callback(&code, &started.state, &started.sign_in_cookie)
            .await;
        assert_eq!(response.status().as_u16(), 303);
        assert_eq!(response.headers()["location"], format!("{APP}/app.html"));
        cookie_pair(&cookie(&response, "__Host-estaciona_session").unwrap())
    }

    async fn me(&self, session: &str) -> reqwest::Response {
        self.http
            .get(format!("{}/me", self.base))
            .header("cookie", session)
            .send()
            .await
            .unwrap()
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn a_signed_in_account_gets_a_session_cookie_and_no_email(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    let started = f.start("/app.html").await;
    f.grant(
        "code-1",
        &started,
        f.provider.id_claims(&started.nonce, &["user"]),
    );
    let response = f
        .callback("code-1", &started.state, &started.sign_in_cookie)
        .await;
    assert_eq!(response.status().as_u16(), 303);
    assert_eq!(response.headers()["location"], format!("{APP}/app.html"));
    assert_eq!(response.headers()["cache-control"], "no-store");
    let session = cookie(&response, "__Host-estaciona_session").unwrap();
    for attribute in ["HttpOnly", "Secure", "SameSite=Lax", "Path=/"] {
        assert!(session.contains(attribute), "{session}");
    }
    assert!(!session.to_ascii_lowercase().contains("domain="));
    assert!(
        cookie(&response, "__Host-estaciona_signin")
            .unwrap()
            .contains("Max-Age=0")
    );

    let me = json_body(f.me(&cookie_pair(&session)).await).await;
    assert_eq!(me["username"], "ea-7k3m9qx2vd");
    assert_eq!(me["display_name"], "Maria Teste");
    assert_eq!(me["role"], "user");
    assert!(me.get("email").is_none());

    let row: String = sqlx::query_scalar("SELECT row_to_json(u)::text FROM users u")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(!row.contains("Maria"), "the name is stored encrypted only");
    let sid: Option<String> = sqlx::query_scalar("SELECT idp_session_id FROM auth_sessions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(sid.as_deref(), Some("provider-session-1"));
    let attempts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM oidc_login_attempts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(attempts, 0, "a sign-in attempt is single use");
}

#[sqlx::test(migrations = "./migrations")]
async fn forged_replayed_and_mismatched_returns_create_no_session(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    let started = f.start("/").await;
    let claims = f.provider.id_claims(&started.nonce, &["user"]);

    // Another browser's cookie, or none, does not complete this sign-in.
    let other = f.start("/").await;
    f.grant("a", &started, claims.clone());
    for cookie in [other.sign_in_cookie.as_str(), ""] {
        let response = f.callback("a", &started.state, cookie).await;
        assert_eq!(response.status().as_u16(), 400);
        assert!(cookie_value_absent(&response));
    }
    // A wrong nonce, audience, issuer, algorithm or expiry fails validation.
    let mut cases = Vec::new();
    for (key, value) in [
        ("nonce", json!("other-nonce")),
        ("aud", json!("other-installation")),
        ("iss", json!("https://evil.example/realms/test")),
        ("exp", json!(chrono::Utc::now().timestamp() - 60)),
    ] {
        let mut changed = claims.clone();
        changed[key] = value;
        cases.push(changed);
    }
    for (index, changed) in cases.into_iter().enumerate() {
        let started = f.start("/").await;
        let mut changed = changed;
        if changed["nonce"] != "other-nonce" {
            changed["nonce"] = json!(started.nonce);
        }
        let code = format!("bad-{index}");
        f.grant(&code, &started, changed);
        let response = f
            .callback(&code, &started.state, &started.sign_in_cookie)
            .await;
        assert_eq!(response.status().as_u16(), 400, "case {index}");
        assert!(cookie_value_absent(&response));
    }
    // A verifier that does not match the challenge is refused by the provider.
    let wrong_pkce = f.start("/").await;
    f.provider.grants.lock().unwrap().insert(
        "pkce".into(),
        Grant {
            challenge: "not-the-challenge".into(),
            claims: f.provider.id_claims(&wrong_pkce.nonce, &["user"]),
        },
    );
    let response = f
        .callback("pkce", &wrong_pkce.state, &wrong_pkce.sign_in_cookie)
        .await;
    assert_eq!(response.status().as_u16(), 400);

    // The original attempt still works once, and only once.
    let started_ok = f.start("/").await;
    f.grant(
        "ok",
        &started_ok,
        f.provider.id_claims(&started_ok.nonce, &["user"]),
    );
    let response = f
        .callback("ok", &started_ok.state, &started_ok.sign_in_cookie)
        .await;
    assert_eq!(response.status().as_u16(), 303);
    f.grant(
        "again",
        &started_ok,
        f.provider.id_claims(&started_ok.nonce, &["user"]),
    );
    let replay = f
        .callback("again", &started_ok.state, &started_ok.sign_in_cookie)
        .await;
    assert_eq!(replay.status().as_u16(), 400);
    let sessions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM auth_sessions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(sessions, 1);
}

async fn json_body(response: reqwest::Response) -> Value {
    serde_json::from_slice(&response.bytes().await.unwrap()).unwrap()
}

fn cookie_value_absent(response: &reqwest::Response) -> bool {
    cookie(response, "__Host-estaciona_session").is_none()
}

#[sqlx::test(migrations = "./migrations")]
async fn an_account_without_an_installation_role_is_refused(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    let started = f.start("/").await;
    f.grant(
        "none",
        &started,
        f.provider.id_claims(&started.nonce, &["manage-account"]),
    );
    let response = f
        .callback("none", &started.state, &started.sign_in_cookie)
        .await;
    assert_eq!(response.status().as_u16(), 403);
    assert!(cookie_value_absent(&response));
    let users: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(users, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn expired_or_unsafe_sign_in_starts_are_refused(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    for return_to in ["//evil.example", "https://evil.example", "app.html"] {
        let response = f
            .http
            .get(format!("{}/auth/login?return_to={return_to}", f.base))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 400, "{return_to}");
    }
    let started = f.start("/").await;
    sqlx::query("UPDATE oidc_login_attempts SET expires_at = NOW() - INTERVAL '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    f.grant(
        "late",
        &started,
        f.provider.id_claims(&started.nonce, &["user"]),
    );
    let response = f
        .callback("late", &started.state, &started.sign_in_cookie)
        .await;
    assert_eq!(response.status().as_u16(), 400);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_new_role_replaces_the_old_and_ends_sessions_with_it(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    let as_user = f.sign_in(&["user"]).await;
    assert_eq!(f.me(&as_user).await.status().as_u16(), 200);
    let as_admin = f.sign_in(&["user", "admin"]).await;
    let me = json_body(f.me(&as_admin).await).await;
    assert_eq!(me["role"], "admin");
    let accounts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(accounts, 1, "the provider subject identifies the account");
    assert_eq!(f.me(&as_user).await.status().as_u16(), 401);
}

#[sqlx::test(migrations = "./migrations")]
async fn sessions_ended_by_a_role_change_stay_ended_when_the_role_returns(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    for (original, other) in [("admin", "user"), ("user", "admin")] {
        let old = f.sign_in(&[original]).await;
        sqlx::query(
            "INSERT INTO ws_tickets (session_id,digest,user_id,role,destination,session_expires_at,expires_at) \
             SELECT id,sha256(convert_to(id::text,'UTF8')),user_id,role,'app',EXTRACT(EPOCH FROM expires_at)::BIGINT,NOW()+INTERVAL '1 minute' \
             FROM auth_sessions WHERE revoked_at IS NULL",
        )
        .execute(&pool)
        .await
        .unwrap();
        let changed = f.sign_in(&[other]).await;
        let tickets: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ws_tickets")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(tickets, 0, "{original}: tickets of ended sessions go too");

        let restored = f.sign_in(&[original]).await;
        let path = format!("{original}→{other}→{original}");
        assert_eq!(f.me(&old).await.status().as_u16(), 401, "{path}");
        assert_eq!(f.me(&changed).await.status().as_u16(), 401, "{path}");
        assert_eq!(f.me(&restored).await.status().as_u16(), 200, "{path}");
        let live: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM auth_sessions WHERE revoked_at IS NULL")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(live, 1, "{path}");
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn concurrent_sign_ins_with_different_roles_leave_no_stale_session(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    for round in 0..10 {
        tokio::join!(f.sign_in(&["admin"]), f.sign_in(&["user"]));
        let stale: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM auth_sessions s JOIN users u ON u.id=s.user_id \
             WHERE s.revoked_at IS NULL AND s.role<>u.role",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(stale, 0, "round {round}");
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn sessions_with_the_same_role_survive_another_sign_in(pool: PgPool) {
    let f = fixture(pool).await;
    let first_device = f.sign_in(&["user"]).await;
    let second_device = f.sign_in(&["user"]).await;
    assert_eq!(f.me(&first_device).await.status().as_u16(), 200);
    assert_eq!(f.me(&second_device).await.status().as_u16(), 200);
}

#[sqlx::test(migrations = "./migrations")]
async fn back_channel_logout_ends_the_named_session_only_when_valid(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    let session = f.sign_in(&["user"]).await;
    let post = |token: String| {
        let http = f.http.clone();
        let url = format!("{}/auth/backchannel-logout", f.base);
        async move {
            http.post(url)
                .form(&[("logout_token", token)])
                .send()
                .await
                .unwrap()
                .status()
                .as_u16()
        }
    };
    for invalid in [
        json!({"aud": "other-installation"}),
        json!({"iss": "https://evil.example/realms/test"}),
        json!({"events": {}}),
        json!({"nonce": "not-allowed"}),
        json!({"exp": chrono::Utc::now().timestamp() - 120}),
        json!({"sid": null}),
    ] {
        assert_eq!(
            post(f.provider.logout(invalid.clone())).await,
            400,
            "{invalid}"
        );
        assert_eq!(f.me(&session).await.status().as_u16(), 200, "{invalid}");
    }
    let mut unsigned = f.provider.logout(json!({}));
    unsigned.replace_range(unsigned.rfind('.').unwrap() + 1.., "AAAA");
    assert_eq!(post(unsigned).await, 400);

    assert_eq!(
        post(f.provider.logout(json!({"sid": "another-session"}))).await,
        200
    );
    assert_eq!(f.me(&session).await.status().as_u16(), 200);
    assert_eq!(post(f.provider.logout(json!({}))).await, 200);
    assert_eq!(f.me(&session).await.status().as_u16(), 401);

    // Without a session ID, the subject's sessions end. A sign-in after a
    // logout gets a new provider session, as the provider issues one.
    let second = f
        .sign_in_with(&["user"], json!({"sid": "provider-session-2"}))
        .await;
    let by_subject = f.provider.logout(json!({"sid": null, "sub": "subject-1"}));
    assert_eq!(post(by_subject).await, 200);
    assert_eq!(f.me(&second).await.status().as_u16(), 401);
}

#[sqlx::test(migrations = "./migrations")]
async fn logout_by_cookie_needs_the_app_origin_and_returns_the_provider_logout(pool: PgPool) {
    let f = fixture(pool).await;
    let session = f.sign_in(&["user"]).await;
    let logout = |origin: Option<&'static str>| {
        let mut request = f
            .http
            .post(format!("{}/auth/logout", f.base))
            .header("cookie", session.clone());
        if let Some(origin) = origin {
            request = request.header("origin", origin);
        }
        request.send()
    };
    assert_eq!(logout(None).await.unwrap().status().as_u16(), 403);
    assert_eq!(
        logout(Some("https://evil.example"))
            .await
            .unwrap()
            .status()
            .as_u16(),
        403
    );
    assert_eq!(f.me(&session).await.status().as_u16(), 200);
    let response = logout(Some(APP)).await.unwrap();
    assert_eq!(response.status().as_u16(), 200);
    assert!(
        cookie(&response, "__Host-estaciona_session")
            .unwrap()
            .contains("Max-Age=0")
    );
    let body = json_body(response).await;
    let url = reqwest::Url::parse(body["end_session_url"].as_str().unwrap()).unwrap();
    let query: HashMap<String, String> = url.query_pairs().into_owned().collect();
    assert_eq!(query["client_id"], CLIENT);
    assert_eq!(query["post_logout_redirect_uri"], format!("{APP}/"));
    assert_eq!(f.me(&session).await.status().as_u16(), 401);
}

#[sqlx::test(migrations = "./migrations")]
async fn eligibility_is_declared_by_its_owner_and_stored_encrypted(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    let session = f.sign_in(&["user"]).await;
    let declare = |origin: &'static str| {
        f.http
            .put(format!("{}/me/eligibility", f.base))
            .header("cookie", session.clone())
            .header("origin", origin)
            .header("content-type", "application/json")
            .body(r#"{"date_of_birth":"1950-01-02","pcd_status":true}"#)
            .send()
    };
    assert_eq!(
        declare("https://evil.example")
            .await
            .unwrap()
            .status()
            .as_u16(),
        403
    );
    let response = declare(APP).await.unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let me = json_body(response).await;
    assert_eq!(me["date_of_birth"], "1950-01-02");
    assert_eq!(me["pcd_status"], true);
    let row: String = sqlx::query_scalar("SELECT row_to_json(u)::text FROM users u")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(!row.contains("1950-01-02"));

    // An envelope copied from another account does not open.
    let other = Uuid::new_v4();
    let envelope = crate::personal_data::PersonalDataCipher::for_unit_test()
        .encrypt(
            crate::personal_data::Field::UserPcdStatus,
            other,
            &Some(true),
        )
        .unwrap();
    sqlx::query("UPDATE users SET pcd_status_enc = $1")
        .bind(envelope)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(f.me(&session).await.status().as_u16(), 500);
}

#[sqlx::test(migrations = "./migrations")]
async fn abandoned_sign_ins_are_cleaned_up(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    f.start("/").await;
    f.start("/").await;
    sqlx::query("UPDATE oidc_login_attempts SET expires_at = NOW() - INTERVAL '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    f.start("/").await;
    assert_eq!(crate::sessions::cleanup_expired(&pool).await.unwrap(), 2);
    let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM oidc_login_attempts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 1);
}

#[tokio::test]
async fn an_unreachable_provider_blocks_sign_in_only_until_it_answers() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let config = IdentityConfig::new(
        &format!("http://127.0.0.1:{port}/realms/test"),
        CLIENT,
        SECRET.into(),
        "http://localhost:8000/auth/callback",
        APP,
    )
    .unwrap();
    let identity = IdentityProvider::new(config).unwrap();
    assert!(identity.start().await.is_err());
    // A second attempt inside the retry interval does not reach the network.
    assert_eq!(
        identity.start().await.err(),
        Some(super::IdentityError::Discovery)
    );
    assert_eq!(identity.end_session_url().await, None);
    assert_eq!(
        identity.verify_logout("a.b.c").await.err(),
        Some(super::IdentityError::Discovery)
    );
}

async fn post_logout(f: &Fixture, token: String) -> u16 {
    f.http
        .post(format!("{}/auth/backchannel-logout", f.base))
        .form(&[("logout_token", token)])
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

async fn live_sessions(pool: &PgPool, sid: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM auth_sessions WHERE idp_session_id=$1 AND revoked_at IS NULL",
    )
    .bind(sid)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn a_logout_received_while_sign_in_completes_refuses_the_older_token(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    let started = f.start("/app.html").await;
    // The provider issued the token, then ended that session before the
    // browser came back with the code.
    f.grant(
        "in-flight",
        &started,
        f.provider.id_claims(&started.nonce, &["admin"]),
    );
    assert_eq!(post_logout(&f, f.provider.logout(json!({}))).await, 200);
    let response = f
        .callback("in-flight", &started.state, &started.sign_in_cookie)
        .await;
    assert_eq!(response.status().as_u16(), 401);
    assert!(cookie(&response, "__Host-estaciona_session").is_none());
    assert_eq!(live_sessions(&pool, "provider-session-1").await, 0);

    // A new provider session, or a token issued after the logout, signs in.
    let session = f
        .sign_in_with(&["user"], json!({"sid": "provider-session-2"}))
        .await;
    assert_eq!(f.me(&session).await.status().as_u16(), 200);
    let later = chrono::Utc::now().timestamp() + 5;
    let session = f.sign_in_with(&["user"], json!({"iat": later})).await;
    assert_eq!(f.me(&session).await.status().as_u16(), 200);
}

#[sqlx::test(migrations = "./migrations")]
async fn an_account_logout_refuses_every_token_issued_before_it(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    let started = f.start("/app.html").await;
    f.grant(
        "in-flight",
        &started,
        f.provider.id_claims(&started.nonce, &["user"]),
    );
    let by_subject = f.provider.logout(json!({"sid": null, "sub": "subject-1"}));
    assert_eq!(post_logout(&f, by_subject).await, 200);
    let response = f
        .callback("in-flight", &started.state, &started.sign_in_cookie)
        .await;
    assert_eq!(response.status().as_u16(), 401);
    let users: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM auth_sessions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(users, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn concurrent_logout_and_callback_never_leave_the_ended_session_alive(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    for round in 0..12 {
        let sid = format!("provider-session-race-{round}");
        let started = f.start("/app.html").await;
        let mut claims = f.provider.id_claims(&started.nonce, &["user"]);
        claims["sid"] = json!(sid);
        let code = format!("race-{round}");
        f.grant(&code, &started, claims);
        let logout = f.provider.logout(json!({"sid": sid}));
        let (callback, logged_out) = tokio::join!(
            f.callback(&code, &started.state, &started.sign_in_cookie),
            post_logout(&f, logout),
        );
        assert_eq!(logged_out, 200, "round {round}");
        assert!(
            matches!(callback.status().as_u16(), 303 | 401),
            "round {round}"
        );
        assert_eq!(live_sessions(&pool, &sid).await, 0, "round {round}");
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn expired_logout_marks_are_cleaned_up(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    assert_eq!(post_logout(&f, f.provider.logout(json!({}))).await, 200);
    let expired = sqlx::query(
        "UPDATE oidc_logout_marks SET expires_at = NOW() - INTERVAL '1 second' \
         WHERE kind = 'sid' AND value = 'provider-session-1'",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(expired.rows_affected(), 1, "the logout left its mark");
    crate::sessions::cleanup_expired(&pool).await.unwrap();
    let marks: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM oidc_logout_marks")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(marks, 0);
}
