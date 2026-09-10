//! Key derivation and generation.
//!
//! - `derive_kek`: password → Argon2id → `Kek`.
//! - `derive_dek`: `MasterKey` + file path → HKDF-SHA256 → `Dek`.
//! - `generate_*`: CSPRNG material via ring (SystemRandom).
//!
//! Key types are newtypes so a `Kek` can never be accidentally used as a
//! `MasterKey` (or vice versa) — mixing them up would silently produce
//! unrecoverable data.

use argon2::{Algorithm, Argon2, Params, Version};
use hkdf::Hkdf;
use ring::rand::{SecureRandom, SystemRandom};
use sha2::Sha256;

/// Argon2id parameters (OWASP 2024 baseline for 32-byte keys).
const ARGON2_M_KIB: u32 = 19 * 1024; // 19 MiB
const ARGON2_T_ITER: u32 = 2;
const ARGON2_P_LANES: u32 = 1;

/// Salt length for Argon2id (16 bytes is the RFC 9106 recommendation).
pub const SALT_LEN: usize = 16;

/// Recovery keys are 32 random bytes, hex-encoded with a prefix (64 hex chars).
pub const RECOVERY_KEY_LEN: usize = 32;

/// HKDF info string binding DEKs to their purpose.
const DEK_INFO: &[u8] = b"mycelium2/dek/v1";

/// A key encryption key derived from a password. Wraps master keys only.
#[derive(Clone)]
pub struct Kek([u8; 32]);

/// A user's master key. Sealed at rest; derives per-file DEKs.
#[derive(Clone)]
pub struct MasterKey([u8; 32]);

/// A per-file data encryption key, bound to one canonical path.
#[derive(Clone)]
pub struct Dek([u8; 32]);

/// A recovery key (parsed from its hex string form).
#[derive(Clone)]
pub struct RecoveryKey([u8; 32]);

/// The server-wide service key for global/shared data.
#[derive(Clone)]
pub struct ServiceKey([u8; 32]);

macro_rules! impl_key_common {
    ($name:ident, $doc:expr) => {
        #[doc = $doc]
        impl $name {
            /// Raw 32-byte key material (for ring calls).
            pub fn as_bytes(&self) -> &[u8; 32] {
                &self.0
            }

            /// Construct from raw bytes (32 required).
            pub fn from_bytes(bytes: &[u8]) -> Result<Self, KeyError> {
                let arr: [u8; 32] = bytes.try_into().map_err(|_| KeyError::InvalidLength {
                    expected: 32,
                    got: bytes.len(),
                })?;
                Ok(Self(arr))
            }
        }
    };
}

impl_key_common!(Kek, "Key encryption key (password-derived).");
impl_key_common!(MasterKey, "User master key.");
impl_key_common!(Dek, "Per-file data encryption key.");
impl_key_common!(RecoveryKey, "Recovery key.");
impl_key_common!(ServiceKey, "Server-wide service key.");

impl std::fmt::Debug for Kek {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Kek(<redacted>)")
    }
}
impl std::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MasterKey(<redacted>)")
    }
}
impl std::fmt::Debug for Dek {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Dek(<redacted>)")
    }
}
impl std::fmt::Debug for RecoveryKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RecoveryKey(<redacted>)")
    }
}
impl std::fmt::Debug for ServiceKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ServiceKey(<redacted>)")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    #[error("argon2 derivation failed")]
    Argon2,
    #[error("invalid key length: expected {expected}, got {got}")]
    InvalidLength { expected: usize, got: usize },
    #[error("invalid recovery key format (expected 'myc2-recovery-' + 64 hex chars)")]
    InvalidRecoveryKey,
    #[error("invalid file path for DEK derivation: {path:?}")]
    InvalidPath { path: String },
}

/// Derive a `Kek` from a password and salt using Argon2id.
///
/// Deterministic: same (password, salt) → same KEK. The salt must be stored
/// alongside the sealed master key; the password never is.
pub fn derive_kek(password: &str, salt: &[u8]) -> Result<Kek, KeyError> {
    if salt.len() != SALT_LEN {
        return Err(KeyError::InvalidLength {
            expected: SALT_LEN,
            got: salt.len(),
        });
    }
    let params = Params::new(ARGON2_M_KIB, ARGON2_T_ITER, ARGON2_P_LANES, Some(32))
        .map_err(|_| KeyError::Argon2)?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut kek = [0u8; 32];
    argon
        .hash_password_into(password.as_bytes(), salt, &mut kek)
        .map_err(|_| KeyError::Argon2)?;
    Ok(Kek(kek))
}

/// Derive a per-file `Dek` from the master key and the file's canonical path.
///
/// The path binding means a ciphertext moved to another path (or a bundle
/// with renamed files) fails to decrypt — an extra tamper check. The path
/// must be non-empty and start with `/` (canonical bundle form).
pub fn derive_dek(master_key: &MasterKey, path: &str) -> Result<Dek, KeyError> {
    if path.is_empty() || !path.starts_with('/') {
        return Err(KeyError::InvalidPath {
            path: path.to_string(),
        });
    }
    // Single expansion over purpose || path: simple, sound, and no
    // degenerate all-zero case.
    let hk = Hkdf::<Sha256>::new(None, master_key.as_bytes());
    let mut info = Vec::with_capacity(DEK_INFO.len() + path.len());
    info.extend_from_slice(DEK_INFO);
    info.extend_from_slice(path.as_bytes());
    let mut dek = [0u8; 32];
    hk.expand(&info, &mut dek)
        .expect("32-byte output cannot exceed HKDF capacity");
    Ok(Dek(dek))
}

