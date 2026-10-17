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
}
