//! Store-scoped key derivations: opaque filenames and index keys.
//!
//! The file repository stores ciphertext under flat, opaque filenames so
//! the directory listing leaks nothing about concept paths. The search
//! index stores HMAC'd tokens so plaintext terms never touch the database.

use ring::hmac::{Context, HMAC_SHA256, Key};

use crate::keys::{Dek, MasterKey, ServiceKey};

/// HKDF info strings for store-scoped derivations.
const FILENAME_INFO: &[u8] = b"mycelium2/filename/v1";
const META_DEK_INFO: &[u8] = b"mycelium2/meta-dek/v1";
const INDEX_TOKEN_INFO: &[u8] = b"mycelium2/index-token/v1";
const INDEX_DEK_INFO: &[u8] = b"mycelium2/index-dek/v1";

/// Keys for the file repository: opaque filenames + a metadata DEK.
///
/// - `filename_key`: HMAC-SHA256 over the canonical path → hex filename.
///   Deterministic: the same path always maps to the same stored file.
/// - `meta_dek`: encrypts the path metadata (the inner envelope of the
///   two-layer format) so the plaintext path is recoverable only with the
///   master key.
#[derive(Clone)]
pub struct FileKeys {
    filename_key: Key,
    meta_dek: Dek,
}

impl FileKeys {
    /// Derive file-repository keys from a user's master key.
    pub fn from_master_key(master: &MasterKey) -> Self {
        Self {
            filename_key: derive_hmac_key(master, FILENAME_INFO),
            meta_dek: derive_dek_from(master, META_DEK_INFO),
        }
    }

    /// Derive file-repository keys from the service key (global scopes).
    pub fn from_service_key(service: &ServiceKey) -> Self {
        Self {
            filename_key: derive_hmac_key_service(service, FILENAME_INFO),
            meta_dek: derive_dek_from_service(service, META_DEK_INFO),
        }
    }

    /// Opaque stored filename for a canonical path: 64 hex chars (no
    /// extension, no directory structure — flat namespace).
    pub fn stored_name(&self, canonical_path: &str) -> String {
        let mut ctx = Context::with_key(&self.filename_key);
        ctx.update(canonical_path.as_bytes());
        let tag = ctx.sign();
        hex::encode(tag)
    }

    /// The metadata DEK (inner envelope key).
    pub fn meta_dek(&self) -> &Dek {
        &self.meta_dek
    }
}

/// Keys for the encrypted search index.
///
/// - `token_key`: HMAC-SHA256 over a search term → hex token. Deterministic
///   so equal terms collide in the index; irreversible so the database
///   never sees plaintext terms.
/// - `index_dek`: encrypts index documents (title/snippet payloads).
#[derive(Clone)]
pub struct IndexKeys {
    token_key: Key,
    index_dek: Dek,
}

impl IndexKeys {
    /// Derive index keys from a user's master key.
    pub fn from_master_key(master: &MasterKey) -> Self {
        Self {
            token_key: derive_hmac_key(master, INDEX_TOKEN_INFO),
            index_dek: derive_dek_from(master, INDEX_DEK_INFO),
        }
    }

    /// Derive index keys from the service key (global scopes).
    pub fn from_service_key(service: &ServiceKey) -> Self {
        Self {
            token_key: derive_hmac_key_service(service, INDEX_TOKEN_INFO),
            index_dek: derive_dek_from_service(service, INDEX_DEK_INFO),
        }
    }

    /// Opaque token for a search term (hex HMAC).
    pub fn token(&self, term: &str) -> String {
        let mut ctx = Context::with_key(&self.token_key);
        ctx.update(term.as_bytes());
        hex::encode(ctx.sign())
    }

    /// The index document DEK.
    pub fn index_dek(&self) -> &Dek {
        &self.index_dek
    }
}

impl std::fmt::Debug for FileKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FileKeys(<redacted>)")
    }
}

impl std::fmt::Debug for IndexKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("IndexKeys(<redacted>)")
    }
}

fn derive_hmac_key(master: &MasterKey, info: &[u8]) -> Key {
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(None, master.as_bytes());
    let mut material = [0u8; 32];
    hk.expand(info, &mut material)
        .expect("32-byte output cannot exceed HKDF capacity");
    Key::new(HMAC_SHA256, &material)
}

fn derive_hmac_key_service(service: &ServiceKey, info: &[u8]) -> Key {
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(None, service.as_bytes());
    let mut material = [0u8; 32];
    hk.expand(info, &mut material)
        .expect("32-byte output cannot exceed HKDF capacity");
    Key::new(HMAC_SHA256, &material)
}

fn derive_dek_from(master: &MasterKey, info: &[u8]) -> Dek {
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(None, master.as_bytes());
    let mut material = [0u8; 32];
    hk.expand(info, &mut material)
        .expect("32-byte output cannot exceed HKDF capacity");
    Dek::from_bytes(&material).expect("32 bytes")
}

fn derive_dek_from_service(service: &ServiceKey, info: &[u8]) -> Dek {
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(None, service.as_bytes());
    let mut material = [0u8; 32];
    hk.expand(info, &mut material)
        .expect("32-byte output cannot exceed HKDF capacity");
    Dek::from_bytes(&material).expect("32 bytes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::{ServiceKey, generate_master_key};

    fn service_key() -> ServiceKey {
        ServiceKey::from_bytes(&[7u8; 32]).unwrap()
    }

    #[test]
    fn stored_name_is_deterministic_and_opaque() {
        let master = generate_master_key();
        let keys = FileKeys::from_master_key(&master);
        let a = keys.stored_name("/notes/todo.md");
        let b = keys.stored_name("/notes/todo.md");
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        // No path leakage.
        assert!(!a.contains("notes"));
        // Different path → different name.
        assert_ne!(a, keys.stored_name("/notes/other.md"));
    }

    #[test]
    fn stored_name_differs_by_key() {
        let master_a = generate_master_key();
        let master_b = generate_master_key();
        let a = FileKeys::from_master_key(&master_a).stored_name("/x.md");
        let b = FileKeys::from_master_key(&master_b).stored_name("/x.md");
        assert_ne!(a, b);
    }

    #[test]
    fn service_scope_keys_differ_from_user_scope() {
        let master = generate_master_key();
        let user = FileKeys::from_master_key(&master);
        let service = FileKeys::from_service_key(&service_key());
        assert_ne!(user.stored_name("/x.md"), service.stored_name("/x.md"));
    }

    #[test]
    fn index_token_is_deterministic_and_opaque() {
        let master = generate_master_key();
        let keys = IndexKeys::from_master_key(&master);
        let a = keys.token("rust");
        let b = keys.token("rust");
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        assert!(!a.contains("rust"));
        assert_ne!(a, keys.token("cargo"));
    }

    #[test]
    fn debug_is_redacted() {
        let master = generate_master_key();
        let fk = FileKeys::from_master_key(&master);
        let ik = IndexKeys::from_master_key(&master);
        assert!(format!("{fk:?}").contains("<redacted>"));
        assert!(format!("{ik:?}").contains("<redacted>"));
    }
}
