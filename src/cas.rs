use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use tokio::fs;

#[derive(Clone, Debug)]
pub struct MediaStore {
    root: PathBuf,
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{:02x}", b);
    }
    s
}

impl MediaStore {
    pub async fn open(root: &Path) -> Result<Self> {
        fs::create_dir_all(root)
            .await
            .with_context(|| format!("Failed to create media store at {}", root.display()))?;
        Ok(Self {
            root: root.to_owned(),
        })
    }

    fn path_for(&self, hash: &str) -> PathBuf {
        self.root.join(hash)
    }

    /// Store data and return its SHA-256 hex hash. Writes are atomic (write to
    /// temp then rename) and idempotent.
    pub async fn store(&self, data: &[u8]) -> Result<String> {
        let hash = hex_encode(Sha256::digest(data).as_slice());
        let dest = self.path_for(&hash);

        if dest.exists() {
            return Ok(hash);
        }

        let tmp = dest.with_extension("tmp");
        fs::write(&tmp, data)
            .await
            .context("Failed to write CAS temp file")?;
        fs::rename(&tmp, &dest)
            .await
            .context("Failed to rename CAS temp file")?;

        Ok(hash)
    }

    pub async fn load(&self, hash: &str) -> Result<Vec<u8>> {
        if !hash.chars().all(|c| c.is_ascii_hexdigit()) || hash.len() != 64 {
            bail!("Invalid CAS hash: {}", hash);
        }
        fs::read(self.path_for(hash))
            .await
            .with_context(|| format!("Failed to read CAS object {}", hash))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn test_store() -> (MediaStore, TempDir) {
        let dir = TempDir::new().unwrap();
        let store = MediaStore::open(dir.path()).await.unwrap();
        (store, dir)
    }

    #[tokio::test]
    async fn store_and_load() {
        let (store, _dir) = test_store().await;
        let data = b"hello world";
        let hash = store.store(data).await.unwrap();
        assert_eq!(hash.len(), 64);

        let loaded = store.load(&hash).await.unwrap();
        assert_eq!(loaded, data);
    }

    #[tokio::test]
    async fn store_is_idempotent() {
        let (store, _dir) = test_store().await;
        let data = b"same content";
        let h1 = store.store(data).await.unwrap();
        let h2 = store.store(data).await.unwrap();
        assert_eq!(h1, h2);
    }

    #[tokio::test]
    async fn different_content_different_hash() {
        let (store, _dir) = test_store().await;
        let h1 = store.store(b"aaa").await.unwrap();
        let h2 = store.store(b"bbb").await.unwrap();
        assert_ne!(h1, h2);
    }

    #[tokio::test]
    async fn load_invalid_hash() {
        let (store, _dir) = test_store().await;
        assert!(store.load("not-a-hash").await.is_err());
    }
}
