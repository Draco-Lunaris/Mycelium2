//! Encrypted file repository.
//!
//! Files are stored as flat, opaque HMAC filenames (no directory structure,
//! no path leakage) with a two-layer envelope:
//!
//! ```text
//! outer envelope (DEK derived from scope key + stored filename):
//!   plaintext = inner envelope bytes
//! inner envelope (meta DEK):
//!   plaintext = canonical path (JSON)   ← recoverable only with the key
//! ```
//!
//! The outer DEK is bound to the *stored filename* (not the path), so a
//! file swapped under a known name fails to decrypt. The inner envelope
//! carries the true path, verified on read — a file copied to another name
//! fails the path check.
//!
//! All file I/O is async (`tokio::fs`); the AEAD work is CPU-bound and cheap.

use std::path::{Path, PathBuf};

use mycelium_crypto::aead::{aead_open, aead_seal};
use mycelium_crypto::keys::{Dek, MasterKey, ServiceKey};
use mycelium_crypto::store_keys::FileKeys;
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum FileRepoError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("crypto error: {0}")]
    Crypto(#[from] mycelium_crypto::CryptoError),
    #[error("stored file {name} does not belong to the requested path")]
    PathMismatch { name: String },
    #[error("stored file {name} is corrupt: {reason}")]
    Corrupt { name: String, reason: String },
    #[error("invalid canonical path {path:?}: must be non-empty and start with '/'")]
    InvalidPath { path: String },
}

/// Which key scope a repo operates under.
#[derive(Debug, Clone)]
pub enum Scope {
    /// A user's private bundle (master-key derived FileKeys).
    User(MasterKey),
    /// Shared/global data (service-key derived FileKeys).
    Service(ServiceKey),
}

impl Scope {
    fn file_keys(&self) -> FileKeys {
        match self {
            Scope::User(master) => FileKeys::from_master_key(master),
            Scope::Service(service) => FileKeys::from_service_key(service),
        }
    }
}

/// Inner-envelope payload: the canonical path this file belongs to.
#[derive(Serialize, Deserialize)]
struct PathMeta {
    path: String,
}

/// Flat encrypted file repository rooted at a directory.
#[derive(Debug, Clone)]
pub struct FileRepo {
    base: PathBuf,
}

impl FileRepo {
    pub fn new(base: impl AsRef<Path>) -> Self {
        Self {
            base: base.as_ref().to_path_buf(),
        }
    }

    /// Validate a canonical bundle path (non-empty, leading `/`).
    fn validate(canonical_path: &str) -> Result<(), FileRepoError> {
        if canonical_path.is_empty() || !canonical_path.starts_with('/') {
            return Err(FileRepoError::InvalidPath {
                path: canonical_path.to_string(),
            });
        }
        Ok(())
    }

    /// Encrypt and write a file atomically (write to temp, fsync, rename).
    pub async fn write(
        &self,
        canonical_path: &str,
        plaintext: &[u8],
        scope: &Scope,
    ) -> Result<(), FileRepoError> {
        Self::validate(canonical_path)?;
        // Ensure the base directory exists (user dirs are created lazily).
        tokio::fs::create_dir_all(&self.base).await?;
        let keys = scope.file_keys();
        let name = keys.stored_name(canonical_path);
        let target = self.base.join(&name);

        // Inner envelope: the path metadata.
        let meta = PathMeta {
            path: canonical_path.to_string(),
        };
        let meta_json = serde_json::to_vec(&meta).map_err(|e| FileRepoError::Corrupt {
            name: name.clone(),
            reason: e.to_string(),
        })?;
        let inner = aead_seal(&meta_json, name.as_bytes(), keys.meta_dek())?;

        // Outer envelope: inner envelope + plaintext, keyed by stored name.
        let mut outer_plaintext = inner.len().to_le_bytes().to_vec();
        outer_plaintext.extend_from_slice(&inner);
        outer_plaintext.extend_from_slice(plaintext);
        let outer_dek = dek_for_name(&keys, &name)?;
        let outer = aead_seal(&outer_plaintext, name.as_bytes(), &outer_dek)?;

        // Atomic write: temp file in the same directory, then rename.
        let tmp = self
            .base
            .join(format!("{name}.tmp-{}", uuid::Uuid::new_v4()));
        tokio::fs::write(&tmp, &outer).await?;
        let f = tokio::fs::File::open(&tmp).await?;
        f.sync_all().await?;
        drop(f);
        tokio::fs::rename(&tmp, &target).await?;
        Ok(())
    }

