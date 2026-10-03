//! Header of an encrypted file.
//!
//! A sealed file is one JSON line naming the algorithm and the key that
//! sealed it, followed by the ciphertext:
//!
//! ```text
//! {"crucible_envelope":1,"alg":"age-x25519","key_id":"3f1c..."}\n<age binary>
//! ```
//!
//! The key id lets a file be matched to the right private key after a key
//! rotation. Stripping the first line leaves a standard age file, so an
//! admin can also decrypt with the `age` CLI.

use serde::{Deserialize, Serialize};

pub const ENVELOPE_VERSION: u32 = 1;
pub const ALG_AGE_X25519: &str = "age-x25519";
const MAX_HEADER: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    pub crucible_envelope: u32,
    pub alg: String,
    /// Fingerprint of the public key (see `crucible-crypto::key_id`).
    pub key_id: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EnvelopeError {
    #[error("not a crucible envelope")]
    NotEnvelope,
    #[error("unsupported envelope version or algorithm")]
    Unsupported,
}

impl Envelope {
    pub fn new(key_id: impl Into<String>) -> Self {
        Envelope {
            crucible_envelope: ENVELOPE_VERSION,
            alg: ALG_AGE_X25519.into(),
            key_id: key_id.into(),
        }
    }

    /// The header line, newline included.
    pub fn header_line(&self) -> Vec<u8> {
        let mut out = serde_json::to_vec(self).expect("envelope serializes");
        out.push(b'\n');
        out
    }

    /// Split a sealed file into its header and ciphertext.
    pub fn split(data: &[u8]) -> Result<(Envelope, &[u8]), EnvelopeError> {
        let end = data
            .iter()
            .take(MAX_HEADER)
            .position(|&b| b == b'\n')
            .ok_or(EnvelopeError::NotEnvelope)?;
        let env: Envelope =
            serde_json::from_slice(&data[..end]).map_err(|_| EnvelopeError::NotEnvelope)?;
        if env.crucible_envelope != ENVELOPE_VERSION || env.alg != ALG_AGE_X25519 {
            return Err(EnvelopeError::Unsupported);
        }
        Ok((env, &data[end + 1..]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut file = Envelope::new("abcd").header_line();
        file.extend_from_slice(b"age-encryption.org/v1\n...");
        let (env, body) = Envelope::split(&file).unwrap();
        assert_eq!(env.key_id, "abcd");
        assert!(body.starts_with(b"age-encryption.org/v1"));
    }

    #[test]
    fn rejects() {
        assert_eq!(
            Envelope::split(b"no newline"),
            Err(EnvelopeError::NotEnvelope)
        );
        assert_eq!(
            Envelope::split(b"{\"crucible_envelope\":2,\"alg\":\"age-x25519\",\"key_id\":\"x\"}\n"),
            Err(EnvelopeError::Unsupported)
        );
    }
}
