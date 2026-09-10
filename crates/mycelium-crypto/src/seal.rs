//! Sealed master keys: the KEK/DEK split that makes password changes cheap.
//!
//! The user's 32-byte master key is sealed twice — once under the password
//! KEK and once under the recovery key — so the user can regain access with
//! either. Changing the password re-wraps the master key; file ciphertext
//! (encrypted under path-derived DEKs) is untouched.
//!
//! The record carries a `version` so future KDF-parameter upgrades can be
//! distinguished from old records (reject unknown versions loudly).

use serde::{Deserialize, Serialize};

use crate::CryptoError;
use crate::aead::{aead_open, aead_seal};
use crate::keys::{
    Dek, MasterKey, RecoveryKey, SALT_LEN, derive_kek, generate_salt, parse_recovery_key,
};

/// Current sealed-record format version.
pub const SEALED_VERSION: u8 = 1;

/// AAD purpose strings binding each seal to its use.
const PASSWORD_SEAL_AAD: &[u8] = b"mycelium2/seal/password/v1";
const RECOVERY_SEAL_AAD: &[u8] = b"mycelium2/seal/recovery/v1";

/// The sealed master key record persisted for each user.
///
/// `salt` + `password_sealed` let the password unwrap the master key;
/// `recovery_sealed` lets the recovery key do the same. The master key
/// itself never appears in plaintext.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SealedMasterKey {
    /// Record format version (1 today).
    pub version: u8,
    /// Argon2id salt for the password KEK (16 bytes, hex).
    pub salt_hex: String,
    /// Master key sealed under the password KEK (envelope bytes, hex).
    pub password_sealed_hex: String,
    /// Master key sealed under the recovery key (envelope bytes, hex).
    pub recovery_sealed_hex: String,
}

#[derive(Debug, thiserror::Error)]
pub enum SealError {
    #[error("unseal failed: wrong password or recovery key, or corrupted record")]
    UnsealFailed,
    #[error("unsupported sealed-record version: {0}")]
    UnsupportedVersion(u8),
    #[error("hex decode failed: {0}")]
    Hex(String),
}

fn hex_err(e: hex::FromHexError) -> SealError {
    SealError::Hex(e.to_string())
}

/// Seal a master key under a password and a recovery key.
///
/// Callers generate the master key (`keys::generate_master_key`) and the
/// recovery key (`keys::generate_recovery_key`) once at account creation,
/// show the recovery key to the user, then persist this record.
pub fn seal_master_key(
    master_key: &MasterKey,
    password: &str,
    recovery_key: &str,
) -> Result<SealedMasterKey, CryptoError> {
    // Validate the recovery key BEFORE the expensive Argon2 derivation.
    let recovery: RecoveryKey = parse_recovery_key(recovery_key)?;
    let salt = generate_salt();
    let kek = derive_kek(password, &salt)?;
    let password_sealed = aead_seal(
        master_key.as_bytes(),
        PASSWORD_SEAL_AAD,
        &wrap_as_dek(kek.as_bytes()),
    )?;
    let recovery_sealed = aead_seal(
        master_key.as_bytes(),
        RECOVERY_SEAL_AAD,
        &wrap_as_dek(recovery.as_bytes()),
    )?;
    Ok(SealedMasterKey {
        version: SEALED_VERSION,
        salt_hex: hex::encode(salt),
        password_sealed_hex: hex::encode(password_sealed),
        recovery_sealed_hex: hex::encode(recovery_sealed),
    })
}

/// Unseal the master key with the account password.
pub fn unseal_with_password(
    sealed: &SealedMasterKey,
    password: &str,
) -> Result<MasterKey, CryptoError> {
    check_version(sealed)?;
    let salt = hex::decode(&sealed.salt_hex).map_err(hex_err)?;
    // A wrong-length salt here means a corrupted record (derive_kek's
    // InvalidLength is a programming error at creation time).
    if salt.len() != SALT_LEN {
        return Err(SealError::UnsealFailed.into());
    }
    let kek = derive_kek(password, &salt)?;
    let sealed_bytes = hex::decode(&sealed.password_sealed_hex).map_err(hex_err)?;
    let master = aead_open(
        &sealed_bytes,
        PASSWORD_SEAL_AAD,
        &wrap_as_dek(kek.as_bytes()),
    )?;
    Ok(MasterKey::from_bytes(&master)?)
}