    /// Read and decrypt a file; verifies the inner path matches.
    pub async fn read(
        &self,
        canonical_path: &str,
        scope: &Scope,
    ) -> Result<Vec<u8>, FileRepoError> {
        Self::validate(canonical_path)?;
        let keys = scope.file_keys();
        let name = keys.stored_name(canonical_path);
        let target = self.base.join(&name);
        let outer = tokio::fs::read(&target).await?;
        let outer_dek = dek_for_name(&keys, &name)?;
        let outer_plaintext = aead_open(&outer, name.as_bytes(), &outer_dek)?;

        // Split inner-length prefix from the content (checked arithmetic).
        if outer_plaintext.len() < 8 {
            return Err(FileRepoError::Corrupt {
                name,
                reason: "missing inner-length prefix".into(),
            });
        }
        let inner_len =
            usize::from_le_bytes(outer_plaintext[..8].try_into().expect("8-byte prefix"));
        let Some(content_start) = inner_len.checked_add(8) else {
            return Err(FileRepoError::Corrupt {
                name,
                reason: "inner length overflow".into(),
            });
        };
        if outer_plaintext.len() < content_start {
            return Err(FileRepoError::Corrupt {
                name,
                reason: "truncated inner envelope".into(),
            });
        }
        let inner = &outer_plaintext[8..content_start];
        let content = &outer_plaintext[content_start..];

        // Open the inner envelope and verify the path.
        let meta_json = aead_open(inner, name.as_bytes(), keys.meta_dek())?;
        let meta: PathMeta =
            serde_json::from_slice(&meta_json).map_err(|e| FileRepoError::Corrupt {
                name: name.clone(),
                reason: e.to_string(),
            })?;
        if meta.path != canonical_path {
            // Do NOT include the decrypted path in the error — it may be
            // another file's metadata (swap scenario).
            return Err(FileRepoError::PathMismatch { name });
        }
        Ok(content.to_vec())
    }

