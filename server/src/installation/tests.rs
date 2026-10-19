// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

use super::{DescriptorError, Environment, InstallationDescriptor};
use serde_json::{Value, json};
use std::{fs, io::Write, path::PathBuf};
use uuid::Uuid;

// All identities and labels here are synthetic; they provision no installation.
const ORGANIZATION: &str = "11111111-1111-4111-8111-111111111111";
const INSTALLATION: &str = "22222222-2222-4222-8222-222222222222";

fn document() -> Value {
    json!({
        "schema_version": 1,
        "organization": {"id": ORGANIZATION, "slug": "synthetic-org", "name": "Synthetic organization"},
        "installation": {"id": INSTALLATION, "slug": "synthetic-site", "name": "Synthetic installation"},
        "environment": "test"
    })
}

struct Fixture(PathBuf);
impl Fixture {
    fn bytes(bytes: &[u8]) -> Self {
        let path =
            std::env::temp_dir().join(format!("estaciona-descriptor-{}.json", Uuid::new_v4()));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        file.write_all(bytes).unwrap();
        Self(path)
    }
    fn json(document: &Value) -> Self {
        Self::bytes(&serde_json::to_vec(document).unwrap())
    }
    fn load(&self) -> Result<InstallationDescriptor, DescriptorError> {
        InstallationDescriptor::load(&self.0)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[test]
fn valid_descriptor_preserves_explicit_identity_and_metadata() {
    let descriptor = Fixture::json(&document()).load().unwrap();
    assert_eq!(descriptor.schema_version(), 1);
    assert_eq!(
        descriptor.organization().id(),
        Uuid::parse_str(ORGANIZATION).unwrap()
    );
    assert_eq!(
        descriptor.installation().id(),
        Uuid::parse_str(INSTALLATION).unwrap()
    );
    assert_eq!(descriptor.organization().slug(), "synthetic-org");
    assert_eq!(descriptor.installation().name(), "Synthetic installation");
    assert_eq!(descriptor.identity().environment(), Environment::Test);
    assert_eq!(
        descriptor.identity().organization_id(),
        descriptor.organization().id()
    );
    assert_eq!(
        descriptor.identity().installation_id(),
        descriptor.installation().id()
    );
}

#[test]
fn identity_ignores_labels_but_includes_organization_installation_and_environment() {
    let original = Fixture::json(&document()).load().unwrap().identity();
    let mut renamed = document();
    for section in ["organization", "installation"] {
        renamed[section]["name"] = json!("Renamed synthetic label");
        renamed[section]["slug"] = json!("renamed-label");
    }
    assert_eq!(Fixture::json(&renamed).load().unwrap().identity(), original);
    for section in ["organization", "installation"] {
        let mut changed = document();
        changed[section]["id"] = json!("33333333-3333-4333-8333-333333333333");
        assert_ne!(Fixture::json(&changed).load().unwrap().identity(), original);
    }
    for (name, expected) in [
        ("production", Environment::Production),
        ("demo", Environment::Demo),
        ("test", Environment::Test),
    ] {
        let mut changed = document();
        changed["environment"] = json!(name);
        let identity = Fixture::json(&changed).load().unwrap().identity();
        assert_eq!(identity.environment(), expected);
        assert_eq!(identity == original, name == "test");
    }
}

#[test]
fn rejects_missing_fields_at_every_level() {
    for field in [
        "schema_version",
        "organization",
        "installation",
        "environment",
    ] {
        let mut invalid = document();
        invalid.as_object_mut().unwrap().remove(field);
        assert!(Fixture::json(&invalid).load().is_err(), "missing {field}");
    }
    for section in ["organization", "installation"] {
        for field in ["id", "slug", "name"] {
            let mut invalid = document();
            invalid[section].as_object_mut().unwrap().remove(field);
            assert!(
                Fixture::json(&invalid).load().is_err(),
                "missing {section}.{field}"
            );
        }
    }
}

#[test]
fn rejects_unknown_fields_at_every_level() {
    let mut invalid = document();
    invalid["unexpected"] = json!(true);
    assert!(Fixture::json(&invalid).load().is_err());
    for section in ["organization", "installation"] {
        let mut invalid = document();
        invalid[section]["unexpected"] = json!(true);
        assert!(Fixture::json(&invalid).load().is_err());
    }
}

#[test]
fn rejects_unsupported_versions_and_undefined_environments() {
    for version in [
        json!(0),
        json!(2),
        json!(-1),
        json!(1.5),
        json!("1"),
        Value::Null,
    ] {
        let mut invalid = document();
        invalid["schema_version"] = version;
        assert!(Fixture::json(&invalid).load().is_err());
    }
    for environment in [
        json!("staging"),
        json!("Production"),
        json!(""),
        json!(false),
        Value::Null,
    ] {
        let mut invalid = document();
        invalid["environment"] = environment;
        assert!(Fixture::json(&invalid).load().is_err());
    }
}

#[test]
fn rejects_invalid_and_nil_uuids_without_generating_replacements() {
    for section in ["organization", "installation"] {
        for id in [
            json!("not-a-uuid"),
            json!("00000000-0000-0000-0000-000000000000"),
            json!(""),
            json!(12),
            Value::Null,
        ] {
            let mut invalid = document();
            invalid[section]["id"] = id;
            assert!(Fixture::json(&invalid).load().is_err());
        }
    }
}

#[test]
fn enforces_ascii_slug_boundaries_without_normalizing_them() {
    for section in ["organization", "installation"] {
        for slug in ["a".to_owned(), "a".repeat(63), "a--b-2".to_owned()] {
            let mut valid = document();
            valid[section]["slug"] = json!(slug);
            assert!(Fixture::json(&valid).load().is_ok());
        }
        for slug in [
            "".to_owned(),
            "a".repeat(64),
            "-a".to_owned(),
            "a-".to_owned(),
            "A".to_owned(),
            "á".to_owned(),
            "a_b".to_owned(),
            " a".to_owned(),
            "a ".to_owned(),
        ] {
            let mut invalid = document();
            invalid[section]["slug"] = json!(slug);
            assert!(Fixture::json(&invalid).load().is_err());
        }
    }
}

#[test]
fn trims_names_and_counts_unicode_characters_instead_of_bytes() {
    for section in ["organization", "installation"] {
        let mut valid = document();
        valid[section]["name"] = json!(format!("  {}  ", "é".repeat(128)));
        let loaded = Fixture::json(&valid).load().unwrap();
        let name = if section == "organization" {
            loaded.organization().name()
        } else {
            loaded.installation().name()
        };
        assert_eq!(name, "é".repeat(128));
        for name in ["".to_owned(), " \t\n".to_owned(), "é".repeat(129)] {
            let mut invalid = document();
            invalid[section]["name"] = json!(name);
            assert!(Fixture::json(&invalid).load().is_err());
        }
    }
}

#[test]
fn enforces_sixteen_kib_limit_in_bytes_including_valid_trailing_whitespace() {
    let mut bytes = serde_json::to_vec(&document()).unwrap();
    bytes.resize(16 * 1024, b' ');
    assert!(Fixture::bytes(&bytes).load().is_ok());
    bytes.push(b' ');
    assert!(matches!(
        Fixture::bytes(&bytes).load(),
        Err(DescriptorError::TooLarge)
    ));
}

#[test]
fn rejects_invalid_utf8_json_types_and_duplicate_keys() {
    for bytes in [
        b"\xff".as_slice(),
        b"{",
        b"[]",
        b"null",
        b"{} {}",
        b"{\"schema_version\":1,\"schema_version\":1}",
    ] {
        assert!(Fixture::bytes(bytes).load().is_err());
    }
    let duplicate = document().to_string().replacen(
        "\"schema_version\":1",
        "\"schema_version\":1,\"schema_version\":1",
        1,
    );
    assert!(Fixture::bytes(duplicate.as_bytes()).load().is_err());
    for id in [ORGANIZATION, INSTALLATION] {
        let key = format!("\"id\":\"{id}\"");
        let duplicate = document()
            .to_string()
            .replacen(&key, &format!("{key},{key}"), 1);
        assert!(Fixture::bytes(duplicate.as_bytes()).load().is_err());
    }
    for section in ["organization", "installation"] {
        let mut invalid = document();
        invalid[section] = json!([]);
        assert!(Fixture::json(&invalid).load().is_err());
    }
}

#[test]
fn missing_file_is_an_error_and_creates_nothing() {
    let path =
        std::env::temp_dir().join(format!("estaciona-no-descriptor-{}.json", Uuid::new_v4()));
    assert!(matches!(
        InstallationDescriptor::load(&path),
        Err(DescriptorError::Io(_))
    ));
    assert!(!path.exists());
}
