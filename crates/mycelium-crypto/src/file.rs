//! High-level file encryption: master key + path-bound DEK + AEAD envelope.

use crate::CryptoError;
use crate::aead::{aead_open, aead_seal};
use crate::keys::{MasterKey, derive_dek};

/// Encrypt `plaintext` as the file at canonical bundle path `path`,
/// under the user's master key.
///
/// The DEK is derived from (master key, path), and the path is ALSO bound as
/// AEAD associated data — so a ciphertext cannot be moved or renamed without
/// failing to decrypt. The path must be canonical (non-empty, leading `/`).
pub fn encrypt_file(
    plaintext: &[u8],
    path: &str,
    master_key: &MasterKey,
) -> Result<Vec<u8>, CryptoError> {
    let dek = derive_dek(master_key, path)?;
    aead_seal(plaintext, path.as_bytes(), &dek)
}

/// Decrypt a file previously encrypted by [`encrypt_file`].
///
/// Fails on: wrong master key, wrong path, tampered ciphertext, or a
/// malformed envelope.
pub fn decrypt_file(
    ciphertext: &[u8],
    path: &str,
    master_key: &MasterKey,
) -> Result<Vec<u8>, CryptoError> {
    let dek = derive_dek(master_key, path)?;
    aead_open(ciphertext, path.as_bytes(), &dek)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::generate_master_key;

    #[test]
    fn round_trip() {
        let master = generate_master_key();
        let sealed = encrypt_file(b"# Note\nbody", "/notes/todo.md", &master).unwrap();
        let opened = decrypt_file(&sealed, "/notes/todo.md", &master).unwrap();
        assert_eq!(opened, b"# Note\nbody");
    }

    #[test]
    fn wrong_path_fails() {
        let master = generate_master_key();
        let sealed = encrypt_file(b"secret", "/notes/todo.md", &master).unwrap();
        assert!(decrypt_file(&sealed, "/notes/other.md", &master).is_err());
    }

    #[test]
    fn wrong_master_key_fails() {
        let master = generate_master_key();
        let other = generate_master_key();
        let sealed = encrypt_file(b"secret", "/notes/todo.md", &master).unwrap();
        assert!(decrypt_file(&sealed, "/notes/todo.md", &other).is_err());
    }

    #[test]
    fn non_canonical_path_rejected() {
        let master = generate_master_key();
        assert!(encrypt_file(b"x", "", &master).is_err());
        assert!(encrypt_file(b"x", "relative.md", &master).is_err());
    }

    #[test]
    fn unicode_path_round_trip() {
        let master = generate_master_key();
        let sealed = encrypt_file(b"unicode", "/notes/日本語.md", &master).unwrap();
        let opened = decrypt_file(&sealed, "/notes/日本語.md", &master).unwrap();
        assert_eq!(opened, b"unicode");
    }

    #[test]
    fn large_payload_round_trip() {
        let master = generate_master_key();
        let big = vec![0xABu8; 1024 * 1024]; // 1 MiB
        let sealed = encrypt_file(&big, "/books/big.md", &master).unwrap();
        let opened = decrypt_file(&sealed, "/books/big.md", &master).unwrap();
        assert_eq!(opened.len(), big.len());
        assert_eq!(opened, big);
    }
}
