//! Content-addressed blob storage. Everything stored is already encrypted;
//! the store only sees opaque bytes named by their SHA-256.
//!
//! Location is computed from the hash, so no index is needed, and every
//! read can be verified against its name.

mod github;
mod local;

use std::future::Future;

use sha2::{Digest, Sha256};

pub use github::GithubReleaseStore;
pub use local::LocalDirStore;

/// Number of release shards (`blobs-00` … `blobs-31`).
pub const SHARDS: u8 = 32;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("not a SHA-256 hex digest: {0:?}")]
    BadHash(String),
    #[error("blob {0} not found")]
    NotFound(String),
    #[error("blob {0} failed its hash check")]
    Corrupt(String),
    #[error("store: {0}")]
    Io(#[from] std::io::Error),
    #[error("GitHub API: {0}")]
    Api(String),
    #[error("store config: {0}")]
    Config(String),
}

pub trait BlobStore {
    /// Store `data` under its SHA-256; idempotent. Returns the hex digest.
    fn put(&self, data: &[u8]) -> impl Future<Output = Result<String, StoreError>> + Send;
    /// Fetch a blob and verify it against `hash`.
    fn get(&self, hash: &str) -> impl Future<Output = Result<Vec<u8>, StoreError>> + Send;
    fn exists(&self, hash: &str) -> impl Future<Output = Result<bool, StoreError>> + Send;
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// Lower-case, 64 hex chars. Also what makes a hash safe as a file or asset
/// name.
pub fn check_hash(hash: &str) -> Result<(), StoreError> {
    let ok = hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if ok {
        Ok(())
    } else {
        Err(StoreError::BadHash(hash.chars().take(80).collect()))
    }
}

/// Shard = the first 5 bits of the hash (first byte >> 3), 0..=31.
pub fn shard(hash: &str) -> Result<u8, StoreError> {
    check_hash(hash)?;
    let first = u8::from_str_radix(&hash[..2], 16).expect("checked hex");
    Ok(first >> 3)
}

/// Release tag holding a blob: `blobs-00` … `blobs-31`.
pub fn release_tag(hash: &str) -> Result<String, StoreError> {
    Ok(format!("blobs-{:02}", shard(hash)?))
}

fn verify(hash: &str, data: Vec<u8>) -> Result<Vec<u8>, StoreError> {
    if sha256_hex(&data) == hash {
        Ok(data)
    } else {
        Err(StoreError::Corrupt(hash.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sharding() {
        let h = |first: &str| format!("{first}{}", "0".repeat(62));
        assert_eq!(shard(&h("00")).unwrap(), 0);
        assert_eq!(shard(&h("07")).unwrap(), 0);
        assert_eq!(shard(&h("08")).unwrap(), 1);
        assert_eq!(shard(&h("7f")).unwrap(), 15);
        assert_eq!(shard(&h("80")).unwrap(), 16);
        assert_eq!(shard(&h("ff")).unwrap(), 31);
        assert_eq!(release_tag(&h("f8")).unwrap(), "blobs-31");
        assert_eq!(release_tag(&h("10")).unwrap(), "blobs-02");
        // sha256("") = e3b0c442...: 0xe3 >> 3 = 28
        assert_eq!(release_tag(&sha256_hex(b"")).unwrap(), "blobs-28");
    }

    #[test]
    fn every_shard_reachable() {
        let mut seen = [false; SHARDS as usize];
        for b in 0..=255u8 {
            seen[shard(&format!("{b:02x}{}", "a".repeat(62))).unwrap() as usize] = true;
        }
        assert!(seen.iter().all(|s| *s));
    }

    #[test]
    fn hash_check() {
        check_hash(&sha256_hex(b"x")).unwrap();
        for bad in [
            "",
            "abc",
            &"A".repeat(64),
            &"g".repeat(64),
            &format!("../{}", "a".repeat(61)),
            &"a".repeat(65),
        ] {
            assert!(check_hash(bad).is_err(), "{bad}");
        }
    }
}
