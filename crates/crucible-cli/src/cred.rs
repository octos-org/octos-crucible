//! Where the model credential of a run comes from. Every source yields the
//! same thing: one JSON line `{"api_key","endpoint"}` on stdout, piped
//! straight into `crucible run` (never a file, never an env var).
//!
//! Sources:
//! - `github-secret` (development): a repository secret holding the
//!   credential sealed to the platform key (`crucible seal` output, raw or
//!   base64), opened with the platform private key.
//! - `workers-kv`: the user's sealed credential from Workers KV, fetched
//!   with `GET <worker>/internal/cred/<eval_id>` (Bearer
//!   `CRUCIBLE_WORKER_TOKEN`). It is `{api_key, endpoint, download_password,
//!   eval_id}`; the eval id inside must match the run's, so a credential
//!   cannot be replayed into another evaluation.

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

/// What the meter gets.
#[derive(Deserialize, Serialize)]
struct Cred {
    api_key: String,
    endpoint: String,
}

/// The whole opened credential.
#[derive(Deserialize)]
struct FullCred {
    api_key: String,
    endpoint: String,
    #[serde(default)]
    download_password: Option<String>,
    #[serde(default)]
    eval_id: Option<String>,
}

/// Open a sealed credential and check the eval id inside (required when
/// `expect_eval_id` is given).
fn open_full(sealed: &[u8], keys: &[PrivateKey], expect_eval_id: Option<&str>) -> Result<FullCred> {
    let plain = crucible_crypto::open(keys, &sealed_bytes(sealed)?)?;
    let cred: FullCred = serde_json::from_slice(&plain)
        .map_err(|_| anyhow!("credential is not {{\"api_key\",\"endpoint\"}} JSON"))?;
    drop(plain);
    if let Some(want) = expect_eval_id
        && cred.eval_id.as_deref() != Some(want)
    {
        bail!("credential belongs to another evaluation");
    }
    Ok(cred)
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
    expect_eval_id: Option<&str>,
) -> Result<String> {
    let full = open_full(sealed, keys, expect_eval_id)?;
    let mut cred = Cred {
        api_key: full.api_key,
        endpoint: full.endpoint,
    };
    if let Some(e) = endpoint.filter(|e| !e.trim().is_empty()) {
        cred.endpoint = e.trim().to_owned();
    }
    if cred.api_key.trim().is_empty() {
        bail!("credential has an empty api_key");
    }
    validate_endpoint(&cred.endpoint, false)?;
    Ok(serde_json::to_string(&cred)?)
}

/// The download password of a sealed credential (eval id checked); `None`
/// when it has none.
pub fn download_password(
    sealed: &[u8],
    keys: &[PrivateKey],
    expect_eval_id: &str,
) -> Result<Option<String>> {
    Ok(open_full(sealed, keys, Some(expect_eval_id))?
        .download_password
        .filter(|p| !p.is_empty()))
}

/// Read the sealed credential for `source`; `workers-kv` needs the Worker
/// and the eval id.
pub async fn read_sealed(
    source: CredSource,
    sealed_env: &str,
    worker: Option<(&crate::worker::Worker, &str)>,
) -> Result<Vec<u8>> {
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
        CredSource::WorkersKv => {
            let (w, eval_id) =
                worker.ok_or_else(|| anyhow!("workers-kv needs --worker-url and --eval-id"))?;
            w.get_cred(eval_id).await
        }
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
        let line = open_credential(&sealed, std::slice::from_ref(&sk), None, None).unwrap();
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
            None,
        )
        .unwrap();
        assert!(line.contains("api.example.com"));
        assert!(open_credential(&sealed, &[sk], Some("http://127.0.0.1/v1"), None).is_err());
        assert!(open_credential(b"garbage", &[PrivateKey::generate()], None, None).is_err());
    }

    #[test]
    fn eval_id_checked_and_password_kept_out_of_the_meter_line() {
        let sk = PrivateKey::generate();
        let keys = std::slice::from_ref(&sk);
        let plain = br#"{"api_key":"sk-S","endpoint":"https://api.z.ai/v4","download_password":"pw-1","eval_id":"ev-0000001"}"#;
        let sealed = crucible_crypto::seal(&sk.public(), plain).unwrap();
        let line = open_credential(&sealed, keys, None, Some("ev-0000001")).unwrap();
        assert!(!line.contains("pw-1") && !line.contains("ev-0000001"));
        assert!(open_credential(&sealed, keys, None, Some("ev-0000002")).is_err());
        assert_eq!(
            download_password(&sealed, keys, "ev-0000001")
                .unwrap()
                .as_deref(),
            Some("pw-1")
        );
        assert!(download_password(&sealed, keys, "other-eval").is_err());
        // Development credentials carry no eval id.
        let dev = crucible_crypto::seal(
            &sk.public(),
            br#"{"api_key":"k","endpoint":"https://a.b/v1"}"#,
        )
        .unwrap();
        assert!(open_credential(&dev, keys, None, Some("ev-0000001")).is_err());
    }
}
