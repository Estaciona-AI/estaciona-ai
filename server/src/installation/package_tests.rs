// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

use super::{
    InstallationPackage, startup,
    store::{self, BindMode},
    store_tests::identity,
};
use sqlx::PgPool;
use std::{fs, path::PathBuf};

pub(super) struct Fixture {
    pub directory: PathBuf,
}
impl Fixture {
    pub fn new(site: u128) -> Self {
        let directory =
            std::env::temp_dir().join(format!("estaciona-package-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("installation.json"), serde_json::json!({
            "schema_version":1,
            "organization":{"id":uuid::Uuid::from_u128(1),"slug":"synthetic-org","name":"Synthetic org"},
            "installation":{"id":uuid::Uuid::from_u128(site),"slug":"synthetic-site","name":"Synthetic site"},
            "environment":"test",
        }).to_string()).unwrap();
        fs::write(
            directory.join("config.json"),
            serde_json::json!([
                {"name":"Synthetic lot","path":[{"x":0,"z":0},{"x":1,"z":0}],"legacy_metadata":true}
            ])
            .to_string(),
        )
        .unwrap();
        fs::write(
            directory.join("spots_3d.json"),
            serde_json::json!([
                {"id":"package-spot","center3D":{"x":2,"y":0,"z":1}}
            ])
            .to_string(),
        )
        .unwrap();
        Self { directory }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.directory).unwrap();
    }
}

#[test]
fn loads_valid_package_with_defaults_and_custom_manifest_filename() {
    let f = Fixture::new(2);
    let package = InstallationPackage::load(&f.directory).unwrap();
    assert_eq!(package.identity(), identity(2, "test"));
    assert_eq!(package.directory(), f.directory.canonicalize().unwrap());
    let path = f.directory.join("custom-descriptor.json");
    fs::rename(f.directory.join("installation.json"), &path).unwrap();
    assert!(InstallationPackage::from_manifest(&path).is_ok());
}

#[test]
fn missing_or_malformed_files_are_rejected_without_echoing_content() {
    for name in ["installation.json", "config.json", "spots_3d.json"] {
        let f = Fixture::new(2);
        let path = f.directory.join(name);
        fs::remove_file(&path).unwrap();
        assert!(InstallationPackage::load(&f.directory).is_err());
        fs::write(&path, "do-not-echo-this-input").unwrap();
        let error = InstallationPackage::load(&f.directory).unwrap_err();
        assert!(!format!("{error:?} {error}").contains("do-not-echo-this-input"));
    }
    for content in [
        "{",
        r#"{"hold_seconds":0}"#,
        r#"{"do-not-echo-this-input":"secret"}"#,
    ] {
        let f = Fixture::new(2);
        fs::write(f.directory.join("reservation_policy.json"), content).unwrap();
        let error = InstallationPackage::load(&f.directory).unwrap_err();
        assert!(!format!("{error:?} {error}").contains("do-not-echo-this-input"));
    }
}

#[test]
fn rejects_duplicate_spots_and_navigation_name_collisions() {
    for spots in [
        serde_json::json!([{"id":"cam-01","center3D":{"x":2,"y":0,"z":1}}]),
        serde_json::json!([{"id":"x","center3D":{"x":2,"y":0,"z":1}},{"id":"x","center3D":{"x":3,"y":0,"z":1}}]),
    ] {
        let f = Fixture::new(2);
        fs::write(f.directory.join("spots_3d.json"), spots.to_string()).unwrap();
        assert!(InstallationPackage::load(&f.directory).is_err());
    }
}

