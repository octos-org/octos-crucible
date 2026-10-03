//! Agent packages: fetch (builtin / url / git) and validate. A package is
//! only ever bytes on the host: it becomes a `docker build` context, nothing
//! in it is executed here.

use std::io::Cursor;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use crucible_core::AgentSpec;
use serde::Serialize;

use crate::keys::Store;
use crate::zipdir::{self, ExtractLimits};
use crucible_crypto::PrivateKey;

pub const MAX_ZIP_BYTES: u64 = 50 << 20;
pub const PKG_LIMITS: ExtractLimits = ExtractLimits {
    max_files: 5_000,
    max_bytes: 200 << 20,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Builtin(String),
    Url(String),
    Git {
        url: String,
        git_ref: String,
    },
    /// A sealed agent zip in the blob store (uploaded through the website).
    Blob(String),
}

fn ref_ok(r: &str) -> bool {
    !r.is_empty()
        && r.len() <= 100
        && !r.starts_with('-')
        && !r.contains("..")
        && r.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._/-".contains(&b))
}

/// `https://` URL without credentials, whitespace or shell-hostile bytes.
fn https_ok(u: &str) -> bool {
    u.len() <= 2000
        && u.starts_with("https://")
        && u.len() > "https://".len()
        && !u.contains('@')
        && u.bytes()
            .all(|b| b.is_ascii_graphic() && !b"\"'`\\<>{}|^".contains(&b))
}

impl Source {
    pub fn parse(s: &str) -> Result<Source> {
        let (kind, rest) = s.split_once(':').unwrap_or((s, ""));
        match kind {
            "builtin" if crucible_core::is_slug(rest, 40) => Ok(Source::Builtin(rest.into())),
            "url" if https_ok(rest) => Ok(Source::Url(rest.into())),
            "blob" if crucible_core::blob::is_sha256_hex(rest) => Ok(Source::Blob(rest.into())),
            "git" => {
                let (url, git_ref) = rest
                    .rsplit_once('@')
                    .ok_or_else(|| anyhow!("git source must be git:https://host/repo@ref"))?;
                if !https_ok(url) || !ref_ok(git_ref) {
                    bail!("git source must be git:https://host/repo@ref");
                }
                Ok(Source::Git {
                    url: url.into(),
                    git_ref: git_ref.into(),
                })
            }
            _ => bail!(
                "agent_source must be builtin:<name>, url:<https zip> or git:<https url>@<ref>"
            ),
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Source::Builtin(_) => "builtin",
            Source::Url(_) => "url",
            Source::Git { .. } => "git",
            Source::Blob(_) => "blob",
        }
    }
}

/// What `crucible fetch` reports (and the manifest records).
#[derive(Debug, Serialize)]
pub struct Facts {
    pub source_kind: &'static str,
    /// Never the raw URL of a `url:` source (it may be a signed link).
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zip_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// `blob:` sources: the sealed package as stored.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package_key_id: Option<String>,
    pub agent: AgentSpec,
}

/// `agent.json` + `Dockerfile` as regular files at the package root.
pub fn validate(pkg: &Path) -> Result<AgentSpec> {
    let regular = |name: &str| std::fs::symlink_metadata(pkg.join(name)).is_ok_and(|m| m.is_file());
    if !regular("Dockerfile") {
        bail!("package has no Dockerfile (regular file) at its root");
    }
    if !regular("agent.json") {
        bail!("package has no agent.json (regular file) at its root");
    }
    let raw = std::fs::read(pkg.join("agent.json"))?;
    if raw.len() > 64 << 10 {
        bail!("agent.json is larger than 64 KiB");
    }
    let spec: AgentSpec =
        serde_json::from_slice(&raw).map_err(|_| anyhow!("agent.json is not valid"))?;
    spec.validate()?;
    Ok(spec)
}

fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let ft = e.file_type()?;
        let to = dst.join(e.file_name());
        if ft.is_dir() {
            copy_tree(&e.path(), &to)?;
        } else if ft.is_file() {
            std::fs::copy(e.path(), &to)?;
        }
    }
    Ok(())
}

async fn download(url: &str) -> Result<Vec<u8>> {
    let client = reqwest::Client::builder()
        .user_agent("crucible-fetch/1")
        .timeout(std::time::Duration::from_secs(300))
        .build()?;
    // Errors carry the class only: the URL may be a signed link.
    let mut resp = client
        .get(url)
        .send()
        .await
        .map_err(|_| anyhow!("download failed"))?;
    if !resp.status().is_success() {
        bail!("download failed: HTTP {}", resp.status().as_u16());
    }
    let mut buf = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|_| anyhow!("download failed"))? {
        buf.extend_from_slice(&chunk);
        if buf.len() as u64 > MAX_ZIP_BYTES {
            bail!("agent zip exceeds {} MB", MAX_ZIP_BYTES >> 20);
        }
    }
    Ok(buf)
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_LFS_SKIP_SMUDGE", "1")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .context("running git")?;
    if !out.status.success() {
        bail!("git {} failed", args.first().unwrap_or(&""));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Fetch `source` into `out` (replaced) and validate it.
pub async fn fetch(
    source: &Source,
    builtin_dir: &Path,
    out: &Path,
    blob_access: Option<&(Store, Vec<PrivateKey>)>,
) -> Result<Facts> {
    if out.exists() {
        std::fs::remove_dir_all(out)?;
    }
    let mut facts = Facts {
        source_kind: source.kind(),
        source: String::new(),
        zip_sha256: None,
        commit: None,
        package_sha256: None,
        package_key_id: None,
        agent: AgentSpec {
            schema: 1,
            name: "x".into(),
            version: "0".into(),
            entrypoint: None,
            streaming: false,
            app_start_cmd: None,
        },
    };
    match source {
        Source::Builtin(name) => {
            let dir = builtin_dir.join(name);
            if !dir.is_dir() {
                bail!("unknown builtin agent {name:?}");
            }
            copy_tree(&dir, out)?;
            facts.source = format!("builtin:{name}");
        }
        Source::Url(url) => {
            let data = download(url).await?;
            facts.zip_sha256 = Some(crucible_store::sha256_hex(&data));
            zipdir::safe_extract(Cursor::new(data), out, PKG_LIMITS)
                .context("agent zip rejected")?;
            facts.source = "url".into();
        }
        Source::Blob(sha) => {
            let (store, keys) =
                blob_access.ok_or_else(|| anyhow!("blob: source needs a store and a key"))?;
            let sealed = store.get(sha).await?;
            let key_id = crucible_crypto::sealed_key_id(&sealed)?;
            let data = crucible_crypto::open(keys, &sealed)?;
            drop(sealed);
            if data.len() as u64 > MAX_ZIP_BYTES {
                bail!("agent zip exceeds {} MB", MAX_ZIP_BYTES >> 20);
            }
            facts.zip_sha256 = Some(crucible_store::sha256_hex(&data));
            zipdir::safe_extract(Cursor::new(data), out, PKG_LIMITS)
                .context("agent zip rejected")?;
            facts.source = format!("blob:{sha}");
            facts.package_sha256 = Some(sha.clone());
            facts.package_key_id = Some(key_id);
        }
        Source::Git { url, git_ref } => {
            std::fs::create_dir_all(out)?;
            git(out, &["init", "-q"])?;
            git(
                out,
                &[
                    "-c",
                    "protocol.allow=never",
                    "-c",
                    "protocol.https.allow=always",
                    "fetch",
                    "-q",
                    "--depth",
                    "1",
                    "--no-recurse-submodules",
                    "--",
                    url,
                    git_ref,
                ],
            )?;
            git(
                out,
                &[
                    "-c",
                    "advice.detachedHead=false",
                    "checkout",
                    "-q",
                    "FETCH_HEAD",
                ],
            )?;
            facts.commit = Some(git(out, &["rev-parse", "HEAD"])?);
            std::fs::remove_dir_all(out.join(".git"))?;
            facts.source = format!("git:{url}@{git_ref}");
        }
    }
    facts.agent = validate(out)?;
    Ok(facts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sources() {
        assert_eq!(
            Source::parse("builtin:octos").unwrap(),
            Source::Builtin("octos".into())
        );
        assert_eq!(
            Source::parse("git:https://github.com/o/r@v1.2").unwrap(),
            Source::Git {
                url: "https://github.com/o/r".into(),
                git_ref: "v1.2".into()
            }
        );
        Source::parse("url:https://example.com/a.zip?sig=x").unwrap();
        let h = "ab".repeat(32);
        assert_eq!(
            Source::parse(&format!("blob:{h}")).unwrap(),
            Source::Blob(h)
        );
        for bad in [
            "builtin:../x",
            "builtin:",
            "url:http://example.com/a.zip",
            "url:https://u:p@example.com/a.zip",
            "url:https://example.com/a b.zip",
            "url:https://x/$(id)'",
            "git:https://github.com/o/r",
            "git:https://github.com/o/r@--upload-pack=x",
            "git:https://github.com/o/r@a..b",
            "git:file:///etc@main",
            "ftp:x",
            "blob:abc",
            "blob:../x",
        ] {
            assert!(Source::parse(bad).is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn builtin_fetch_and_validate() {
        let root = tempfile::tempdir().unwrap();
        let pkg = root.path().join("agents/demo");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("Dockerfile"), "FROM scratch").unwrap();
        std::fs::write(
            pkg.join("agent.json"),
            r#"{"schema":1,"name":"demo","streaming":true}"#,
        )
        .unwrap();
        let out = root.path().join("out");
        let f = fetch(
            &Source::Builtin("demo".into()),
            &root.path().join("agents"),
            &out,
            None,
        )
        .await
        .unwrap();
        assert_eq!(f.agent.name, "demo");
        assert!(f.agent.streaming);
        assert!(
            fetch(
                &Source::Builtin("nope".into()),
                &root.path().join("agents"),
                &out,
                None
            )
            .await
            .is_err()
        );
        std::fs::remove_file(pkg.join("Dockerfile")).unwrap();
        assert!(validate(&pkg).is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/hosts", pkg.join("Dockerfile")).unwrap();
            assert!(validate(&pkg).is_err());
        }
    }

    #[tokio::test]
    async fn blob_fetch() {
        use std::io::Write;
        let root = tempfile::tempdir().unwrap();
        let mut z = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let o = zip::write::SimpleFileOptions::default();
        z.start_file("agent.json", o).unwrap();
        z.write_all(br#"{"schema":1,"name":"up"}"#).unwrap();
        z.start_file("Dockerfile", o).unwrap();
        z.write_all(b"FROM scratch").unwrap();
        let zip = z.finish().unwrap().into_inner();
        let sk = PrivateKey::generate();
        let store = Store::parse(&format!("dir:{}", root.path().join("store").display())).unwrap();
        let blob = store.put_sealed(&sk.public(), &zip).await.unwrap();
        let out = root.path().join("out");
        let access = (store, vec![sk]);
        let f = fetch(
            &Source::Blob(blob.sha256.clone()),
            root.path(),
            &out,
            Some(&access),
        )
        .await
        .unwrap();
        assert_eq!(f.agent.name, "up");
        assert_eq!(f.package_sha256.as_deref(), Some(blob.sha256.as_str()));
        assert_eq!(f.package_key_id.as_deref(), Some(blob.key_id.as_str()));
        let wrong = (access.0, vec![PrivateKey::generate()]);
        assert!(
            fetch(&Source::Blob(blob.sha256), root.path(), &out, Some(&wrong))
                .await
                .is_err()
        );
    }
}
