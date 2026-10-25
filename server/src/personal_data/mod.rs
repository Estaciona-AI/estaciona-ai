// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

//! Encryption of personal fields at rest. The database stores only envelopes;
//! the keys stay in a file mounted into the server alone.
mod keyring;

pub use keyring::Keyring;
#[cfg(test)]
use keyring::KeyringError;

use ring::{
    aead::{Aad, NONCE_LEN, Nonce},
    rand::{SecureRandom, SystemRandom},
};
use serde::{Serialize, de::DeserializeOwned};
use std::fmt;
use uuid::Uuid;
use zeroize::Zeroizing;

const FORMAT: u8 = 1;
const AAD_CONTEXT: &[u8] = b"estaciona/personal-data/v1";
const HEADER_BYTES: usize = 1 + 4 + NONCE_LEN;
const TAG_BYTES: usize = 16;

/// Each value is bound to its column so an envelope cannot be moved elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    UserDateOfBirth,
    UserPcdStatus,
}

impl Field {
    fn label(self) -> &'static [u8] {
        match self {
            Self::UserDateOfBirth => b"users.date_of_birth",
            Self::UserPcdStatus => b"users.pcd_status",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum CipherError {
    Malformed,
    UnknownKey(u32),
    Authentication,
    Encoding,
    Randomness,
}

impl fmt::Display for CipherError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed => f.write_str("Personal data envelope is malformed"),
            Self::UnknownKey(id) => write!(f, "Personal data key {id} is not loaded"),
            Self::Authentication => f.write_str("Personal data envelope failed authentication"),
            Self::Encoding => f.write_str("Personal data value has an unexpected type"),
            Self::Randomness => f.write_str("System randomness unavailable"),
        }
    }
}

impl std::error::Error for CipherError {}

/// Envelope: `format (1) ‖ key id (u32 BE) ‖ nonce (12) ‖ AES-256-GCM ciphertext ‖ tag`.
/// AAD: context, installation, column and row, NUL-separated.
pub struct PersonalDataCipher {
    keyring: Keyring,
    active: u32,
    installation: Uuid,
    random: SystemRandom,
}

impl PersonalDataCipher {
    pub fn new(keyring: Keyring, installation: Uuid) -> Self {
        let active = *keyring
            .keys
            .keys()
            .next_back()
            .expect("a parsed keyring is never empty");
        Self {
            keyring,
            active,
            installation,
            random: SystemRandom::new(),
        }
    }

    #[cfg(test)]
    pub fn for_unit_test() -> Self {
        Self::new(Keyring::for_unit_test(), Uuid::from_u128(0x5eed))
    }

    pub fn encrypt<T: Serialize + ?Sized>(
        &self,
        field: Field,
        row: Uuid,
        value: &T,
    ) -> Result<Vec<u8>, CipherError> {
        let mut nonce = [0u8; NONCE_LEN];
        self.random
            .fill(&mut nonce)
            .map_err(|_| CipherError::Randomness)?;
        let mut body = serde_json::to_vec(value).map_err(|_| CipherError::Encoding)?;
        self.keyring.keys[&self.active]
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(self.aad(field, row)),
                &mut body,
            )
            .map_err(|_| CipherError::Encoding)?;
        let mut envelope = Vec::with_capacity(HEADER_BYTES + body.len());
        envelope.push(FORMAT);
        envelope.extend_from_slice(&self.active.to_be_bytes());
        envelope.extend_from_slice(&nonce);
        envelope.extend_from_slice(&body);
        Ok(envelope)
    }

    pub fn decrypt<T: DeserializeOwned>(
        &self,
        field: Field,
        row: Uuid,
        envelope: &[u8],
    ) -> Result<T, CipherError> {
        if envelope.len() < HEADER_BYTES + TAG_BYTES || envelope[0] != FORMAT {
            return Err(CipherError::Malformed);
        }
        let id = u32::from_be_bytes(envelope[1..5].try_into().expect("four header bytes"));
        let key = self
            .keyring
            .keys
            .get(&id)
            .ok_or(CipherError::UnknownKey(id))?;
        let nonce = Nonce::try_assume_unique_for_key(&envelope[5..HEADER_BYTES])
            .map_err(|_| CipherError::Malformed)?;
        let mut body = Zeroizing::new(envelope[HEADER_BYTES..].to_vec());
        let plain = key
            .open_in_place(nonce, Aad::from(self.aad(field, row)), &mut body)
            .map_err(|_| CipherError::Authentication)?;
        serde_json::from_slice(plain).map_err(|_| CipherError::Encoding)
    }

    /// Decrypts a nullable column; an absent envelope means an undeclared value.
    pub fn decrypt_optional<T: DeserializeOwned>(
        &self,
        field: Field,
        row: Uuid,
        envelope: Option<&[u8]>,
    ) -> Result<Option<T>, CipherError> {
        match envelope {
            Some(envelope) => self.decrypt::<Option<T>>(field, row, envelope),
            None => Ok(None),
        }
    }

    fn aad(&self, field: Field, row: Uuid) -> Vec<u8> {
        let label = field.label();
        let mut aad = Vec::with_capacity(AAD_CONTEXT.len() + label.len() + 35);
        aad.extend_from_slice(AAD_CONTEXT);
        aad.push(0);
        aad.extend_from_slice(self.installation.as_bytes());
        aad.push(0);
        aad.extend_from_slice(label);
        aad.push(0);
        aad.extend_from_slice(row.as_bytes());
        aad
    }
}

/// Base64 of 32 fixed bytes, for tests and disposable fixtures only.
#[cfg(test)]
pub fn fixture_key() -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode([0x5a; 32])
}

#[cfg(test)]
mod tests;