#[test]
fn file_reads_accept_exact_byte_limits_and_refuse_the_next_byte() {
    for (name, limit) in [
        ("config.json", 2 * 1024 * 1024),
        ("spots_3d.json", 2 * 1024 * 1024),
        ("reservation_policy.json", 16 * 1024),
    ] {
        let f = Fixture::new(2);
        let path = f.directory.join(name);
        let mut bytes = if name == "reservation_policy.json" {
            b"{}".to_vec()
        } else {
            fs::read(&path).unwrap()
        };
        bytes.resize(limit, b' ');
        fs::write(&path, &bytes).unwrap();
        assert!(InstallationPackage::load(&f.directory).is_ok(), "{name}");
        bytes.push(b' ');
        fs::write(&path, &bytes).unwrap();
        assert!(InstallationPackage::load(&f.directory).is_err(), "{name}");
    }
}

#[test]
fn oversized_formatted_config_preserves_the_existing_file() {
    let f = Fixture::new(2);
    let path = f.directory.join("config.json");
    let before = fs::read(&path).unwrap();
    let payload = serde_json::json!([{"name":"Synthetic lot","path":[{"x":0,"z":0},{"x":1,"z":0}],"extra":vec![0;300_000]}]);
    assert!(serde_json::to_vec(&payload).unwrap().len() <= 2 * 1024 * 1024);
    assert!(serde_json::to_vec_pretty(&payload).unwrap().len() > 2 * 1024 * 1024);
    let error = crate::site_config::save_parking_config(&f.directory, &payload).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(fs::read(&path).unwrap(), before);
    assert!(
        !fs::read_dir(&f.directory).unwrap().any(|p| p
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp"))
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn package_identity_mismatch_has_no_reconciliation_effect(pool: PgPool) {
    let f = Fixture::new(3);
    let expected = identity(2, "test");
    store::bind(&pool, &expected, BindMode::EmptyDatabase)
        .await
        .unwrap();
    let verified = startup::prepare(pool.clone(), expected).await.unwrap();
    let package = InstallationPackage::load(&f.directory).unwrap();
    let before = fs::read(f.directory.join("config.json")).unwrap();
    let result = crate::state::init_state(
        verified,
        "jwt".into(),
        "pepper".into(),
        crate::personal_data::Keyring::for_unit_test(),
        package,
        Default::default(),
    )
    .await;
    assert!(matches!(
        result,
        Err(crate::site_reconciliation::ReconcileError::InstallationMismatch)
    ));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM site_map_revisions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM spots")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    assert_eq!(fs::read(f.directory.join("config.json")).unwrap(), before);
}

#[sqlx::test(migrations = "./migrations")]
async fn context_consumes_validated_policy_without_reading_the_file_again(pool: PgPool) {
    let f = Fixture::new(2);
    let expected = identity(2, "test");
    store::bind(&pool, &expected, BindMode::EmptyDatabase)
        .await
        .unwrap();
    let path = f.directory.join("reservation_policy.json");
    fs::write(&path, r#"{"hold_seconds":300}"#).unwrap();
    let package = InstallationPackage::load(&f.directory).unwrap();
    fs::write(&path, r#"{"hold_seconds":600}"#).unwrap();
    let verified = startup::prepare(pool.clone(), expected).await.unwrap();
    let state = crate::state::init_state(
        verified,
        "jwt".into(),
        "pepper".into(),
        crate::personal_data::Keyring::for_unit_test(),
        package,
        Default::default(),
    )
    .await
    .unwrap();
    assert_eq!(state.installation.reservation_policy.hold_seconds, 300);
    let package = InstallationPackage::load(&f.directory).unwrap();
    let verified = startup::prepare(pool, expected).await.unwrap();
    let state = crate::state::init_state(
        verified,
        "jwt".into(),
        "pepper".into(),
        crate::personal_data::Keyring::for_unit_test(),
        package,
        Default::default(),
    )
    .await
    .unwrap();
    assert_eq!(state.installation.reservation_policy.hold_seconds, 600);
}

#[test]
fn present_but_broken_policy_link_is_not_a_default_policy() {
    let f = Fixture::new(2);
    std::os::unix::fs::symlink(
        f.directory.join("missing-policy.json"),
        f.directory.join("reservation_policy.json"),
    )
    .unwrap();
    assert!(InstallationPackage::load(&f.directory).is_err());
}
