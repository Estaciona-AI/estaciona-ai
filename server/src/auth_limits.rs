// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

use axum::{
    extract::{Request, State},
    http::{Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::state::SharedState;

pub const AUTH_BODY_LIMIT: usize = 16 * 1024;
const AUTH_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

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
                "Authentication worker failed",
            )
                .into_response(),
        }
    }
}

pub struct AuthLimits {
    hash_slots: Arc<Semaphore>,
    request_slots: Arc<Semaphore>,
    login: Mutex<TokenBucket>,
    register: Mutex<TokenBucket>,
}

impl Default for AuthLimits {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            hash_slots: Arc::new(Semaphore::new(1)),
            request_slots: Arc::new(Semaphore::new(8)),
            login: Mutex::new(TokenBucket::new(Duration::from_millis(200), 10, now)),
            register: Mutex::new(TokenBucket::new(Duration::from_secs(1), 2, now)),
        }
    }
}

#[derive(Clone, Copy)]
enum AuthEndpoint {
    Login,
    Register,
}

impl AuthLimits {
    pub async fn run_blocking<T, F>(&self, work: F) -> Result<T, AuthLimitError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let permit = self
            .hash_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| AuthLimitError::Unavailable)?;

        tokio::task::spawn_blocking(move || {
            let output = work();
            // The closure owns the permit even if its HTTP caller is cancelled.
            drop(permit);
            output
        })
        .await
        .map_err(|error| {
            tracing::error!(panic = error.is_panic(), "Authentication worker failed");
            AuthLimitError::Internal
        })
    }

    fn take_rate_at(&self, endpoint: AuthEndpoint, now: Instant) -> Result<(), AuthLimitError> {
        let bucket = match endpoint {
            AuthEndpoint::Login => &self.login,
            AuthEndpoint::Register => &self.register,
        };
        let mut bucket = bucket.lock().map_err(|_| AuthLimitError::Internal)?;
        if bucket.try_take_at(now) {
            Ok(())
        } else {
            Err(AuthLimitError::RateLimited)
        }
    }

    fn admit_request(
        &self,
        endpoint: AuthEndpoint,
    ) -> Result<OwnedSemaphorePermit, AuthLimitError> {
        self.take_rate_at(endpoint, Instant::now())?;
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

pub async fn guard_login(
    State(state): State<SharedState>,
    request: Request,
    next: Next,
) -> Response {
    let method = request.method().clone();
    guard_future(
        &state.auth_limits,
        AuthEndpoint::Login,
        method,
        next.run(request),
    )
    .await
}

pub async fn guard_register(
    State(state): State<SharedState>,
    request: Request,
    next: Next,
) -> Response {
    let method = request.method().clone();
    guard_future(
        &state.auth_limits,
        AuthEndpoint::Register,
        method,
        next.run(request),
    )
    .await
}

async fn guard_future(
    limits: &AuthLimits,
    endpoint: AuthEndpoint,
    method: Method,
    work: impl std::future::Future<Output = Response>,
) -> Response {
    if method != Method::POST {
        return work.await;
    }
    let permit = match limits.admit_request(endpoint) {
        Ok(permit) => permit,
        Err(error) => return error.into_response(),
    };
    let response = tokio::time::timeout(AUTH_REQUEST_TIMEOUT, work)
        .await
        .unwrap_or_else(|_| AuthLimitError::Unavailable.into_response());
    drop(permit);
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::oneshot;

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
    async fn blocking_job_returns_its_value_and_releases_the_slot() {
        let limits = AuthLimits::default();
        assert_eq!(limits.run_blocking(|| 42).await, Ok(42));
        assert_eq!(limits.run_blocking(|| 7).await, Ok(7));
    }

    #[tokio::test]
    async fn work_error_is_preserved_and_releases_the_slot() {
        let limits = AuthLimits::default();
        assert_eq!(
            limits
                .run_blocking(|| Err::<(), _>("synthetic work error"))
                .await,
            Ok(Err("synthetic work error"))
        );
        assert_eq!(limits.run_blocking(|| 17).await, Ok(17));
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
    fn request_admission_rejects_ninth_without_queueing() {
        let limits = AuthLimits::default();
        let permits = (0..8)
            .map(|_| limits.admit_request(AuthEndpoint::Login).unwrap())
            .collect::<Vec<_>>();
        assert!(matches!(
            limits.admit_request(AuthEndpoint::Login),
            Err(AuthLimitError::Unavailable)
        ));
        drop(permits);
        assert!(limits.admit_request(AuthEndpoint::Login).is_ok());
    }

    #[test]
    fn login_and_registration_buckets_are_separate() {
        let limits = AuthLimits::default();
        let now = Instant::now();
        for _ in 0..10 {
            assert!(limits.take_rate_at(AuthEndpoint::Login, now).is_ok());
        }
        assert_eq!(
            limits.take_rate_at(AuthEndpoint::Login, now),
            Err(AuthLimitError::RateLimited)
        );
        assert!(limits.take_rate_at(AuthEndpoint::Register, now).is_ok());
        assert!(limits.take_rate_at(AuthEndpoint::Register, now).is_ok());
        assert_eq!(
            limits.take_rate_at(AuthEndpoint::Register, now),
            Err(AuthLimitError::RateLimited)
        );
        assert!(
            limits
                .take_rate_at(AuthEndpoint::Login, now + Duration::from_millis(200))
                .is_ok()
        );
        assert_eq!(
            limits.take_rate_at(AuthEndpoint::Register, now + Duration::from_millis(200)),
            Err(AuthLimitError::RateLimited)
        );
        assert!(
            limits
                .take_rate_at(AuthEndpoint::Register, now + Duration::from_secs(1))
                .is_ok()
        );
    }

    #[test]
    fn concurrent_rate_admission_shares_a_single_burst() {
        let limits = AuthLimits::default();
        let now = Instant::now();
        let accepted = std::thread::scope(|scope| {
            let callers = (0..32)
                .map(|_| scope.spawn(|| limits.take_rate_at(AuthEndpoint::Login, now).is_ok()))
                .collect::<Vec<_>>();
            callers
                .into_iter()
                .map(|caller| usize::from(caller.join().unwrap()))
                .sum::<usize>()
        });
        assert_eq!(accepted, 10);
    }

    #[tokio::test]
    async fn successful_guard_preserves_the_handler_response() {
        let limits = AuthLimits::default();
        let response = guard_future(&limits, AuthEndpoint::Login, Method::POST, async {
            (StatusCode::ACCEPTED, "accepted handler body").into_response()
        })
        .await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(
            to_bytes(response.into_body(), 1024).await.unwrap().as_ref(),
            b"accepted handler body"
        );
    }

    #[tokio::test]
    async fn deadline_releases_request_capacity_but_keeps_running_hash_bounded() {
        let limits = Arc::new(AuthLimits::default());
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let request_limits = limits.clone();
        let start = Instant::now();
        let request = tokio::spawn(async move {
            guard_future(&request_limits, AuthEndpoint::Login, Method::POST, async {
                let _ = request_limits
                    .run_blocking(move || {
                        let _ = started_tx.send(());
                        let _ = release_rx.recv_timeout(Duration::from_secs(10));
                    })
                    .await;
                StatusCode::OK.into_response()
            })
            .await
        });
        tokio::time::timeout(Duration::from_secs(2), started_rx)
            .await
            .unwrap()
            .unwrap();
        let response = tokio::time::timeout(Duration::from_secs(8), request)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[header::RETRY_AFTER], "1");
        assert!(start.elapsed() >= Duration::from_secs(4));
        let permits = (0..8)
            .map(|_| limits.admit_request(AuthEndpoint::Login).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            limits.run_blocking(|| ()).await,
            Err(AuthLimitError::Unavailable)
        );
        release_tx.send(()).unwrap();
        drop(permits);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn running_job_stays_bounded_after_its_caller_is_cancelled() {
        let limits = Arc::new(AuthLimits::default());
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worker_limits = limits.clone();
        let caller = tokio::spawn(async move {
            worker_limits
                .run_blocking(move || {
                    let _ = started_tx.send(());
                    let _ = release_rx.recv_timeout(Duration::from_secs(3));
                })
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), started_rx)
            .await
            .unwrap()
            .unwrap();
        let rejected_work_started = Arc::new(AtomicBool::new(false));
        let observed = rejected_work_started.clone();
        assert_eq!(
            limits
                .run_blocking(move || observed.store(true, Ordering::SeqCst))
                .await,
            Err(AuthLimitError::Unavailable)
        );
        assert!(!rejected_work_started.load(Ordering::SeqCst));
        // This task runs on the only async worker while the blocking closure waits.
        assert_eq!(tokio::spawn(async { 9 }).await.unwrap(), 9);
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        assert_eq!(
            limits.run_blocking(|| ()).await,
            Err(AuthLimitError::Unavailable)
        );
        release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match limits.run_blocking(|| 11).await {
                    Ok(value) => break assert_eq!(value, 11),
                    Err(AuthLimitError::Unavailable) => tokio::task::yield_now().await,
                    Err(error) => panic!("unexpected worker error: {error:?}"),
                }
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn panic_returns_internal_error_and_releases_the_slot() {
        let limits = AuthLimits::default();
        assert_eq!(
            limits
                .run_blocking::<(), _>(|| panic!("synthetic worker panic"))
                .await,
            Err(AuthLimitError::Internal)
        );
        assert_eq!(limits.run_blocking(|| 13).await, Ok(13));
    }

    #[tokio::test]
    async fn closed_budget_rejects_work_without_running_it() {
        let limits = AuthLimits::default();
        assert_eq!(limits.run_blocking(|| 19).await, Ok(19));
        limits.hash_slots.close();
        let ran = Arc::new(AtomicBool::new(false));
        let observed = ran.clone();
        assert_eq!(
            limits
                .run_blocking(move || observed.store(true, Ordering::SeqCst))
                .await,
            Err(AuthLimitError::Unavailable)
        );
        assert!(!ran.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn other_methods_preserve_405_without_consuming_authentication_burst() {
        let limits = AuthLimits::default();
        for (endpoint, method, burst) in [
            (AuthEndpoint::Login, Method::GET, 10),
            (AuthEndpoint::Register, Method::OPTIONS, 2),
        ] {
            for _ in 0..12 {
                let response = guard_future(&limits, endpoint, method.clone(), async {
                    StatusCode::METHOD_NOT_ALLOWED.into_response()
                })
                .await;
                assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
            }
            let now = Instant::now();
            for _ in 0..burst {
                assert!(limits.take_rate_at(endpoint, now).is_ok());
            }
            assert_eq!(
                limits.take_rate_at(endpoint, now),
                Err(AuthLimitError::RateLimited)
            );
        }
    }
}
