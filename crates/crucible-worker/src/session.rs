//! HMAC-SHA256 signed tokens: the browser session (`Authorization: Bearer`)
//! and the OAuth `state` parameter. Each kind is MACed under its own
//! purpose label so one can never be replayed as the other.
//!
//! Format: `v1.<base64url(payload JSON)>.<base64url(mac)>`.

use hmac::{Hmac, Mac};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::util::{b64url, b64url_decode};

type HmacSha256 = Hmac<Sha256>;

pub const SESSION_TTL_S: u64 = 24 * 3600;
pub const STATE_TTL_S: u64 = 600;
const MAX_TOKEN: usize = 2048;
/// Tolerated clock skew for `iat`.
const SKEW_S: u64 = 60;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    /// GitHub numeric user id.
    pub gid: u64,
    pub login: String,
    pub iat: u64,
    pub exp: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct State {
    /// Nonce, also stored in the short-lived OAuth cookie.
    n: String,
    exp: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum TokenError {
    Malformed,
    BadSignature,
    Expired,
}

fn mac(key: &[u8], purpose: &str, signed: &str) -> HmacSha256 {
    let mut m = HmacSha256::new_from_slice(key).expect("HMAC takes any key length");
    m.update(purpose.as_bytes());
    m.update(&[0]);
    m.update(signed.as_bytes());
    m
}

fn sign<T: Serialize>(key: &[u8], purpose: &str, payload: &T) -> String {
    let body = format!(
        "v1.{}",
        b64url(&serde_json::to_vec(payload).expect("payload serializes"))
    );
    let tag = mac(key, purpose, &body).finalize().into_bytes();
    format!("{body}.{}", b64url(&tag))
}

fn open<T: DeserializeOwned>(key: &[u8], purpose: &str, token: &str) -> Result<T, TokenError> {
    if token.len() > MAX_TOKEN {
        return Err(TokenError::Malformed);
    }
    let (body, tag) = token.rsplit_once('.').ok_or(TokenError::Malformed)?;
    let payload = body.strip_prefix("v1.").ok_or(TokenError::Malformed)?;
    let tag = b64url_decode(tag).ok_or(TokenError::Malformed)?;
    // `verify_slice` compares in constant time.
    mac(key, purpose, body)
        .verify_slice(&tag)
        .map_err(|_| TokenError::BadSignature)?;
    let raw = b64url_decode(payload).ok_or(TokenError::Malformed)?;
    serde_json::from_slice(&raw).map_err(|_| TokenError::Malformed)
}

pub fn issue_session(key: &[u8], gid: u64, login: &str, now: u64) -> String {
    sign(
        key,
        "session",
        &Session {
            gid,
            login: login.to_owned(),
            iat: now,
            exp: now + SESSION_TTL_S,
        },
    )
}

pub fn verify_session(key: &[u8], token: &str, now: u64) -> Result<Session, TokenError> {
    let s: Session = open(key, "session", token)?;
    if s.exp <= now || s.iat > now + SKEW_S || s.exp - s.iat > SESSION_TTL_S {
        return Err(TokenError::Expired);
    }
    Ok(s)
}

pub fn issue_state(key: &[u8], nonce: &str, now: u64) -> String {
    sign(
        key,
        "oauth-state",
        &State {
            n: nonce.to_owned(),
            exp: now + STATE_TTL_S,
        },
    )
}

/// Returns the nonce the state was issued for.
pub fn verify_state(key: &[u8], state: &str, now: u64) -> Result<String, TokenError> {
    let s: State = open(key, "oauth-state", state)?;
    if s.exp <= now {
        return Err(TokenError::Expired);
    }
    Ok(s.n)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = b"0123456789abcdef0123456789abcdef";
    const NOW: u64 = 1_790_985_600;

    #[test]
    fn session_round_trip() {
        let t = issue_session(KEY, 42, "octocat", NOW);
        let s = verify_session(KEY, &t, NOW + 10).unwrap();
        assert_eq!((s.gid, s.login.as_str()), (42, "octocat"));
        assert_eq!(s.exp, NOW + SESSION_TTL_S);
    }

    #[test]
    fn session_expiry() {
        let t = issue_session(KEY, 42, "octocat", NOW);
        assert!(verify_session(KEY, &t, NOW + SESSION_TTL_S - 1).is_ok());
        assert_eq!(
            verify_session(KEY, &t, NOW + SESSION_TTL_S),
            Err(TokenError::Expired)
        );
        // Issued in the future (clock skew beyond tolerance).
        assert_eq!(
            verify_session(KEY, &t, NOW - 3600),
            Err(TokenError::Expired)
        );
    }

    #[test]
    fn session_tamper() {
        let t = issue_session(KEY, 42, "octocat", NOW);
        assert_eq!(
            verify_session(b"another key another key another!", &t, NOW),
            Err(TokenError::BadSignature)
        );
        // Swap in a payload claiming another user, keep the MAC.
        let (_, tag) = t.rsplit_once('.').unwrap();
        let forged_payload = b64url(
            &serde_json::to_vec(&Session {
                gid: 1,
                login: "admin".into(),
                iat: NOW,
                exp: NOW + 100,
            })
            .unwrap(),
        );
        let forged = format!("v1.{forged_payload}.{tag}");
        assert_eq!(
            verify_session(KEY, &forged, NOW),
            Err(TokenError::BadSignature)
        );
        for junk in [
            "",
            "v1",
            "v1..",
            "v2.e30.AAAA",
            "a.b.c.d",
            &"x".repeat(4000),
        ] {
            assert!(verify_session(KEY, junk, NOW).is_err(), "{junk}");
        }
    }

    #[test]
    fn purposes_do_not_mix() {
        let state = issue_state(KEY, "nonce", NOW);
        assert_eq!(verify_state(KEY, &state, NOW).unwrap(), "nonce");
        assert!(verify_session(KEY, &state, NOW).is_err());
        let session = issue_session(KEY, 42, "octocat", NOW);
        assert!(verify_state(KEY, &session, NOW).is_err());
        assert_eq!(
            verify_state(KEY, &state, NOW + STATE_TTL_S),
            Err(TokenError::Expired)
        );
    }
}
