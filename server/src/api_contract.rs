// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

//! HTTP version boundary; existing endpoints keep their legacy wire responses.
use std::{collections::BTreeMap, sync::OnceLock};

use axum::{
    Json,
    extract::{Path, Request},
    http::{HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;
use tracing::Instrument;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Serialize, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    BadRequest,
    Unauthorized,
    Forbidden,
    NotFound,
    MethodNotAllowed,
    Conflict,
    PayloadTooLarge,
    UnprocessableEntity,
    TooManyRequests,
    ServiceUnavailable,
    InternalError,
    HttpError,
}

impl From<StatusCode> for ErrorCode {
    fn from(status: StatusCode) -> Self {
        match status {
            StatusCode::BAD_REQUEST => Self::BadRequest,
            StatusCode::UNAUTHORIZED => Self::Unauthorized,
            StatusCode::FORBIDDEN => Self::Forbidden,
            StatusCode::NOT_FOUND => Self::NotFound,
            StatusCode::METHOD_NOT_ALLOWED => Self::MethodNotAllowed,
            StatusCode::CONFLICT => Self::Conflict,
            StatusCode::PAYLOAD_TOO_LARGE => Self::PayloadTooLarge,
            StatusCode::UNPROCESSABLE_ENTITY => Self::UnprocessableEntity,
            StatusCode::TOO_MANY_REQUESTS => Self::TooManyRequests,
            StatusCode::SERVICE_UNAVAILABLE => Self::ServiceUnavailable,
            value if value.is_server_error() => Self::InternalError,
            _ => Self::HttpError,
        }
    }
}

#[derive(Serialize, JsonSchema)]
pub struct ErrorEnvelope {
    pub code: ErrorCode,
    pub message: &'static str,
    pub request_id: Uuid,
}

#[derive(Serialize, JsonSchema)]
pub struct AuthenticationCapabilities {
    pub human_sessions: &'static str,
    pub browser_websocket: &'static str,
    pub edge_authentication: &'static str,
    pub logout_scope: &'static str,
    pub sso: bool,
}

#[derive(Serialize, JsonSchema)]
pub struct Capabilities {
    pub api_version: &'static str,
    pub contract_version: &'static str,
    pub tenancy_mode: &'static str,
    pub authentication: AuthenticationCapabilities,
    pub paths: BTreeMap<&'static str, &'static str>,
}

pub async fn capabilities() -> Json<Capabilities> {
    Json(Capabilities {
        api_version: "1",
        contract_version: crate::contracts::VERSION,
        tenancy_mode: "single_installation",
        authentication: AuthenticationCapabilities {
            human_sessions: "revocable_local_jwt",
            browser_websocket: "single_use_ticket",
            edge_authentication: "individual_gateway_bearer",
            logout_scope: "current_session",
            sso: true,
        },
        paths: BTreeMap::from([
            ("sign_in", "/auth/login"),
            ("current_user", "/api/v1/me"),
            ("logout", "/api/v1/auth/logout"),
            ("websocket_ticket", "/api/v1/auth/ws-ticket"),
            ("contract_manifest", "/api/v1/contracts/v1/manifest.json"),
            ("app_websocket", "/ws/app"),
            ("dashboard_websocket", "/ws/dashboard"),
            ("edge_websocket", "/ws/edge"),
        ]),
    })
}

// Only immutable type metadata is global: no user, map or tenant state.
static CONTRACT_ARTIFACTS: OnceLock<BTreeMap<String, Value>> = OnceLock::new();

pub async fn contract(Path(path): Path<String>) -> Result<Json<Value>, StatusCode> {
    let artifacts = CONTRACT_ARTIFACTS.get_or_init(|| {
        crate::contracts::artifacts()
            .into_iter()
            .filter(|(name, _)| name != "v1/fixtures.json")
            .collect()
    });
    artifacts
        .get(&path)
        .cloned()
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

pub async fn versioned_response(request: Request, next: Next) -> Response {
    let request_id = Uuid::new_v4();
    let span = tracing::info_span!("api_v1",%request_id);
    let mut response = next.run(request).instrument(span).await;
    let status = response.status();
    if status.is_client_error() || status.is_server_error() {
        // Do not inspect or copy legacy diagnostics (including SQL errors).
        // Preserve status, Retry-After, Allow and authentication/cookie headers.
        let (mut parts, _) = response.into_parts();
        parts.headers.remove(header::CONTENT_LENGTH);
        parts.headers.remove(header::CONTENT_ENCODING);
        parts.headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        let body = Json(ErrorEnvelope {
            code: status.into(),
            message: status.canonical_reason().unwrap_or("HTTP request failed"),
            request_id,
        })
        .into_response()
        .into_body();
        response = Response::from_parts(parts, body);
    }
    response.headers_mut().insert(
        "x-request-id",
        HeaderValue::from_str(&request_id.to_string()).expect("UUID header is valid"),
    );
    response
        .headers_mut()
        .insert("x-estaciona-api-version", HeaderValue::from_static("1"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        http::{StatusCode, header},
        middleware,
        routing::{get, post},
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
    };

    async fn call(
        path: &str,
        method: &str,
    ) -> (String, std::collections::BTreeMap<String, String>, Vec<u8>) {
        let router = Router::new()
            .route(
                "/failure",
                get(|| async {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "postgres://private-password@host/database",
                    )
                }),
            )
            .route(
                "/busy",
                get(|| async {
                    (
                        StatusCode::SERVICE_UNAVAILABLE,
                        [(header::RETRY_AFTER, "23")],
                        "private diagnostic",
                    )
                }),
            )
            .route("/empty", post(|| async { StatusCode::NO_CONTENT }))
            .fallback(|| async { StatusCode::NOT_FOUND })
            .layer(middleware::from_fn(versioned_response));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream.write_all(format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nX-Request-ID: attacker-value\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).await.unwrap();
        server.abort();
        let split = bytes
            .windows(4)
            .position(|value| value == b"\r\n\r\n")
            .unwrap();
        let headers = std::str::from_utf8(&bytes[..split]).unwrap();
        let mut lines = headers.lines();
        let status = lines.next().unwrap().to_owned();
        let headers = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(key, value)| (key.to_ascii_lowercase(), value.trim().to_owned()))
            .collect();
        (status, headers, bytes[split + 4..].to_vec())
    }

    #[tokio::test]
    async fn errors_are_json_correlated_and_do_not_echo_private_diagnostics() {
        let (status, headers, body) = call("/failure", "GET").await;
        assert!(status.contains("500"));
        assert_eq!(
            headers.get("content-type").map(String::as_str),
            Some("application/json")
        );
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["code"], "INTERNAL_ERROR");
        assert!(!body.to_string().contains("private-password"));
        let id = body["request_id"].as_str().unwrap();
        assert_eq!(headers.get("x-request-id").map(String::as_str), Some(id));
        uuid::Uuid::parse_str(id).unwrap();
        assert_ne!(id, "attacker-value");
    }

    #[tokio::test]
    async fn retry_headers_and_empty_success_are_preserved() {
        let (_, headers, body) = call("/busy", "GET").await;
        assert_eq!(headers.get("retry-after").map(String::as_str), Some("23"));
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["code"], "SERVICE_UNAVAILABLE");
        let (status, headers, body) = call("/empty", "POST").await;
        assert!(status.contains("204"));
        assert!(body.is_empty());
        assert_eq!(
            headers.get("x-estaciona-api-version").map(String::as_str),
            Some("1")
        );
    }

    #[tokio::test]
    async fn not_found_and_method_errors_use_the_same_contract() {
        for (path, method, expected) in [
            ("/missing", "GET", "NOT_FOUND"),
            ("/failure", "POST", "METHOD_NOT_ALLOWED"),
        ] {
            let (_, _, body) = call(path, method).await;
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["code"], expected);
        }
    }

    #[test]
    fn every_status_has_a_stable_error_code() {
        for (status, code) in [
            (StatusCode::BAD_REQUEST, "BAD_REQUEST"),
            (StatusCode::UNAUTHORIZED, "UNAUTHORIZED"),
            (StatusCode::FORBIDDEN, "FORBIDDEN"),
            (StatusCode::NOT_FOUND, "NOT_FOUND"),
            (StatusCode::METHOD_NOT_ALLOWED, "METHOD_NOT_ALLOWED"),
            (StatusCode::CONFLICT, "CONFLICT"),
            (StatusCode::PAYLOAD_TOO_LARGE, "PAYLOAD_TOO_LARGE"),
            (StatusCode::UNPROCESSABLE_ENTITY, "UNPROCESSABLE_ENTITY"),
            (StatusCode::TOO_MANY_REQUESTS, "TOO_MANY_REQUESTS"),
            (StatusCode::SERVICE_UNAVAILABLE, "SERVICE_UNAVAILABLE"),
            (StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR"),
            (StatusCode::BAD_GATEWAY, "INTERNAL_ERROR"),
            (StatusCode::GONE, "HTTP_ERROR"),
        ] {
            assert_eq!(
                serde_json::to_value(ErrorCode::from(status)).unwrap(),
                code,
                "{status}"
            );
        }
    }

    #[tokio::test]
    async fn published_contracts_exclude_the_test_fixtures() {
        assert!(contract(Path("v1/manifest.json".into())).await.is_ok());
        assert_eq!(
            contract(Path("v1/fixtures.json".into())).await.unwrap_err(),
            StatusCode::NOT_FOUND
        );
    }
}
