//! AEAD seal/open via ring's AES-256-GCM.

use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};

use crate::CryptoError;
use crate::envelope::{Envelope, NONCE_LEN};
use crate::keys::Dek;

/// Seal `plaintext` with a per-file DEK and optional associated data.
///
/// Returns a versioned envelope: magic + version + random nonce +
/// ciphertext-with-tag. Fresh random nonce per call (envelope ciphertexts
/// differ for identical plaintexts — by design).
pub fn aead_seal(plaintext: &[u8], aad: &[u8], key: &Dek) -> Result<Vec<u8>, CryptoError> {
    let unbound = UnboundKey::new(&AES_256_GCM, key.as_bytes())
        .map_err(|e| CryptoError::Encrypt(format!("bad key: {e}")))?;
    let sealing_key = LessSafeKey::new(unbound);

    let mut nonce_bytes = [0u8; NONCE_LEN];
    let rng = SystemRandom::new();
    rng.fill(&mut nonce_bytes)
        .map_err(|e| CryptoError::Encrypt(format!("rng failure: {e}")))?;
    let nonce = Nonce::assume_unique_for_key(nonce_bytes);

    let mut in_out = plaintext.to_vec();
    sealing_key
        .seal_in_place_append_tag(nonce, Aad::from(aad), &mut in_out)
        .map_err(|e| CryptoError::Encrypt(format!("seal failure: {e}")))?;

    let envelope = Envelope {
        version: crate::ENVELOPE_VERSION,
        nonce: nonce_bytes,
        ciphertext: in_out,
    };
    Ok(envelope.to_bytes())
}

/// Open a sealed envelope produced by [`aead_seal`].
///
/// Fails on: bad magic/version, tampered ciphertext, wrong key, or AAD
/// mismatch (ring's GCM tag covers the AAD).
pub fn aead_open(envelope_bytes: &[u8], aad: &[u8], key: &Dek) -> Result<Vec<u8>, CryptoError> {
    let envelope = Envelope::from_bytes(envelope_bytes)?;
    let unbound = UnboundKey::new(&AES_256_GCM, key.as_bytes())
        .map_err(|e| CryptoError::Decrypt(format!("bad key: {e}")))?;
    let opening_key = LessSafeKey::new(unbound);

    let nonce = Nonce::assume_unique_for_key(envelope.nonce);
    let mut in_out = envelope.ciphertext;
    let plaintext = opening_key
        .open_in_place(nonce, Aad::from(aad), &mut in_out)
        .map_err(|_| CryptoError::Decrypt("authentication failed".into()))?;
    Ok(plaintext.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::{derive_dek, generate_master_key};

    fn dek() -> Dek {
        derive_dek(&generate_master_key(), "/test.md").unwrap()
    }

    #[test]
    fn round_trip() {
        let key = dek();
        let sealed = aead_seal(b"hello mycelium", b"aad", &key).unwrap();
        let opened = aead_open(&sealed, b"aad", &key).unwrap();
        assert_eq!(opened, b"hello mycelium");
    }

    #[test]
    fn empty_plaintext_round_trip() {
        let key = dek();
        let sealed = aead_seal(b"", b"", &key).unwrap();
        let opened = aead_open(&sealed, b"", &key).unwrap();
        assert!(opened.is_empty());
    }

    #[test]
    fn fresh_nonce_per_call() {
        let key = dek();
        let a = aead_seal(b"same", b"", &key).unwrap();
        let b = aead_seal(b"same", b"", &key).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn wrong_key_fails() {
        let key = dek();
        let other = dek();
        let sealed = aead_seal(b"secret", b"", &key).unwrap();
        assert!(aead_open(&sealed, b"", &other).is_err());
    }

    #[test]
    fn wrong_aad_fails() {
        let key = dek();
        let sealed = aead_seal(b"secret", b"expected-aad", &key).unwrap();
        assert!(aead_open(&sealed, b"other-aad", &key).is_err());
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let key = dek();
        let mut sealed = aead_seal(b"secret", b"", &key).unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 0xFF;
        assert!(aead_open(&sealed, b"", &key).is_err());
    }

    #[test]
    fn tampered_nonce_fails() {
        let key = dek();
        let mut sealed = aead_seal(b"secret", b"", &key).unwrap();
        // Nonce starts after magic(4) + version(1).
        sealed[5] ^= 0xFF;
        assert!(aead_open(&sealed, b"", &key).is_err());
    }

    #[test]
    fn truncated_input_fails() {
        let key = dek();
        let sealed = aead_seal(b"secret", b"", &key).unwrap();
        assert!(aead_open(&sealed[..sealed.len() - 5], b"", &key).is_err());
    }
}
