//! Personal API tokens for the command line (`crucible submit`).
//!
//! Format: `crt_<id: 16 hex>_<secret: 64 hex>`. KV keeps only the SHA-256
//! of the whole token under `token/<id>`; the plaintext is shown once, when
//! it is created. A token acts as its owner on the user endpoints, never as
//! an administrator, and cannot manage tokens (that needs a web session).

use serde::{Deserialize, Serialize};

use crate::shard::sha256_hex;
use crate::util::ct_eq;

pub const PREFIX: &str = "crt_";
/// Tokens per user.
pub const MAX_PER_USER: usize = 20;
pub const MAX_NAME: usize = 60;

/// KV `token/<id>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenRecord {
    pub owner_id: u64,
    pub login: String,
    pub name: String,
    /// SHA-256 hex of the whole token.
    pub hash: String,
    pub created_at: String,
}

/// What `GET /tokens` lists (also the metadata of `tokens/<gid>/<id>`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenSummary {
    pub id: String,
    pub name: String,
    pub created_at: String,
}

pub fn is_id(s: &str) -> bool {
    s.len() == 16
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A new token from 8 + 32 random bytes: `(id, token)`.
pub fn mint(random: &[u8]) -> (String, String) {
    assert_eq!(random.len(), 40, "8 bytes of id, 32 of secret");
    let id = hex::encode(&random[..8]);
    let token = format!("{PREFIX}{id}_{}", hex::encode(&random[8..]));
    (id, token)
}

/// The id of a well-formed token (`None` for anything else, e.g. a session).
pub fn parse(token: &str) -> Option<&str> {
    let rest = token.strip_prefix(PREFIX)?;
    let (id, secret) = rest.split_once('_')?;
    let hex64 = secret.len() == 64
        && secret
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    (is_id(id) && hex64).then_some(id)
}

pub fn hash(token: &str) -> String {
    sha256_hex(token.as_bytes())
}

/// Constant-time check of a token against its stored record.
pub fn matches(rec: &TokenRecord, token: &str) -> bool {
    ct_eq(hash(token).as_bytes(), rec.hash.as_bytes())
}

/// Optional display name: trimmed, at most [`MAX_NAME`] characters, no
/// control characters; empty means `cli`.
pub fn clean_name(name: Option<&str>) -> Option<String> {
    let n = name.unwrap_or("").trim();
    if n.chars().count() > MAX_NAME || n.chars().any(char::is_control) {
        return None;
    }
    Some(if n.is_empty() { "cli".into() } else { n.into() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mint_parse_match() {
        let random: Vec<u8> = (0..40).collect();
        let (id, token) = mint(&random);
        assert_eq!(parse(&token), Some(id.as_str()));
        let rec = TokenRecord {
            owner_id: 1,
            login: "a".into(),
            name: "cli".into(),
            hash: hash(&token),
            created_at: String::new(),
        };
        assert!(matches(&rec, &token));
        let mut other = token.clone();
        other.pop();
        other.push('f');
        assert!(!matches(&rec, &other)); // token ends in "27"
        for junk in [
            "",
            "crt_",
            "crt_00_11",
            "v1.abc.def",
            &token[..token.len() - 1],
        ] {
            assert_eq!(parse(junk), None, "{junk}");
        }
        assert_eq!(parse(&token.to_uppercase()), None);
    }

    #[test]
    fn names() {
        assert_eq!(clean_name(None).as_deref(), Some("cli"));
        assert_eq!(clean_name(Some("  laptop ")).as_deref(), Some("laptop"));
        assert_eq!(clean_name(Some("a\nb")), None);
        assert_eq!(clean_name(Some(&"x".repeat(61))), None);
    }
}
