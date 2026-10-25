// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

//! Admission for the unauthenticated sign-in endpoints. Each request writes a
//! row or calls the identity provider, so the process caps both rate and
//! concurrency. Password guessing is the identity provider's to stop.
use axum::{
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::state::SharedState;

const SIGN_IN_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthLimitError {
    RateLimited,
    Unavailable,
    Internal,
}

impl IntoResponse for AuthLimitError {
    fn into_response(self) -> Response {
        match self {
            Self::RateLimited => (
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, "1")],
                "Too many authentication requests",
            )
                .into_response(),
            Self::Unavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                [(header::RETRY_AFTER, "1")],
                "Authentication temporarily unavailable",
            )
                .into_response(),
            Self::Internal => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Authentication admission failed",
            )
                .into_response(),
        }
    }
}

pub struct AuthLimits {
    request_slots: Arc<Semaphore>,
    sign_in: Mutex<TokenBucket>,
}

impl Default for AuthLimits {
    fn default() -> Self {
        Self {
            request_slots: Arc::new(Semaphore::new(16)),
            // 20 per second with a burst of 40: a class arriving together, not a flood.
            sign_in: Mutex::new(TokenBucket::new(
                Duration::from_millis(50),
                40,
                Instant::now(),
            )),
        }
    }
}

impl AuthLimits {
    fn take_rate_at(&self, now: Instant) -> Result<(), AuthLimitError> {
        let mut bucket = self.sign_in.lock().map_err(|_| AuthLimitError::Internal)?;
        if bucket.try_take_at(now) {
            Ok(())
        } else {
            Err(AuthLimitError::RateLimited)
        }
    }

    fn admit_request(&self) -> Result<OwnedSemaphorePermit, AuthLimitError> {
        self.take_rate_at(Instant::now())?;
        self.request_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| AuthLimitError::Unavailable)
    }
}

struct TokenBucket {
    period: Duration,
    capacity: Duration,
    available: Duration,
    last_refill: Instant,
}

impl TokenBucket {
    fn new(period: Duration, burst: u32, now: Instant) -> Self {
        let capacity = period * burst;
        Self {
            period,
            capacity,
            available: capacity,
            last_refill: now,
        }
    }

    fn try_take_at(&mut self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.last_refill);
        self.available = self.available.saturating_add(elapsed).min(self.capacity);
        self.last_refill = self.last_refill.max(now);
        if self.available >= self.period {
            self.available -= self.period;
            true
        } else {
            false
        }
    }
}

pub async fn guard_sign_in(
    State(state): State<SharedState>,
    request: Request,
    next: Next,
) -> Response {
    guard_future(
        &state.auth_limits,
        SIGN_IN_REQUEST_TIMEOUT,
        next.run(request),
    )
    .await
}

async fn guard_future(
    limits: &AuthLimits,
    deadline: Duration,
    work: impl std::future::Future<Output = Response>,
) -> Response {
    let permit = match limits.admit_request() {
        Ok(permit) => permit,
        Err(error) => return error.into_response(),
    };
    let response = tokio::time::timeout(deadline, work)
        .await
        .unwrap_or_else(|_| AuthLimitError::Unavailable.into_response());
    drop(permit);
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[test]
    fn bucket_rejects_exhausted_burst_and_refills_fractional_credit() {
        let now = Instant::now();
        let mut bucket = TokenBucket::new(Duration::from_millis(200), 10, now);
        for _ in 0..10 {
            assert!(bucket.try_take_at(now));
        }
        assert!(!bucket.try_take_at(now));
        assert!(!bucket.try_take_at(now + Duration::from_millis(100)));
        assert!(bucket.try_take_at(now + Duration::from_millis(200)));
        assert!(!bucket.try_take_at(now + Duration::from_millis(200)));
        assert!(bucket.try_take_at(now + Duration::from_secs(60)));
        for _ in 0..9 {
            assert!(bucket.try_take_at(now + Duration::from_secs(60)));
        }
        assert!(!bucket.try_take_at(now + Duration::from_secs(60)));
    }

    #[tokio::test]
    async fn error_responses_use_retry_after_only_for_retryable_limits() {
        for (error, status, retry_after) in [
            (
                AuthLimitError::RateLimited,
                StatusCode::TOO_MANY_REQUESTS,
                Some("1"),
            ),
            (
                AuthLimitError::Unavailable,
                StatusCode::SERVICE_UNAVAILABLE,
                Some("1"),
            ),
            (
                AuthLimitError::Internal,
                StatusCode::INTERNAL_SERVER_ERROR,
                None,
            ),
        ] {
            let response = error.into_response();
            assert_eq!(response.status(), status);
            assert_eq!(
                response
                    .headers()
                    .get(header::RETRY_AFTER)
                    .map(|value| value.to_str().unwrap()),
                retry_after,
            );
            assert!(
                !to_bytes(response.into_body(), 1024)
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[test]
    fn request_admission_rejects_the_seventeenth_without_queueing() {
        let limits = AuthLimits::default();
        let permits = (0..16)
            .map(|_| limits.admit_request().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            limits.admit_request().err(),
            Some(AuthLimitError::Unavailable)
        );
        drop(permits);
        assert!(limits.admit_request().is_ok());
    }

    #[test]
    fn concurrent_rate_admission_shares_a_single_burst() {
        let limits = AuthLimits::default();
        let now = Instant::now();
        let accepted = std::thread::scope(|scope| {
            let callers = (0..64)
                .map(|_| scope.spawn(|| limits.take_rate_at(now).is_ok()))
                .collect::<Vec<_>>();
            callers
                .into_iter()
                .map(|caller| usize::from(caller.join().unwrap()))
                .sum::<usize>()
        });
        assert_eq!(accepted, 40);
    }

    #[tokio::test]
    async fn successful_guard_preserves_the_handler_response() {
        let limits = AuthLimits::default();
        let response = guard_future(&limits, SIGN_IN_REQUEST_TIMEOUT, async {
            (StatusCode::SEE_OTHER, "redirect body").into_response()
        })
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            to_bytes(response.into_body(), 1024).await.unwrap().as_ref(),
            b"redirect body"
        );
    }

    #[tokio::test]
    async fn a_stalled_sign_in_times_out_and_releases_its_slot() {
        let limits = AuthLimits::default();
        let response = guard_future(
            &limits,
            Duration::from_millis(20),
            std::future::pending::<Response>(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let permits: Vec<_> = (0..16)
            .map(|_| limits.request_slots.clone().try_acquire_owned().unwrap())
            .collect();
        assert_eq!(permits.len(), 16);
    }
}
