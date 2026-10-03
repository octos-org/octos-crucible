//! Reference to a stored file: its content address plus the key that
//! sealed it.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobRef {
    /// Lower-case hex SHA-256 of the stored (sealed) bytes.
    pub sha256: String,
    pub key_id: String,
}

impl BlobRef {
    pub fn is_valid(&self) -> bool {
        is_sha256_hex(&self.sha256)
            && !self.key_id.is_empty()
            && self.key_id.len() <= 64
            && self.key_id.bytes().all(|b| b.is_ascii_alphanumeric())
    }
}

/// 64 lower-case hex characters.
pub fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validity() {
        let ok = BlobRef {
            sha256: "ab".repeat(32),
            key_id: "1ffa702796eb5ee8".into(),
        };
        assert!(ok.is_valid());
        let mut bad = ok.clone();
        bad.sha256 = "AB".repeat(32);
        assert!(!bad.is_valid());
        let mut bad = ok;
        bad.key_id = "../x".into();
        assert!(!bad.is_valid());
    }
}