/// Unseal the master key with the recovery key.
pub fn unseal_with_recovery(
    sealed: &SealedMasterKey,
    recovery_key: &str,
) -> Result<MasterKey, CryptoError> {
    check_version(sealed)?;
    let recovery = parse_recovery_key(recovery_key)?;
    let sealed_bytes = hex::decode(&sealed.recovery_sealed_hex).map_err(hex_err)?;
    let master = aead_open(
        &sealed_bytes,
        RECOVERY_SEAL_AAD,
        &wrap_as_dek(recovery.as_bytes()),
    )?;
    Ok(MasterKey::from_bytes(&master)?)
}

/// Change the account password: re-wrap the master key under the new
/// password's KEK. The recovery seal and all file ciphertext are untouched.
pub fn change_password(
    sealed: &SealedMasterKey,
    old_password: &str,
    new_password: &str,
) -> Result<SealedMasterKey, CryptoError> {
    let master = unseal_with_password(sealed, old_password)?;
    let salt = generate_salt();
    let kek = derive_kek(new_password, &salt)?;
    let password_sealed = aead_seal(
        master.as_bytes(),
        PASSWORD_SEAL_AAD,
        &wrap_as_dek(kek.as_bytes()),
    )?;
    Ok(SealedMasterKey {
        version: SEALED_VERSION,
        salt_hex: hex::encode(salt),
        password_sealed_hex: hex::encode(password_sealed),
        recovery_sealed_hex: sealed.recovery_sealed_hex.clone(),
    })
}

/// Rotate the recovery key: re-wrap the master key under a new recovery key.
/// The password seal and all file ciphertext are untouched.
pub fn rotate_recovery_key(
    sealed: &SealedMasterKey,
    password: &str,
    new_recovery_key: &str,
) -> Result<SealedMasterKey, CryptoError> {
    // Validate the new recovery key before unsealing.
    let recovery: RecoveryKey = parse_recovery_key(new_recovery_key)?;
    let master = unseal_with_password(sealed, password)?;
    let recovery_sealed = aead_seal(
        master.as_bytes(),
        RECOVERY_SEAL_AAD,
        &wrap_as_dek(recovery.as_bytes()),
    )?;
    Ok(SealedMasterKey {
        version: SEALED_VERSION,
        salt_hex: sealed.salt_hex.clone(),
        password_sealed_hex: sealed.password_sealed_hex.clone(),
        recovery_sealed_hex: hex::encode(recovery_sealed),
    })
}

fn check_version(sealed: &SealedMasterKey) -> Result<(), CryptoError> {
    if sealed.version != SEALED_VERSION {
        return Err(SealError::UnsupportedVersion(sealed.version).into());
    }
    Ok(())
}

