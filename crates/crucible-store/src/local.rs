//! Directory-backed store for tests and local runs: `<root>/<sha256>`.

use std::path::PathBuf;

use crate::{BlobStore, StoreError, check_hash, sha256_hex, verify};

pub struct LocalDirStore {
    root: PathBuf,
}

impl LocalDirStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        LocalDirStore { root: root.into() }
    }

    fn path(&self, hash: &str) -> Result<PathBuf, StoreError> {
        check_hash(hash)?;
        Ok(self.root.join(hash))
    }
}

impl BlobStore for LocalDirStore {
    async fn put(&self, data: &[u8]) -> Result<String, StoreError> {
        let hash = sha256_hex(data);
        let path = self.path(&hash)?;
        if tokio::fs::try_exists(&path).await? {
            return Ok(hash);
        }
        tokio::fs::create_dir_all(&self.root).await?;
        // Write then rename: a reader never sees a half-written blob.
        let tmp = self
            .root
            .join(format!(".{hash}.{}.tmp", std::process::id()));
        tokio::fs::write(&tmp, data).await?;
        tokio::fs::rename(&tmp, &path).await?;
        Ok(hash)
    }

    async fn get(&self, hash: &str) -> Result<Vec<u8>, StoreError> {
        let path = self.path(hash)?;
        match tokio::fs::read(&path).await {
            Ok(data) => verify(hash, data),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(StoreError::NotFound(hash.to_owned()))
            }
            Err(e) => Err(e.into()),
        }
    }

    async fn exists(&self, hash: &str) -> Result<bool, StoreError> {
        Ok(tokio::fs::try_exists(self.path(hash)?).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn put_get_exists() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalDirStore::new(dir.path().join("blobs"));
        let h = store.put(b"hello").await.unwrap();
        assert_eq!(h, sha256_hex(b"hello"));
        assert_eq!(store.put(b"hello").await.unwrap(), h); // idempotent
        assert!(store.exists(&h).await.unwrap());
        assert_eq!(store.get(&h).await.unwrap(), b"hello");
        let missing = sha256_hex(b"nope");
        assert!(!store.exists(&missing).await.unwrap());
        assert!(matches!(
            store.get(&missing).await,
            Err(StoreError::NotFound(_))
        ));
        assert!(matches!(
            store.get("../etc/passwd").await,
            Err(StoreError::BadHash(_))
        ));
    }

    #[tokio::test]
    async fn corruption_detected() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalDirStore::new(dir.path());
        let h = store.put(b"hello").await.unwrap();
        std::fs::write(dir.path().join(&h), b"hellO").unwrap();
        assert!(matches!(store.get(&h).await, Err(StoreError::Corrupt(_))));
    }
}
