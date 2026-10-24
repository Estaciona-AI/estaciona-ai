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

/// Typed synthetic package for tests that deliberately build maps in memory.
pub(crate) fn package(site: crate::site_config::SiteConfig) -> super::InstallationPackage {
    let policy = crate::reservation_policy::ReservationPolicy::load(&site.directory).unwrap();
    super::InstallationPackage::for_unit_test(identity(2, "test"), site, policy)
}

/// Synthetic descriptor for deliberately in-memory unit fixtures.
pub(crate) fn descriptor(expected: super::InstallationIdentity) -> super::InstallationDescriptor {
    let path = std::env::temp_dir().join(format!(
        "estaciona-test-descriptor-{}.json",
        uuid::Uuid::new_v4()
    ));
    let environment = match expected.environment() {
        super::Environment::Production => "production",
        super::Environment::Demo => "demo",
        super::Environment::Test => "test",
    };
    std::fs::write(&path, serde_json::json!({
        "schema_version": 1,
        "organization": {"id":expected.organization_id(),"slug":"synthetic-org","name":"Synthetic org"},
        "installation": {"id":expected.installation_id(),"slug":"synthetic-site","name":"Synthetic site"},
        "environment": environment,
    }).to_string()).unwrap();
    let descriptor = super::InstallationDescriptor::load(&path).unwrap();
    std::fs::remove_file(path).unwrap();
    descriptor
}
