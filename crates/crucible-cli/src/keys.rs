//! Platform keys and blob stores as the CLI sees them.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use crucible_core::BlobRef;
use crucible_crypto::{PrivateKey, PublicKey};
use crucible_store::{BlobStore, GithubReleaseStore, LocalDirStore};
use serde::Deserialize;

#[derive(Deserialize)]
struct KeysFile {
    current: String,
    keys: Vec<KeyEntry>,
}

#[derive(Deserialize)]
struct KeyEntry {
    key_id: String,
    public_key: String,
}

/// The current public key of `config/keys.json`, checked against its id.
pub fn current_public_key(path: &Path) -> Result<PublicKey> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let f: KeysFile =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    let entry = f
        .keys
        .iter()
        .find(|k| k.key_id == f.current)
        .ok_or_else(|| anyhow!("keys.json: current key {} is not listed", f.current))?;
    let key: PublicKey = entry.public_key.parse()?;
    if key.key_id() != entry.key_id {
        bail!(
            "keys.json: key_id {} does not match its public key",
            entry.key_id
        );
    }
    Ok(key)
}

/// Private keys from files and/or environment variables. The variable is
/// read, parsed and dropped; its value is never printed.
pub fn load_identities(files: &[PathBuf], envs: &[String]) -> Result<Vec<PrivateKey>> {
    let mut keys = Vec::new();
    for p in files {
        let raw = std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
        keys.push(PrivateKey::parse(&raw)?);
    }
    for name in envs {
        let raw =
            std::env::var(name).map_err(|_| anyhow!("environment variable {name} is not set"))?;
        keys.push(
            PrivateKey::parse(&raw).map_err(|_| anyhow!("{name} does not hold an age identity"))?,
        );
    }
    if keys.is_empty() {
        bail!("no private key given (--identity or --identity-env)");
    }
    Ok(keys)
}

pub enum Store {
    Dir(LocalDirStore),
    Github(GithubReleaseStore),
}

impl Store {
    pub fn parse(spec: &str) -> Result<Store> {
        match spec.split_once(':') {
            Some(("dir", p)) if !p.is_empty() => Ok(Store::Dir(LocalDirStore::new(p))),
            Some(("github", repo)) => Ok(Store::Github(GithubReleaseStore::from_env(repo)?)),
            _ => bail!("--store must be dir:<path> or github:<owner>/<repo>"),
        }
    }

    pub async fn put(&self, data: &[u8]) -> Result<String> {
        Ok(match self {
            Store::Dir(s) => s.put(data).await?,
            Store::Github(s) => s.put(data).await?,
        })
    }

    pub async fn get(&self, hash: &str) -> Result<Vec<u8>> {
        Ok(match self {
            Store::Dir(s) => s.get(hash).await?,
            Store::Github(s) => s.get(hash).await?,
        })
    }

    /// Seal `plain` to `key`, store it, return its reference.
    pub async fn put_sealed(&self, key: &PublicKey, plain: &[u8]) -> Result<BlobRef> {
        let sealed = crucible_crypto::seal(key, plain)?;
        let sha256 = self.put(&sealed).await?;
        Ok(BlobRef {
            sha256,
            key_id: key.key_id(),
        })
    }

    /// Fetch, verify and open a sealed blob.
    pub async fn get_sealed(&self, blob: &BlobRef, keys: &[PrivateKey]) -> Result<Vec<u8>> {
        let sealed = self.get(&blob.sha256).await?;
        let found = crucible_crypto::sealed_key_id(&sealed)?;
        if found != blob.key_id {
            bail!(
                "blob {} is sealed with key {found}, expected {}",
                blob.sha256,
                blob.key_id
            );
        }
        Ok(crucible_crypto::open(keys, &sealed)?)
    }
}
