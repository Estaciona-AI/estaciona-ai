// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::NaiveDate;
use uuid::Uuid;

use super::{CipherError, Field, Keyring, KeyringError, PersonalDataCipher, fixture_key};

fn key(byte: u8) -> String {
    STANDARD.encode([byte; 32])
}

fn cipher(text: &str, installation: u128) -> PersonalDataCipher {
    PersonalDataCipher::new(Keyring::parse(text).unwrap(), Uuid::from_u128(installation))
}

#[test]
fn values_round_trip_and_each_encryption_uses_a_fresh_nonce() {
    let cipher = PersonalDataCipher::for_unit_test();
    let row = Uuid::new_v4();
    let born = Some(NaiveDate::from_ymd_opt(1950, 1, 2).unwrap());
    let first = cipher.encrypt(Field::UserDateOfBirth, row, &born).unwrap();
    let second = cipher.encrypt(Field::UserDateOfBirth, row, &born).unwrap();
    assert_ne!(first, second);
    for envelope in [&first, &second] {
        assert_eq!(
            cipher
                .decrypt::<Option<NaiveDate>>(Field::UserDateOfBirth, row, envelope)
                .unwrap(),
            born
        );
        assert!(!String::from_utf8_lossy(envelope).contains("1950"));
    }
    let none: Option<bool> = None;
    let envelope = cipher.encrypt(Field::UserPcdStatus, row, &none).unwrap();
    assert_eq!(
        cipher
            .decrypt_optional::<bool>(Field::UserPcdStatus, row, Some(&envelope))
            .unwrap(),
        None
    );
    assert_eq!(
        cipher
            .decrypt_optional::<bool>(Field::UserPcdStatus, row, None)
            .unwrap(),
        None
    );
}

#[test]
fn an_envelope_only_opens_for_its_installation_column_and_row() {
    let text = format!("1:{}", fixture_key());
    let owner = cipher(&text, 1);
    let row = Uuid::new_v4();
    let envelope = owner
        .encrypt(Field::UserPcdStatus, row, &Some(true))
        .unwrap();
    assert_eq!(
        owner.decrypt::<Option<bool>>(Field::UserPcdStatus, Uuid::new_v4(), &envelope),
        Err(CipherError::Authentication)
    );
    assert_eq!(
        owner.decrypt::<Option<bool>>(Field::UserDateOfBirth, row, &envelope),
        Err(CipherError::Authentication)
    );
    assert_eq!(
        cipher(&text, 2).decrypt::<Option<bool>>(Field::UserPcdStatus, row, &envelope),
        Err(CipherError::Authentication)
    );
}

#[test]
fn tampered_truncated_and_foreign_envelopes_are_rejected() {
    let cipher = PersonalDataCipher::for_unit_test();
    let row = Uuid::new_v4();
    let envelope = cipher
        .encrypt(Field::UserPcdStatus, row, &Some(true))
        .unwrap();
    for position in [5, envelope.len() / 2, envelope.len() - 1] {
        let mut tampered = envelope.clone();
        tampered[position] ^= 1;
        assert_eq!(
            cipher.decrypt::<Option<bool>>(Field::UserPcdStatus, row, &tampered),
            Err(CipherError::Authentication)
        );
    }
    assert_eq!(
        cipher.decrypt::<Option<bool>>(Field::UserPcdStatus, row, &envelope[..20]),
        Err(CipherError::Malformed)
    );
    let mut unknown_format = envelope.clone();
    unknown_format[0] = 2;
    assert_eq!(
        cipher.decrypt::<Option<bool>>(Field::UserPcdStatus, row, &unknown_format),
        Err(CipherError::Malformed)
    );
    let mut unknown_key = envelope.clone();
    unknown_key[1..5].copy_from_slice(&7u32.to_be_bytes());
    assert_eq!(
        cipher.decrypt::<Option<bool>>(Field::UserPcdStatus, row, &unknown_key),
        Err(CipherError::UnknownKey(7))
    );
    assert_eq!(
        cipher.decrypt::<Option<NaiveDate>>(Field::UserPcdStatus, row, &envelope),
        Err(CipherError::Encoding)
    );
}

#[test]
fn the_highest_key_encrypts_and_older_keys_still_decrypt() {
    let row = Uuid::new_v4();
    let old = cipher(&format!("1:{}", key(1)), 1);
    let before = old.encrypt(Field::UserPcdStatus, row, &Some(true)).unwrap();
    let rotated = cipher(&format!("# rotated\n2:{}\n1:{}\n", key(2), key(1)), 1);
    let after = rotated
        .encrypt(Field::UserPcdStatus, row, &Some(false))
        .unwrap();
    assert_eq!(&after[1..5], &2u32.to_be_bytes());
    assert_eq!(
        rotated
            .decrypt::<Option<bool>>(Field::UserPcdStatus, row, &before)
            .unwrap(),
        Some(true)
    );
    assert_eq!(
        old.decrypt::<Option<bool>>(Field::UserPcdStatus, row, &after),
        Err(CipherError::UnknownKey(2))
    );
}

#[test]
fn invalid_keyrings_are_refused_without_echoing_their_contents() {
    let secret = key(9);
    let cases = [
        (String::new(), KeyringError::Empty),
        ("# comment only\n\n".to_owned(), KeyringError::Empty),
        (secret.clone(), KeyringError::InvalidLine(1)),
        (format!("0:{secret}"), KeyringError::InvalidLine(1)),
        (format!("x:{secret}"), KeyringError::InvalidLine(1)),
        ("1:not-base64!".to_owned(), KeyringError::InvalidLine(1)),
        (
            format!("1:{}", STANDARD.encode([1u8; 16])),
            KeyringError::InvalidLine(1),
        ),
        (
            format!("\n1:{secret}\n1:{}", key(8)),
            KeyringError::DuplicateId(1),
        ),
    ];
    for (text, expected) in cases {
        let error = Keyring::parse(&text).err().unwrap();
        assert_eq!(error, expected);
        assert!(!error.to_string().contains(&secret));
    }
}

#[test]
fn keyring_files_are_read_with_a_size_limit() {
    let directory = std::env::temp_dir().join(format!("personal-data-{}", Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join("keys");
    std::fs::write(&path, format!("1:{}\n", key(3))).unwrap();
    assert!(Keyring::load(&path).is_ok());
    std::fs::write(&path, "#".repeat(16 * 1024 + 1)).unwrap();
    assert_eq!(Keyring::load(&path).err(), Some(KeyringError::Unreadable));
    assert_eq!(
        Keyring::load(&directory.join("missing")).err(),
        Some(KeyringError::Unreadable)
    );
    std::fs::remove_dir_all(directory).unwrap();
}
