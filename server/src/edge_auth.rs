use sha2::{Digest, Sha256};

pub(crate) fn credential_digest(credential: &str) -> [u8; 32] {
    Sha256::digest(credential.as_bytes()).into()
}
