// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

//! A browser attaches the session cookie to any request it sends, so a write
//! that relies on that cookie must come from an allowed origin. Bearer requests
//! need no check: browsers never attach them on their own.
use axum::{
    extract::{Request, State},
    http::Method,
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::{security, ws_origin::WsOriginPolicy};

pub async fn guard(State(policy): State<WsOriginPolicy>, request: Request, next: Next) -> Response {
    let safe = matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    );
    let headers = request.headers();
    if !safe
        && security::bearer_token(headers).is_none()
        && security::session_cookie_token(headers).is_some()
        && let Err(rejection) = policy.authorize(headers)
    {
        return rejection.into_response();
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use axum::{Router, http::StatusCode, routing::post};
    use openidconnect::reqwest;

    async fn serve() -> String {
        let policy = crate::ws_origin::WsOriginPolicy::parse("https://app.example").unwrap();
        let router = Router::new()
            .route(
                "/write",
                post(|| async { "written" }).get(|| async { "read" }),
            )
            .layer(axum::middleware::from_fn_with_state(policy, super::guard));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{address}/write")
    }

    #[tokio::test]
    async fn cookie_writes_need_an_allowed_origin_and_bearer_or_reads_do_not() {
        let url = serve().await;
        let client = reqwest::Client::new();
        let cookie = "__Host-estaciona_session=token";
        let status = |request: reqwest::RequestBuilder| async move {
            request.send().await.unwrap().status()
        };
        for (origin, expected) in [
            (None, StatusCode::FORBIDDEN),
            (Some("https://evil.example"), StatusCode::FORBIDDEN),
            (Some("https://app.example.evil"), StatusCode::FORBIDDEN),
            (Some("https://app.example"), StatusCode::OK),
        ] {
            let mut request = client.post(&url).header("Cookie", cookie);
            if let Some(origin) = origin {
                request = request.header("Origin", origin);
            }
            assert_eq!(
                status(request).await.as_u16(),
                expected.as_u16(),
                "{origin:?}"
            );
        }
        let bearer = client
            .post(&url)
            .header("Cookie", cookie)
            .header("Authorization", "Bearer token");
        assert_eq!(status(bearer).await.as_u16(), 200);
        let read = client.get(&url).header("Cookie", cookie);
        assert_eq!(status(read).await.as_u16(), 200);
        // Without the session cookie there is nothing to forge; the handler decides.
        assert_eq!(status(client.post(&url)).await.as_u16(), 200);
    }
}
