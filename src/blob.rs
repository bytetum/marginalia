//! Filesystem `BlobStore`: content-addressed files named `<sha256 hex>.<ext>` under one directory.

use std::io;
use std::path::PathBuf;

use async_trait::async_trait;
use sha2::{Digest, Sha256};

use crate::repo::{BlobStore, RepoError, RepoResult, new_id};

pub struct FsBlobStore {
    root: PathBuf,
}

impl FsBlobStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

/// 1 to 10 ASCII alphanumeric characters.
fn valid_extension(ext: &str) -> bool {
    (1..=10).contains(&ext.len()) && ext.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// `<64 lowercase hex chars>.<valid extension>`. Anything else cannot name a file in the root.
fn valid_key(key: &str) -> bool {
    match key.split_once('.') {
        Some((hash, ext)) => {
            hash.len() == 64
                && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
                && valid_extension(ext)
        }
        None => false,
    }
}

fn backend(e: io::Error) -> RepoError {
    RepoError::Backend(e.to_string())
}

#[async_trait]
impl BlobStore for FsBlobStore {
    async fn put(&self, bytes: &[u8], extension: &str) -> RepoResult<String> {
        if !valid_extension(extension) {
            return Err(RepoError::Invalid(format!(
                "blob extension must be 1-10 ASCII alphanumeric characters, got {extension:?}"
            )));
        }
        let key = format!("{}.{}", hex::encode(Sha256::digest(bytes)), extension);
        let path = self.root.join(&key);
        if tokio::fs::try_exists(&path).await.map_err(backend)? {
            return Ok(key);
        }

        tokio::fs::create_dir_all(&self.root).await.map_err(backend)?;
        // Write to a temp file and rename, so a concurrent get never sees a partial blob.
        let tmp = self.root.join(format!(".{}.tmp", new_id()));
        tokio::fs::write(&tmp, bytes).await.map_err(backend)?;
        if let Err(e) = tokio::fs::rename(&tmp, &path).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(backend(e));
        }
        Ok(key)
    }

    async fn get(&self, key: &str) -> RepoResult<Option<Vec<u8>>> {
        if !valid_key(key) {
            return Ok(None);
        }
        match tokio::fs::read(self.root.join(key)).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(backend(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const HELLO_SHA256: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    #[tokio::test]
    async fn put_then_get_roundtrips() {
        let dir = tempdir().unwrap();
        let store = FsBlobStore::new(dir.path());
        let key = store.put(b"<html>hi</html>", "html").await.unwrap();
        assert!(key.ends_with(".html"));
        assert_eq!(
            store.get(&key).await.unwrap(),
            Some(b"<html>hi</html>".to_vec())
        );
    }

    #[tokio::test]
    async fn same_bytes_same_key_and_root_is_created() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("nested").join("blobs");
        let store = FsBlobStore::new(&root);

        let k1 = store.put(b"hello", "html").await.unwrap();
        let k2 = store.put(b"hello", "html").await.unwrap();
        assert_eq!(k1, k2);
        assert_eq!(k1, format!("{HELLO_SHA256}.html"));

        let other = store.put(b"hello!", "html").await.unwrap();
        assert_ne!(other, k1);

        // Exactly two blobs, no temp files left behind.
        let names: Vec<String> = std::fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(names.iter().all(|n| !n.ends_with(".tmp")));
    }

    #[tokio::test]
    async fn existing_blob_is_not_rewritten() {
        let dir = tempdir().unwrap();
        let store = FsBlobStore::new(dir.path());
        let key = store.put(b"original", "txt").await.unwrap();
        // Tamper with the stored file. A second put of the same bytes must skip the write.
        std::fs::write(dir.path().join(&key), b"tampered").unwrap();
        assert_eq!(store.put(b"original", "txt").await.unwrap(), key);
        assert_eq!(store.get(&key).await.unwrap(), Some(b"tampered".to_vec()));
    }

    #[tokio::test]
    async fn invalid_or_missing_keys_return_none() {
        let dir = tempdir().unwrap();
        let store = FsBlobStore::new(dir.path());
        let key = store.put(b"x", "txt").await.unwrap();
        let hash = &key[..64];

        let bad = [
            "../../etc/passwd".to_string(),
            format!("../{key}"),
            format!("{hash}/../{key}"),
            format!("{hash}.tx/t"),
            format!("{hash}."),
            format!("{hash}.txt.txt"),
            format!("{}.txt", hash.to_uppercase()),
            "zz.txt".to_string(),
            String::new(),
            "missing".to_string(),
        ];
        for k in &bad {
            assert_eq!(store.get(k).await.unwrap(), None, "key {k:?}");
        }

        // Well-formed but never stored.
        let absent = format!("{}.txt", "0".repeat(64));
        assert_eq!(store.get(&absent).await.unwrap(), None);
    }

    #[tokio::test]
    async fn invalid_extension_is_rejected() {
        let dir = tempdir().unwrap();
        let store = FsBlobStore::new(dir.path());

        for ext in ["", ".html", "ht ml", "../x", "h-tml", "toolongextension"] {
            let err = store.put(b"x", ext).await.unwrap_err();
            assert!(matches!(err, RepoError::Invalid(_)), "ext {ext:?}");
        }
        // Boundary: exactly 10 alphanumerics is fine.
        assert!(store.put(b"x", "abcdefghij").await.is_ok());
    }
}