/// Generate a random 16-byte Argon2 salt.
pub fn generate_salt() -> [u8; SALT_LEN] {
    let mut salt = [0u8; SALT_LEN];
    let rng = SystemRandom::new();
    rng.fill(&mut salt).expect("SystemRandom failure");
    salt
}

/// Generate a random 32-byte master key.
pub fn generate_master_key() -> MasterKey {
    let mut key = [0u8; 32];
    let rng = SystemRandom::new();
    rng.fill(&mut key).expect("SystemRandom failure");
    MasterKey(key)
}

/// Generate a recovery key: `myc2-recovery-` + 64 hex chars (32 random bytes).
/// Shown to the user once at account creation.
pub fn generate_recovery_key() -> String {
    let mut bytes = [0u8; RECOVERY_KEY_LEN];
    let rng = SystemRandom::new();
    rng.fill(&mut bytes).expect("SystemRandom failure");
    format!("myc2-recovery-{}", hex::encode(bytes))
}

/// Parse a recovery key string into a `RecoveryKey`.
pub fn parse_recovery_key(key: &str) -> Result<RecoveryKey, KeyError> {
    let hex_part = key
        .strip_prefix("myc2-recovery-")
        .ok_or(KeyError::InvalidRecoveryKey)?;
    let bytes = hex::decode(hex_part).map_err(|_| KeyError::InvalidRecoveryKey)?;
    if bytes.len() != RECOVERY_KEY_LEN {
        return Err(KeyError::InvalidRecoveryKey);
    }
    let mut out = [0u8; RECOVERY_KEY_LEN];
    out.copy_from_slice(&bytes);
    Ok(RecoveryKey(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn master_key_is_random() {
        let a = generate_master_key();
        let b = generate_master_key();
        assert_ne!(a.as_bytes(), b.as_bytes());
    }

    #[test]
    fn salt_is_random_and_sized() {
        let a = generate_salt();
        let b = generate_salt();
        assert_ne!(a, b);
        assert_eq!(a.len(), SALT_LEN);
    }

    #[test]
    fn recovery_key_round_trip() {
        let key = generate_recovery_key();
        let parsed = parse_recovery_key(&key).unwrap();
        assert_eq!(parsed.as_bytes().len(), RECOVERY_KEY_LEN);
        assert_eq!(key.len(), "myc2-recovery-".len() + 64);
    }

    #[test]
    fn recovery_key_rejects_bad_input() {
        assert!(parse_recovery_key("nope").is_err());
        assert!(parse_recovery_key("myc2-recovery-zzzz").is_err());
        assert!(parse_recovery_key("myc2-recovery-abc").is_err());
    }

    #[test]
    fn kek_is_deterministic() {
        let salt = generate_salt();
        let a = derive_kek("correct horse battery staple", &salt).unwrap();
        let b = derive_kek("correct horse battery staple", &salt).unwrap();
        assert_eq!(a.as_bytes(), b.as_bytes());
    }

    #[test]
    fn kek_differs_by_password_and_salt() {
        let salt = generate_salt();
        let a = derive_kek("password-a", &salt).unwrap();
        let b = derive_kek("password-b", &salt).unwrap();
        assert_ne!(a.as_bytes(), b.as_bytes());
        let salt2 = generate_salt();
        let c = derive_kek("password-a", &salt2).unwrap();
        assert_ne!(a.as_bytes(), c.as_bytes());
    }

    #[test]
    fn kek_rejects_wrong_salt_length() {
        let err = derive_kek("pw", &[0u8; 8]).unwrap_err();
        assert!(matches!(err, KeyError::InvalidLength { .. }));
    }

    #[test]
    fn dek_is_deterministic_and_path_bound() {
        let master = generate_master_key();
        let a = derive_dek(&master, "/notes/todo.md").unwrap();
        let b = derive_dek(&master, "/notes/todo.md").unwrap();
        assert_eq!(a.as_bytes(), b.as_bytes());
        let c = derive_dek(&master, "/notes/other.md").unwrap();
        assert_ne!(a.as_bytes(), c.as_bytes());
        let master2 = generate_master_key();
        let d = derive_dek(&master2, "/notes/todo.md").unwrap();
        assert_ne!(a.as_bytes(), d.as_bytes());
    }

    #[test]
    fn dek_rejects_invalid_paths() {
        let master = generate_master_key();
        assert!(matches!(
            derive_dek(&master, ""),
            Err(KeyError::InvalidPath { .. })
        ));
        assert!(matches!(
            derive_dek(&master, "notes/relative.md"),
            Err(KeyError::InvalidPath { .. })
        ));
    }

    #[test]
    fn dek_handles_unicode_and_long_paths() {
        let master = generate_master_key();
        let a = derive_dek(&master, "/notes/日本語.md").unwrap();
        let b = derive_dek(&master, "/notes/日本語.md").unwrap();
        assert_eq!(a.as_bytes(), b.as_bytes());
        let long = format!("/notes/{}.md", "x".repeat(4000));
        let c = derive_dek(&master, &long).unwrap();
        let d = derive_dek(&master, &long).unwrap();
        assert_eq!(c.as_bytes(), d.as_bytes());
    }

    #[test]
    fn key_newtypes_reject_wrong_length() {
        assert!(MasterKey::from_bytes(&[0u8; 31]).is_err());
        assert!(Kek::from_bytes(&[0u8; 33]).is_err());
    }

    #[test]
    fn key_debug_is_redacted() {
        let master = generate_master_key();
        let dbg = format!("{master:?}");
        assert!(dbg.contains("<redacted>"));
        assert!(!dbg.contains("0x"));
    }
}
