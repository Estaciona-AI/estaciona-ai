// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

use std::{collections::BTreeMap, fmt, io::Read, path::Path};

use base64::{Engine, engine::general_purpose::STANDARD};
use ring::aead::{AES_256_GCM, LessSafeKey, UnboundKey};
use zeroize::Zeroizing;

const MAX_FILE_BYTES: u64 = 16 * 1024;
const KEY_BYTES: usize = 32;

/// AES-256-GCM keys by id, read from a file that never reaches the database.
/// One `<id>:<base64 of 32 random bytes>` per line; the highest id encrypts.
pub struct Keyring {
    pub(super) keys: BTreeMap<u32, LessSafeKey>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum KeyringError {
    Unreadable,
    Empty,
    InvalidLine(usize),
    DuplicateId(u32),
}

// Messages name the problem and line, never the file contents.
impl fmt::Display for KeyringError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreadable => f.write_str("PERSONAL_DATA_KEYS_FILE cannot be read"),
            Self::Empty => f.write_str("PERSONAL_DATA_KEYS_FILE has no keys"),
            Self::InvalidLine(line) => write!(
                f,
                "PERSONAL_DATA_KEYS_FILE line {line} is not <id>:<base64 of 32 bytes>"
            ),
            Self::DuplicateId(id) => write!(f, "PERSONAL_DATA_KEYS_FILE repeats key id {id}"),
        }
    }
}

impl std::error::Error for KeyringError {}

impl Keyring {
    #[cfg(test)]
    pub fn for_unit_test() -> Self {
        Self::parse(&format!("1:{}", super::fixture_key())).expect("fixture key")
    }

    pub fn load(path: &Path) -> Result<Self, KeyringError> {
        let mut text = Zeroizing::new(String::new());
        std::fs::File::open(path)
            .and_then(|file| file.take(MAX_FILE_BYTES + 1).read_to_string(&mut text))
            .map_err(|_| KeyringError::Unreadable)?;
        if text.len() as u64 > MAX_FILE_BYTES {
            return Err(KeyringError::Unreadable);
        }
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self, KeyringError> {
        let mut keys = BTreeMap::new();
        for (index, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let invalid = KeyringError::InvalidLine(index + 1);
            let (id, encoded) = line.split_once(':').ok_or(invalid)?;
            let id = id
                .parse::<u32>()
                .ok()
                .filter(|id| *id > 0)
                .ok_or(KeyringError::InvalidLine(index + 1))?;
            let bytes = Zeroizing::new(
                STANDARD
                    .decode(encoded)
                    .map_err(|_| KeyringError::InvalidLine(index + 1))?,
            );
            if bytes.len() != KEY_BYTES {
                return Err(KeyringError::InvalidLine(index + 1));
            }
            let key = UnboundKey::new(&AES_256_GCM, &bytes)
                .map_err(|_| KeyringError::InvalidLine(index + 1))?;
            if keys.insert(id, LessSafeKey::new(key)).is_some() {
                return Err(KeyringError::DuplicateId(id));
            }
        }
        if keys.is_empty() {
            return Err(KeyringError::Empty);
        }
        Ok(Self { keys })
    }
}
