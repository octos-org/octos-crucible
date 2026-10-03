//! The platform public key, compiled in from `config/keys.json`. The Worker
//! never holds the private key; it only checks that uploads name the
//! current key.

use std::sync::OnceLock;

use crucible_core::Envelope;
use serde::{Deserialize, Serialize};

const KEYS_JSON: &str = include_str!("../../../config/keys.json");

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PubKey {
    pub key_id: String,
    pub public_key: String,
}

#[derive(Deserialize)]
struct KeysFile {
    current: String,
    keys: Vec<PubKey>,
}

pub fn current() -> &'static PubKey {
    static KEY: OnceLock<PubKey> = OnceLock::new();
    KEY.get_or_init(|| {
        let file: KeysFile = serde_json::from_str(KEYS_JSON).expect("config/keys.json parses");
        file.keys
            .into_iter()
            .find(|k| k.key_id == file.current)
            .expect("config/keys.json: current key is listed")
    })
}

#[derive(Debug, PartialEq, Eq)]
pub enum SealError {
    NotEnvelope,
    WrongKey,
    Empty,
}

/// The bytes start with a crucible envelope header sealed to the current
/// key and carry some ciphertext. Nothing else can be checked without the
/// private key.
pub fn check_sealed(data: &[u8], key_id: &str) -> Result<(), SealError> {
    let (env, body) = Envelope::split(data).map_err(|_| SealError::NotEnvelope)?;
    if env.key_id != key_id {
        return Err(SealError::WrongKey);
    }
    if body.is_empty() {
        return Err(SealError::Empty);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shard::sha256_hex;

    #[test]
    fn current_key_matches_its_fingerprint() {
        let k = current();
        // crucible-crypto::key_id: first 16 hex chars of SHA-256(age1...).
        assert_eq!(k.key_id, sha256_hex(k.public_key.as_bytes())[..16]);
        assert!(k.public_key.starts_with("age1"));
    }

    #[test]
    fn sealed_header() {
        let kid = &current().key_id;
        let mut ok = Envelope::new(kid.clone()).header_line();
        ok.extend_from_slice(b"age-encryption.org/v1\n-> X25519 ...");
        assert_eq!(check_sealed(&ok, kid), Ok(()));

        let mut other = Envelope::new("0000000000000000").header_line();
        other.extend_from_slice(b"x");
        assert_eq!(check_sealed(&other, kid), Err(SealError::WrongKey));

        assert_eq!(
            check_sealed(&Envelope::new(kid.clone()).header_line(), kid),
            Err(SealError::Empty)
        );
        assert_eq!(
            check_sealed(b"PK\x03\x04plain zip", kid),
            Err(SealError::NotEnvelope)
        );
        assert_eq!(check_sealed(b"", kid), Err(SealError::NotEnvelope));
        let bad_alg =
            format!("{{\"crucible_envelope\":1,\"alg\":\"rsa\",\"key_id\":\"{kid}\"}}\nxx");
        assert_eq!(
            check_sealed(bad_alg.as_bytes(), kid),
            Err(SealError::NotEnvelope)
        );
    }
}
