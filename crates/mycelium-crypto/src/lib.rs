//! Per-user encryption and key management.
//!
//! Key hierarchy (DESIGN.md §6):
//! - User password → Argon2id → key encryption key (KEK).
//! - KEK wraps the user's 32-byte master key (sealed at rest).
//! - Master key + HKDF-SHA256 over the file path → per-file data
//!   encryption key (DEK). Password changes only re-wrap the master key;
//!   file ciphertext is untouched.
//! - A service key (server-managed) encrypts global bookshelves, shared
//!   library stacks, and the global skills shelf.

pub mod aead;
pub mod envelope;
pub mod file;
pub mod keys;
pub mod seal;
pub mod service;
pub mod store_keys;

pub use aead::{aead_open, aead_seal};
pub use envelope::{ENVELOPE_MAGIC, ENVELOPE_VERSION, Envelope};
pub use file::{decrypt_file, encrypt_file};
pub use keys::{
    Dek, Kek, KeyError, MasterKey, RECOVERY_KEY_LEN, RecoveryKey, SALT_LEN, ServiceKey, derive_dek,
    derive_kek, generate_master_key, generate_recovery_key, generate_salt, parse_recovery_key,
};
pub use seal::{
    SEALED_VERSION, SealError, SealedMasterKey, change_password, rotate_recovery_key,
    seal_master_key, unseal_with_password, unseal_with_recovery,
};
pub use service::{ServiceKeyError, load_or_create_service_key, load_or_create_service_key_with};
pub use store_keys::{FileKeys, IndexKeys};

/// Unified error for the crypto layer.
#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("key error: {0}")]
    Key(#[from] KeyError),
    #[error("seal error: {0}")]
    Seal(#[from] SealError),
    #[error("service key error: {0}")]
    ServiceKey(#[from] ServiceKeyError),
    #[error("envelope error: {0}")]
    Envelope(#[from] EnvelopeError),
    #[error("encryption failed: {0}")]
    Encrypt(String),
    #[error("decryption failed: {0}")]
    Decrypt(String),
}

/// Envelope format errors.
#[derive(Debug, thiserror::Error)]
pub enum EnvelopeError {
    #[error("input too short for an envelope ({len} bytes)")]
    TooShort { len: usize },
    #[error("bad envelope magic")]
    BadMagic,
    #[error("unsupported envelope version: {0}")]
    UnsupportedVersion(u8),
}
