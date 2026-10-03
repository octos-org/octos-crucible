//! Where the model credential of a run comes from. Every source yields the
//! same thing: one JSON line `{"api_key","endpoint"}` on stdout, piped
//! straight into `crucible run` (never a file, never an env var).
//!
//! Sources:
//! - `github-secret` (development): a repository secret holding the
//!   credential sealed to the platform key (`crucible seal` output, raw or
//!   base64), opened with the platform private key.
//! - `workers-kv` (step 4): the user's sealed credential from Workers KV.

use anyhow::{Result, anyhow, bail};
use crucible_core::netpolicy::validate_endpoint;
use crucible_crypto::PrivateKey;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredSource {
    GithubSecret,
    WorkersKv,
}

impl CredSource {
    pub fn parse(s: &str) -> Result<CredSource> {
        match s {
            "github-secret" => Ok(CredSource::GithubSecret),
            "workers-kv" => Ok(CredSource::WorkersKv),
            _ => bail!("cred_source must be github-secret or workers-kv"),
        }
    }
}

#[derive(Deserialize, Serialize)]
struct Cred {
    api_key: String,
    endpoint: String,
}

fn b64_val(c: u8) -> Option<u32> {
    Some(match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' | b'-' => 62,
        b'/' | b'_' => 63,
        _ => return None,
    } as u32)
}

/// Standard base64 with padding, one line.
pub fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for ch in data.chunks(3) {
        let n = ch
            .iter()
            .enumerate()
            .fold(0u32, |a, (i, b)| a | (*b as u32) << (16 - 8 * i));
        for i in 0..4 {
            if i <= ch.len() {
                s.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

/// Standard or URL-safe base64, whitespace and padding ignored.
pub fn base64_decode(s: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0;
    for &c in s {
        if c.is_ascii_whitespace() || c == b'=' {
            continue;
        }
        acc = (acc << 6) | b64_val(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// A sealed blob, as stored raw or base64-encoded.
pub fn sealed_bytes(raw: &[u8]) -> Result<Vec<u8>> {
    let trimmed = raw.trim_ascii_start();
    if trimmed.starts_with(b"{\"crucible_envelope\"") {
        return Ok(trimmed.to_vec());
    }
    let decoded = base64_decode(trimmed)
        .ok_or_else(|| anyhow!("sealed credential is neither an envelope nor base64"))?;
    if !decoded.starts_with(b"{\"crucible_envelope\"") {
        bail!("sealed credential is not a crucible envelope");
    }
    Ok(decoded)
}

/// Open the sealed credential; `endpoint` (non-empty) overrides the one
/// inside. Returns the JSON line for the meter.
pub fn open_credential(
    sealed: &[u8],
    keys: &[PrivateKey],
    endpoint: Option<&str>,
) -> Result<String> {
    let plain = crucible_crypto::open(keys, &sealed_bytes(sealed)?)?;
    let mut cred: Cred = serde_json::from_slice(&plain)
        .map_err(|_| anyhow!("credential is not {{\"api_key\",\"endpoint\"}} JSON"))?;
    drop(plain);
    if let Some(e) = endpoint.filter(|e| !e.trim().is_empty()) {
        cred.endpoint = e.trim().to_owned();
    }
    if cred.api_key.trim().is_empty() {
        bail!("credential has an empty api_key");
    }
    validate_endpoint(&cred.endpoint, false)?;
    Ok(serde_json::to_string(&cred)?)
}

/// Read the sealed credential for `source`.
pub fn read_sealed(source: CredSource, sealed_env: &str) -> Result<Vec<u8>> {
    match source {
        CredSource::GithubSecret => {
            let v =
                std::env::var_os(sealed_env).ok_or_else(|| anyhow!("{sealed_env} is not set"))?;
            #[cfg(unix)]
            let bytes = {
                use std::os::unix::ffi::OsStrExt;
                v.as_bytes().to_vec()
            };
            #[cfg(not(unix))]
            let bytes = v.to_string_lossy().as_bytes().to_vec();
            if bytes.is_empty() {
                bail!("{sealed_env} is empty");
            }
            Ok(bytes)
        }
        CredSource::WorkersKv => bail!("cred_source workers-kv is not implemented yet (step 4)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b64() {
        assert_eq!(base64_decode(b"aGVsbG8=").unwrap(), b"hello");
        assert_eq!(base64_decode(b"aGVs\nbG8").unwrap(), b"hello");
        assert!(base64_decode(b"a*b").is_none());
        for n in 0..20 {
            let data: Vec<u8> = (0..n).map(|i| (i * 37 + 200) as u8).collect();
            assert_eq!(
                base64_decode(base64_encode(&data).as_bytes()).unwrap(),
                data
            );
        }
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
    }

    #[test]
    fn open_raw_and_base64_with_override() {
        let sk = PrivateKey::generate();
        let plain = br#"{"api_key":"sk-SECRET","endpoint":"https://api.z.ai/api/coding/paas/v4"}"#;
        let sealed = crucible_crypto::seal(&sk.public(), plain).unwrap();
        let line = open_credential(&sealed, std::slice::from_ref(&sk), None).unwrap();
        assert!(line.contains("sk-SECRET") && line.contains("api.z.ai"));
        assert!(!line.contains('\n'));
        let b64: String = {
            const T: &[u8; 64] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let mut s = String::new();
            for ch in sealed.chunks(3) {
                let n = ch
                    .iter()
                    .enumerate()
                    .fold(0u32, |a, (i, b)| a | (*b as u32) << (16 - 8 * i));
                for i in 0..=ch.len() {
                    s.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
                }
            }
            s
        };
        let line = open_credential(
            b64.as_bytes(),
            std::slice::from_ref(&sk),
            Some("https://api.example.com/v1"),
        )
        .unwrap();
        assert!(line.contains("api.example.com"));
        assert!(open_credential(&sealed, &[sk], Some("http://127.0.0.1/v1")).is_err());
        assert!(open_credential(b"garbage", &[PrivateKey::generate()], None).is_err());
    }
}
