// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

use std::{io, path::PathBuf, sync::Arc};

use serde_json::Value;
use tokio::sync::Semaphore;

/// Local-file persistence boundary. Construct once per destination in the
/// process; clones share admission, while independent destinations do not.
#[derive(Clone)]
pub struct LocalConfigStore {
    directory: PathBuf,
    write_slot: Arc<Semaphore>,
}

#[derive(Debug)]
pub enum ConfigWriteError {
    Busy,
    Storage(io::Error),
}

impl LocalConfigStore {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
            write_slot: Arc::new(Semaphore::new(1)),
        }
    }

    pub async fn save(&self, payload: Value) -> Result<(), ConfigWriteError> {
        self.run_write(move |directory| {
            crate::site_config::save_parking_config(&directory, &payload)
        })
        .await
    }

    async fn run_write(
        &self,
        write: impl FnOnce(PathBuf) -> io::Result<()> + Send + 'static,
    ) -> Result<(), ConfigWriteError> {
        let permit = self
            .write_slot
            .clone()
            .try_acquire_owned()
            .map_err(|_| ConfigWriteError::Busy)?;
        let directory = self.directory.clone();
        tokio::task::spawn_blocking(move || {
            // The worker owns admission, including after its HTTP future is dropped.
            let _permit = permit;
            write(directory)
        })
        .await
        .map_err(|error| ConfigWriteError::Storage(io::Error::other(error)))?
        .map_err(ConfigWriteError::Storage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn r10_filesystem_work_does_not_run_on_the_async_worker() {
        let worker = std::thread::current().id();
        let store = LocalConfigStore::new(std::env::temp_dir());
        store
            .run_write(move |_| {
                assert_ne!(std::thread::current().id(), worker);
                Ok(())
            })
            .await
            .unwrap();
    }

    async fn pause_writer(
        store: LocalConfigStore,
    ) -> (
        std::sync::mpsc::Sender<()>,
        tokio::task::JoinHandle<Result<(), ConfigWriteError>>,
    ) {
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let task = tokio::spawn(async move {
            store
                .run_write(move |_| {
                    let _ = started.send(());
                    wait.recv().map_err(io::Error::other)?;
                    Ok(())
                })
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), ready)
            .await
            .unwrap()
            .unwrap();
        (release, task)
    }

    #[tokio::test]
    async fn r10_busy_destination_rejects_without_queuing_a_second_write() {
        let store = LocalConfigStore::new(std::env::temp_dir());
        let (release, task) = pause_writer(store.clone()).await;
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            store.run_write(|_| Ok(())),
        )
        .await
        .unwrap();
        assert!(matches!(result, Err(ConfigWriteError::Busy)));
        release.send(()).unwrap();
        task.await.unwrap().unwrap();
        store.run_write(|_| Ok(())).await.unwrap();
    }

    #[tokio::test]
    async fn r10_canceled_caller_does_not_release_a_running_write() {
        let store = LocalConfigStore::new(std::env::temp_dir());
        let (release, task) = pause_writer(store.clone()).await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(matches!(
            store.run_write(|_| Ok(())).await,
            Err(ConfigWriteError::Busy)
        ));
        release.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match store.run_write(|_| Ok(())).await {
                    Err(ConfigWriteError::Busy) => tokio::task::yield_now().await,
                    result => break result.unwrap(),
                }
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn r10_independent_destinations_do_not_share_write_admission() {
        let first = LocalConfigStore::new("first-site");
        let second = LocalConfigStore::new("second-site");
        let (release, task) = pause_writer(first).await;
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            second.run_write(|_| Ok(())),
        )
        .await
        .unwrap()
        .unwrap();
        release.send(()).unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn r10_worker_panic_is_reported_and_releases_write_admission() {
        let store = LocalConfigStore::new(std::env::temp_dir());
        let result = store.run_write(|_| panic!("injected writer panic")).await;
        assert!(matches!(result, Err(ConfigWriteError::Storage(_))));
        store.run_write(|_| Ok(())).await.unwrap();
    }

    #[tokio::test]
    async fn r10_storage_failure_releases_write_admission() {
        let store = LocalConfigStore::new(std::env::temp_dir());
        let result = store
            .run_write(|_| Err(io::Error::other("injected I/O failure")))
            .await;
        assert!(matches!(result, Err(ConfigWriteError::Storage(_))));
        store.run_write(|_| Ok(())).await.unwrap();
    }
    struct ConfigFixture(PathBuf);

    impl ConfigFixture {
        fn new() -> Self {
            let directory =
                std::env::temp_dir().join(format!("r10-config-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&directory).unwrap();
            std::fs::write(
                directory.join("config.json"),
                serde_json::to_vec(&Self::payload("Initial")).unwrap(),
            )
            .unwrap();
            std::fs::write(directory.join("spots_3d.json"), b"[]").unwrap();
            Self(directory)
        }

        fn payload(name: &str) -> Value {
            serde_json::json!([{
                "name": name,
                "camera": {"position": {"x": 5}},
                "path": [{"x": 0, "z": 0}, {"x": 1, "z": 0}]
            }])
        }

        fn read(&self) -> Value {
            serde_json::from_slice(&std::fs::read(self.0.join("config.json")).unwrap()).unwrap()
        }

        async fn state(&self, pool: sqlx::PgPool) -> crate::state::SharedState {
            crate::state::init_state(
                crate::installation::test_support::provision(&pool).await,
                "r10-disposable-jwt".into(),
                "r10-disposable-pepper".into(),
                crate::personal_data::Keyring::for_unit_test(),
                crate::identity::IdentityProvider::for_unit_test(),
                crate::installation::test_support::package(
                    crate::site_config::SiteConfig::load(&self.0).unwrap(),
                ),
                Default::default(),
            )
            .await
            .unwrap()
        }
    }

    impl Drop for ConfigFixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    async fn principal(state: &crate::state::SharedState, role: &str) -> axum::http::HeaderMap {
        let id = uuid::Uuid::new_v4();
        sqlx::query("INSERT INTO users (id, role) VALUES ($1, $2)")
            .bind(id)
            .bind(role)
            .execute(state.installation.pool())
            .await
            .unwrap();
        let token = crate::sessions::issue(state.installation.pool(), id, role, &state.jwt_secret)
            .await
            .unwrap();
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        headers
    }

    #[tokio::test]
    async fn r10_saving_separate_configs_preserves_extra_fields_and_isolation() {
        let first = ConfigFixture::new();
        let second = ConfigFixture::new();
        let one = LocalConfigStore::new(&first.0);
        let two = LocalConfigStore::new(&second.0);
        let (a, b) = tokio::join!(
            one.save(ConfigFixture::payload("Gávea")),
            two.save(ConfigFixture::payload("Outra unidade"))
        );
        a.unwrap();
        b.unwrap();
        assert_eq!(first.read(), ConfigFixture::payload("Gávea"));
        assert_eq!(second.read(), ConfigFixture::payload("Outra unidade"));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn r10_config_route_preserves_success_and_invalid_payload_contracts(pool: sqlx::PgPool) {
        let fixture = ConfigFixture::new();
        let state = fixture.state(pool).await;
        let headers = principal(&state, "admin").await;
        let payload = ConfigFixture::payload("Gávea");
        let response = crate::save_config(
            axum::extract::State(state.clone()),
            headers.clone(),
            axum::Json(payload.clone()),
        )
        .await;
        assert_eq!(response.unwrap(), r#"{"status": "ok"}"#);
        assert_eq!(fixture.read(), payload);
        let invalid = ConfigFixture::payload("<script>");
        let response =
            crate::save_config(axum::extract::State(state), headers, axum::Json(invalid)).await;
        assert_eq!(response.unwrap_err().0, axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(fixture.read(), payload);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn r10_config_route_rejects_non_admin_before_storage_work(pool: sqlx::PgPool) {
        let fixture = ConfigFixture::new();
        let original = fixture.read();
        let state = fixture.state(pool).await;
        let user_headers = principal(&state, "user").await;
        let (release, task) = pause_writer(state.installation.config_store.clone()).await;
        for (headers, expected) in [
            (
                axum::http::HeaderMap::new(),
                axum::http::StatusCode::UNAUTHORIZED,
            ),
            (user_headers, axum::http::StatusCode::FORBIDDEN),
        ] {
            let response = crate::save_config(
                axum::extract::State(state.clone()),
                headers,
                axum::Json(ConfigFixture::payload("Unauthorized")),
            )
            .await;
            assert_eq!(response.unwrap_err().0, expected);
        }
        assert_eq!(fixture.read(), original);
        release.send(()).unwrap();
        task.await.unwrap().unwrap();
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn r10_config_route_returns_503_while_destination_is_busy_then_recovers(
        pool: sqlx::PgPool,
    ) {
        let fixture = ConfigFixture::new();
        let original = fixture.read();
        let state = fixture.state(pool).await;
        let headers = principal(&state, "admin").await;
        let (release, task) = pause_writer(state.installation.config_store.clone()).await;
        let response = crate::save_config(
            axum::extract::State(state.clone()),
            headers.clone(),
            axum::Json(ConfigFixture::payload("Busy")),
        )
        .await;
        assert_eq!(
            response.unwrap_err().0,
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(fixture.read(), original);
        release.send(()).unwrap();
        task.await.unwrap().unwrap();
        crate::save_config(
            axum::extract::State(state),
            headers,
            axum::Json(ConfigFixture::payload("Recovered")),
        )
        .await
        .unwrap();
        assert_eq!(fixture.read(), ConfigFixture::payload("Recovered"));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn r10_config_route_reports_storage_failure_without_exposing_paths(pool: sqlx::PgPool) {
        let fixture = ConfigFixture::new();
        let state = fixture.state(pool).await;
        let headers = principal(&state, "admin").await;
        let config = fixture.0.join("config.json");
        std::fs::remove_file(&config).unwrap();
        std::fs::create_dir(&config).unwrap();
        let response = crate::save_config(
            axum::extract::State(state.clone()),
            headers.clone(),
            axum::Json(ConfigFixture::payload("Failure")),
        )
        .await;
        let (status, message) = response.unwrap_err();
        assert_eq!(status, axum::http::StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!message.contains(fixture.0.to_str().unwrap()));
        assert!(config.is_dir());
        assert_eq!(std::fs::read_dir(&fixture.0).unwrap().count(), 2);
        std::fs::remove_dir(&config).unwrap();
        crate::save_config(
            axum::extract::State(state),
            headers,
            axum::Json(ConfigFixture::payload("Recovered")),
        )
        .await
        .unwrap();
        assert_eq!(fixture.read(), ConfigFixture::payload("Recovered"));
    }

    #[tokio::test]
    async fn r10_started_publication_completes_after_caller_cancellation() {
        let fixture = ConfigFixture::new();
        let store = LocalConfigStore::new(&fixture.0);
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let (published, completed) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            store
                .run_write(move |directory| {
                    let _ = started.send(());
                    wait.recv().map_err(io::Error::other)?;
                    let result = crate::site_config::save_parking_config(
                        &directory,
                        &ConfigFixture::payload("Published"),
                    );
                    let _ = published.send(result.is_ok());
                    result
                })
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), ready)
            .await
            .unwrap()
            .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        release.send(()).unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(2), completed)
                .await
                .unwrap()
                .unwrap()
        );
        assert_eq!(fixture.read(), ConfigFixture::payload("Published"));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn r11_config_route_rejects_navigation_collisions_without_publication(
        pool: sqlx::PgPool,
    ) {
        let fixture = ConfigFixture::new();
        let mut initial = ConfigFixture::payload("Short");
        initial[0]["path"][1]["x"] = serde_json::json!(0);
        std::fs::write(
            fixture.0.join("config.json"),
            serde_json::to_vec(&initial).unwrap(),
        )
        .unwrap();
        std::fs::write(
            fixture.0.join("spots_3d.json"),
            br#"[{"id":"path_node_1","center3D":{"x":1,"y":0,"z":1}}]"#,
        )
        .unwrap();
        let state = fixture.state(pool).await;
        let headers = principal(&state, "admin").await;
        let result = crate::save_config(
            axum::extract::State(state.clone()),
            headers,
            axum::Json(ConfigFixture::payload("Extended")),
        )
        .await;
        let (status, message) = result.unwrap_err();
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        assert!(message.contains("path_node_1"));
        assert_eq!(fixture.read(), initial);
        assert_eq!(
            state
                .installation
                .graph
                .read()
                .await
                .calculate_route("cam-01", "path_node_1"),
            Some(vec!["cam-01".into(), "path_node_1".into()])
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn r11_config_route_treats_unreadable_geometry_as_storage_failure(pool: sqlx::PgPool) {
        let fixture = ConfigFixture::new();
        let original = fixture.read();
        let state = fixture.state(pool).await;
        let headers = principal(&state, "admin").await;
        for geometry in [
            b"invalid JSON".as_slice(),
            br#"[{"id":"duplicate","center3D":{"x":0,"y":0,"z":0}},{"id":"duplicate","center3D":{"x":1,"y":0,"z":0}}]"#.as_slice(),
        ] {
            std::fs::write(fixture.0.join("spots_3d.json"), geometry).unwrap();
            let result = crate::save_config(
                axum::extract::State(state.clone()), headers.clone(),
                axum::Json(ConfigFixture::payload("Rejected")),
            ).await;
            let (status, message) = result.unwrap_err();
            assert_eq!(status, axum::http::StatusCode::INTERNAL_SERVER_ERROR);
            assert!(!message.contains(fixture.0.to_str().unwrap()));
            assert_eq!(fixture.read(), original);
        }
    }
}
