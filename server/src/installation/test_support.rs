// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

//! Unit-test fixtures only. Never compiled into the production binary.
use super::{
    VerifiedInstallation, startup,
    store::{self, BindMode, BindingStatus},
    store_tests::identity,
};
use sqlx::PgPool;

/// Provision before inserting any business data. Reuse the same identity on restart.
pub(crate) async fn provision(pool: &PgPool) -> VerifiedInstallation {
    let expected = identity(2, "test");
    if store::check(pool, &expected).await.unwrap() == BindingStatus::Unbound {
        store::bind(pool, &expected, BindMode::EmptyDatabase)
            .await
            .unwrap();
    }
    startup::prepare(pool.clone(), expected).await.unwrap()
}

/// Only for unit tests requiring an offline/closed pool or artificial runtime state.
pub(crate) fn unverified_for_unit_test(pool: &PgPool) -> VerifiedInstallation {
    VerifiedInstallation::new(pool.clone(), identity(2, "test"))
}
