// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

//! Admission for WebSocket handshakes and connections. Authenticating a
//! handshake queries the database, whose pool is small and shared with every
//! request: only a few handshakes authenticate at a time and the rest are
//! refused at once, before any query. Established sockets are capped per kind
//! and give their slot back when they close.
use std::sync::Arc;

use axum::{
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Edge events are under 4 KiB; a forecast payload stays well below this.
pub const EDGE_MESSAGE_BYTES: usize = 64 * 1024;
/// Apps and dashboards send short commands only.
pub const CLIENT_MESSAGE_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WsLimits {
    /// Below the pool size, so established operations keep connections.
    pub handshakes: usize,
    pub edge: usize,
    pub app: usize,
    pub dashboard: usize,
}

impl Default for WsLimits {
    fn default() -> Self {
        Self {
            handshakes: 2,
            edge: 64,
            app: 2000,
            dashboard: 64,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Kind {
    Edge,
    App,
    Dashboard,
}

pub struct WsAdmission {
    handshakes: Arc<Semaphore>,
    edge: Arc<Semaphore>,
    app: Arc<Semaphore>,
    dashboard: Arc<Semaphore>,
}

impl Default for WsAdmission {
    fn default() -> Self {
        Self::new(WsLimits::default())
    }
}

/// Refusal for a full slot: answered at once, with a hint to retry.
#[derive(Debug)]
pub struct Busy(&'static str);

impl IntoResponse for Busy {
    fn into_response(self) -> Response {
        let mut response = (StatusCode::SERVICE_UNAVAILABLE, self.0).into_response();
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
        response
    }
}

impl WsAdmission {
    pub fn new(limits: WsLimits) -> Self {
        Self {
            handshakes: Arc::new(Semaphore::new(limits.handshakes)),
            edge: Arc::new(Semaphore::new(limits.edge)),
            app: Arc::new(Semaphore::new(limits.app)),
            dashboard: Arc::new(Semaphore::new(limits.dashboard)),
        }
    }

    /// A slot for the connection's lifetime, taken before authentication.
    pub fn connection(&self, kind: Kind) -> Result<OwnedSemaphorePermit, Busy> {
        let semaphore = match kind {
            Kind::Edge => &self.edge,
            Kind::App => &self.app,
            Kind::Dashboard => &self.dashboard,
        };
        semaphore
            .clone()
            .try_acquire_owned()
            .map_err(|_| Busy("Too many WebSocket connections"))
    }

    /// Held only while the handshake authenticates against the database.
    pub fn handshake(&self) -> Result<OwnedSemaphorePermit, Busy> {
        self.handshakes
            .clone()
            .try_acquire_owned()
            .map_err(|_| Busy("WebSocket authentication busy"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_are_refused_when_taken_and_returned_on_drop() {
        let admission = WsAdmission::new(WsLimits {
            handshakes: 1,
            edge: 1,
            app: 1,
            dashboard: 1,
        });
        let handshake = admission.handshake().unwrap();
        let refused = admission.handshake().unwrap_err().into_response();
        assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(refused.headers()[header::RETRY_AFTER], "1");
        drop(handshake);
        drop(admission.handshake().unwrap());

        let edge = admission.connection(Kind::Edge).unwrap();
        assert!(admission.connection(Kind::Edge).is_err());
        // Kinds do not share slots.
        let _app = admission.connection(Kind::App).unwrap();
        let _dashboard = admission.connection(Kind::Dashboard).unwrap();
        drop(edge);
        drop(admission.connection(Kind::Edge).unwrap());
    }
}
