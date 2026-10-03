//! Blob naming and sharding.
//!
//! 与 crucible-store 保持一致 (keep in sync with `crucible-store`): the store
//! crate pulls in tokio/reqwest and does not build for wasm32, so the rules
//! are restated here and pinned by the same test vectors as
//! `crucible-store/src/lib.rs`.
//!
//! - blob name = lower-case hex SHA-256 of the stored (sealed) bytes;
//! - 32 pre-release Releases `blobs-00` … `blobs-31`;
//! - shard = first byte of the hash >> 3 (the first 5 bits).

use sha2::{Digest, Sha256};

pub const SHARDS: u8 = 32;

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// Lower-case, 64 hex chars; also what makes a hash safe as an asset name,
/// URL path segment and KV key component.
pub fn is_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn shard(hash: &str) -> Option<u8> {
    if !is_hash(hash) {
        return None;
    }
    let first = u8::from_str_radix(&hash[..2], 16).ok()?;
    Some(first >> 3)
}

/// Release tag holding a blob: `blobs-00` … `blobs-31`.
pub fn release_tag(hash: &str) -> Option<String> {
    Some(format!("blobs-{:02}", shard(hash)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Same vectors as crucible-store's `sharding` test.
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
        assert!(is_hash(&sha256_hex(b"x")));
        for bad in [
            "",
            "abc",
            &"A".repeat(64),
            &"g".repeat(64),
            &format!("../{}", "a".repeat(61)),
            &"a".repeat(65),
        ] {
            assert!(!is_hash(bad), "{bad}");
            assert!(shard(bad).is_none());
        }
    }
}