    /// Delete a file (no-op if absent).
    pub async fn delete(&self, canonical_path: &str, scope: &Scope) -> Result<(), FileRepoError> {
        Self::validate(canonical_path)?;
        let keys = scope.file_keys();
        let name = keys.stored_name(canonical_path);
        let target = self.base.join(&name);
        match tokio::fs::remove_file(&target).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// List stored (opaque) filenames — for maintenance/backup only.
    /// Filters temp files left behind by interrupted writes.
    pub async fn list(&self) -> Result<Vec<String>, FileRepoError> {
        let mut names = Vec::new();
        let mut entries = tokio::fs::read_dir(&self.base).await?;
        while let Some(entry) = entries.next_entry().await? {
            let file_name = entry.file_name().to_string_lossy().to_string();
            if file_name.starts_with('.') || file_name.contains(".tmp-") {
                continue;
            }
            names.push(file_name);
        }
        names.sort();
        Ok(names)
    }
}

/// The outer DEK is derived from the scope's FileKeys' meta material and
/// the stored name — bound to the filename so renamed files fail.
fn dek_for_name(keys: &FileKeys, name: &str) -> Result<Dek, FileRepoError> {
    // HKDF over the meta DEK material + stored name, purpose-bound.
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(None, keys.meta_dek().as_bytes());
    let mut info = b"mycelium2/file-dek/v1".to_vec();
    info.extend_from_slice(name.as_bytes());
    let mut material = [0u8; 32];
    hk.expand(&info, &mut material)
        .expect("32-byte output cannot exceed HKDF capacity");
    Ok(Dek::from_bytes(&material).expect("32 bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mycelium_crypto::ServiceKey;
    use mycelium_crypto::keys::generate_master_key;

    fn user_scope() -> Scope {
        Scope::User(generate_master_key())
    }

    fn service_scope() -> Scope {
        Scope::Service(ServiceKey::from_bytes(&[9u8; 32]).unwrap())
    }

    #[tokio::test]
    async fn round_trip_user_scope() {
        let dir = tempfile::tempdir().unwrap();
        let repo = FileRepo::new(dir.path());
        let scope = user_scope();
        repo.write("/notes/todo.md", b"body", &scope).await.unwrap();
        let read = repo.read("/notes/todo.md", &scope).await.unwrap();
        assert_eq!(read, b"body");
    }

    #[tokio::test]
    async fn round_trip_service_scope() {
        let dir = tempfile::tempdir().unwrap();
        let repo = FileRepo::new(dir.path());
        let scope = service_scope();
        repo.write("/skills/deploy.md", b"skill body", &scope)
            .await
            .unwrap();
        assert_eq!(
            repo.read("/skills/deploy.md", &scope).await.unwrap(),
            b"skill body"
        );
    }

    #[tokio::test]
    async fn filenames_are_opaque() {
        let dir = tempfile::tempdir().unwrap();
        let repo = FileRepo::new(dir.path());
        let scope = user_scope();
        repo.write("/notes/secret-path.md", b"x", &scope)
            .await
            .unwrap();
        let names = repo.list().await.unwrap();
        assert_eq!(names.len(), 1);
        assert_eq!(names[0].len(), 64);
        assert!(!names[0].contains("secret"));
        assert!(!names[0].contains("notes"));
    }

    #[tokio::test]
    async fn flat_namespace_no_directories() {
        let dir = tempfile::tempdir().unwrap();
        let repo = FileRepo::new(dir.path());
        let scope = user_scope();
        repo.write("/a/b/c/deep.md", b"x", &scope).await.unwrap();
        repo.write("/top.md", b"y", &scope).await.unwrap();
        // Both files land flat in base — no subdirectories created.
        let names = repo.list().await.unwrap();
        assert_eq!(names.len(), 2);
        assert!(dir.path().join(&names[0]).is_file());
        assert!(dir.path().join(&names[1]).is_file());
    }

    #[tokio::test]
    async fn wrong_scope_cannot_read() {
        let dir = tempfile::tempdir().unwrap();
        let repo = FileRepo::new(dir.path());
        repo.write("/notes/todo.md", b"body", &user_scope())
            .await
            .unwrap();
        // A different user's key derives a different filename → not found
        // (read errors rather than leaking existence).
        assert!(repo.read("/notes/todo.md", &user_scope()).await.is_err());
    }

    #[tokio::test]
    async fn swapped_file_fails_path_check() {
        let dir = tempfile::tempdir().unwrap();
        let repo = FileRepo::new(dir.path());
        let scope = user_scope();
        repo.write("/notes/a.md", b"content-a", &scope)
            .await
            .unwrap();
        repo.write("/notes/b.md", b"content-b", &scope)
            .await
            .unwrap();
        // Swap the two stored files.
        let keys = scope.file_keys();
        let name_a = keys.stored_name("/notes/a.md");
        let name_b = keys.stored_name("/notes/b.md");
        let pa = dir.path().join(&name_a);
        let pb = dir.path().join(&name_b);
        let tmp = dir.path().join("swap-tmp");
        std::fs::rename(&pa, &tmp).unwrap();
        std::fs::rename(&pb, &pa).unwrap();
        std::fs::rename(&tmp, &pb).unwrap();
        // Reading a.md now finds b's ciphertext under a's name. The outer
        // envelope is keyed by the stored name, so the swap fails there
        // (name-binding); even if it opened, the inner path check would
        // catch it. Either error is correct — never silent wrong data.
        let err = repo.read("/notes/a.md", &scope).await.unwrap_err();
        assert!(matches!(
            err,
            FileRepoError::PathMismatch { .. } | FileRepoError::Crypto(_)
        ));
    }

    #[tokio::test]
    async fn delete_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let repo = FileRepo::new(dir.path());
        let scope = user_scope();
        repo.write("/x.md", b"x", &scope).await.unwrap();
        repo.delete("/x.md", &scope).await.unwrap();
        repo.delete("/x.md", &scope).await.unwrap(); // no-op
        assert!(repo.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn overwrite_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let repo = FileRepo::new(dir.path());
        let scope = user_scope();
        repo.write("/x.md", b"v1", &scope).await.unwrap();
        repo.write("/x.md", b"v2 longer", &scope).await.unwrap();
        assert_eq!(repo.read("/x.md", &scope).await.unwrap(), b"v2 longer");
        assert_eq!(repo.list().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn non_canonical_paths_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let repo = FileRepo::new(dir.path());
        let scope = user_scope();
        assert!(matches!(
            repo.write("", b"x", &scope).await,
            Err(FileRepoError::InvalidPath { .. })
        ));
        assert!(matches!(
            repo.write("relative.md", b"x", &scope).await,
            Err(FileRepoError::InvalidPath { .. })
        ));
        assert!(matches!(
            repo.read("", &scope).await,
            Err(FileRepoError::InvalidPath { .. })
        ));
    }

    #[tokio::test]
    async fn unicode_path_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let repo = FileRepo::new(dir.path());
        let scope = user_scope();
        repo.write("/notes/日本語.md", b"unicode", &scope)
            .await
            .unwrap();
        assert_eq!(
            repo.read("/notes/日本語.md", &scope).await.unwrap(),
            b"unicode"
        );
    }

    #[tokio::test]
    async fn list_filters_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let repo = FileRepo::new(dir.path());
        let scope = user_scope();
        repo.write("/real.md", b"x", &scope).await.unwrap();
        // Simulate an interrupted write: leave a temp file behind.
        std::fs::write(dir.path().join("deadbeef.tmp-123"), b"partial").unwrap();
        let names = repo.list().await.unwrap();
        assert_eq!(names.len(), 1);
        assert_eq!(names[0].len(), 64);
    }
}