/// The seal paths encrypt the 32-byte master key with AES-GCM, whose key
/// type in this crate is `Dek`. Wrapping a KEK/RecoveryKey as a Dek here is
/// safe: it is an internal, purpose-bound conversion for the seal operation
/// only (the AAD strings prevent cross-use with file DEKs).
fn wrap_as_dek(key: &[u8; 32]) -> Dek {
    Dek::from_bytes(key).expect("32-byte key")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::{generate_master_key, generate_recovery_key};

    fn fixture() -> (SealedMasterKey, MasterKey, String, String) {
        let master = generate_master_key();
        let recovery = generate_recovery_key();
        let password = "correct horse battery staple";
        let sealed = seal_master_key(&master, password, &recovery).unwrap();
        (sealed, master, password.to_string(), recovery)
    }

    #[test]
    fn unseal_with_password_and_recovery() {
        let (sealed, master, password, recovery) = fixture();
        assert_eq!(
            unseal_with_password(&sealed, &password).unwrap().as_bytes(),
            master.as_bytes()
        );
        assert_eq!(
            unseal_with_recovery(&sealed, &recovery).unwrap().as_bytes(),
            master.as_bytes()
        );
    }

    #[test]
    fn wrong_password_fails() {
        let (sealed, _master, _password, _recovery) = fixture();
        assert!(unseal_with_password(&sealed, "wrong").is_err());
    }

    #[test]
    fn wrong_recovery_key_fails() {
        let (sealed, _master, _password, _recovery) = fixture();
        let other = generate_recovery_key();
        assert!(unseal_with_recovery(&sealed, &other).is_err());
    }

    #[test]
    fn password_change_keeps_files_decryptable() {
        let (sealed, master, password, recovery) = fixture();
        // Encrypt a file under the master key before the change.
        let ct = crate::file::encrypt_file(b"data", "/x.md", &master).unwrap();
        let resealed = change_password(&sealed, &password, "new password 20 chars").unwrap();
        // Old password no longer works on the new record.
        assert!(unseal_with_password(&resealed, &password).is_err());
        // New password unwraps the SAME master key.
        let master2 = unseal_with_password(&resealed, "new password 20 chars").unwrap();
        assert_eq!(master2.as_bytes(), master.as_bytes());
        // File ciphertext still decrypts.
        assert_eq!(
            crate::file::decrypt_file(&ct, "/x.md", &master2).unwrap(),
            b"data"
        );
        // Recovery seal carried over unchanged and still works.
        assert_eq!(resealed.recovery_sealed_hex, sealed.recovery_sealed_hex);
        assert_eq!(
            unseal_with_recovery(&resealed, &recovery)
                .unwrap()
                .as_bytes(),
            master.as_bytes()
        );
    }

    #[test]
    fn password_change_to_same_password_works_with_fresh_salt() {
        let (sealed, _master, password, _recovery) = fixture();
        let resealed = change_password(&sealed, &password, &password).unwrap();
        // Fresh salt, still unsealable.
        assert_ne!(resealed.salt_hex, sealed.salt_hex);
        assert!(unseal_with_password(&resealed, &password).is_ok());
    }

    #[test]
    fn password_change_requires_old_password() {
        let (sealed, _master, _password, _recovery) = fixture();
        assert!(change_password(&sealed, "wrong old", "new").is_err());
    }

    #[test]
    fn recovery_rotation_keeps_password_seal() {
        let (sealed, master, password, _recovery) = fixture();
        let new_recovery = generate_recovery_key();
        let rotated = rotate_recovery_key(&sealed, &password, &new_recovery).unwrap();
        // Password seal untouched.
        assert_eq!(rotated.password_sealed_hex, sealed.password_sealed_hex);
        // New recovery key works and yields the same master.
        assert_eq!(
            unseal_with_recovery(&rotated, &new_recovery)
                .unwrap()
                .as_bytes(),
            master.as_bytes()
        );
        // Old recovery key no longer works.
        assert!(
            unseal_with_recovery(
                &rotated,
                "myc2-recovery-0000000000000000000000000000000000000000000000000000000000000000"
            )
            .is_err()
        );
    }

    #[test]
    fn salt_tamper_fails_unseal() {
        // Swapping salts between records must break the password unseal
        // (the KEK changes, so the seal fails to authenticate).
        let (sealed_a, _master_a, password_a, _rec_a) = fixture();
        let (sealed_b, _master_b, _password_b, _rec_b) = fixture();
        let mut tampered = sealed_a.clone();
        tampered.salt_hex = sealed_b.salt_hex;
        assert!(unseal_with_password(&tampered, &password_a).is_err());
    }

    #[test]
    fn unknown_version_rejected() {
        let (mut sealed, _master, _password, _recovery) = fixture();
        sealed.version = 99;
        assert!(matches!(
            unseal_with_password(&sealed, "x"),
            Err(CryptoError::Seal(SealError::UnsupportedVersion(99)))
        ));
        assert!(matches!(
            unseal_with_recovery(
                &sealed,
                "myc2-recovery-0000000000000000000000000000000000000000000000000000000000000000"
            ),
            Err(CryptoError::Seal(SealError::UnsupportedVersion(99)))
        ));
    }

    #[test]
    fn serde_round_trip() {
        let (sealed, _master, _password, _recovery) = fixture();
        let json = serde_json::to_string(&sealed).unwrap();
        let parsed: SealedMasterKey = serde_json::from_str(&json).unwrap();
        assert_eq!(sealed, parsed);
    }
}
