//! Versioned binary envelope format for encrypted payloads.
//!
//! Layout (little-endian where applicable):
//! ```text
//! +--------+---------+-------+---------------------+
//! | magic  | version | nonce | ciphertext (+ tag)  |
//! | 4 B    | 1 B     | 12 B  | rest                |
//! +--------+---------+-------+---------------------+
//! ```
//! The nonce is random per encryption; the AEAD tag is appended to the
//! ciphertext by ring. AAD is NOT stored (callers bind what they need).

use crate::CryptoError;

pub const ENVELOPE_MAGIC: [u8; 4] = *b"MYC2";
pub const ENVELOPE_VERSION: u8 = 1;
pub const NONCE_LEN: usize = 12;
/// AES-256-GCM tag length (ring appends it to the ciphertext).
pub const TAG_LEN: usize = 16;

const HEADER_LEN: usize = ENVELOPE_MAGIC.len() + 1 + NONCE_LEN;

/// A parsed envelope: nonce + ciphertext (tag included).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    pub version: u8,
    pub nonce: [u8; NONCE_LEN],
    pub ciphertext: Vec<u8>,
}

impl Envelope {
    /// Serialize to bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + self.ciphertext.len());
        out.extend_from_slice(&ENVELOPE_MAGIC);
        out.push(self.version);
        out.extend_from_slice(&self.nonce);
        out.extend_from_slice(&self.ciphertext);
        out
    }

    /// Parse from bytes, validating magic, version, and minimum lengths.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, CryptoError> {
        if bytes.len() < HEADER_LEN + TAG_LEN {
            return Err(CryptoError::Envelope(crate::EnvelopeError::TooShort {
                len: bytes.len(),
            }));
        }
        if bytes[..ENVELOPE_MAGIC.len()] != ENVELOPE_MAGIC {
            return Err(CryptoError::Envelope(crate::EnvelopeError::BadMagic));
        }
        let version = bytes[ENVELOPE_MAGIC.len()];
        if version != ENVELOPE_VERSION {
            return Err(CryptoError::Envelope(
                crate::EnvelopeError::UnsupportedVersion(version),
            ));
        }
        let mut nonce = [0u8; NONCE_LEN];
        nonce.copy_from_slice(
            &bytes[ENVELOPE_MAGIC.len() + 1..ENVELOPE_MAGIC.len() + 1 + NONCE_LEN],
        );
        let ciphertext = bytes[HEADER_LEN..].to_vec();
        Ok(Self {
            version,
            nonce,
            ciphertext,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Envelope {
        Envelope {
            version: ENVELOPE_VERSION,
            nonce: [7u8; NONCE_LEN],
            ciphertext: vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 99],
        }
    }

    #[test]
    fn round_trip() {
        let e = sample();
        let bytes = e.to_bytes();
        let parsed = Envelope::from_bytes(&bytes).unwrap();
        assert_eq!(e, parsed);
    }

    #[test]
    fn rejects_too_short() {
        let err = Envelope::from_bytes(&[0u8; 10]).unwrap_err();
        assert!(matches!(
            err,
            CryptoError::Envelope(crate::EnvelopeError::TooShort { .. })
        ));
    }

    #[test]
    fn rejects_bad_magic() {
        let mut bytes = sample().to_bytes();
        bytes[0] = b'X';
        assert!(matches!(
            Envelope::from_bytes(&bytes).unwrap_err(),
            CryptoError::Envelope(crate::EnvelopeError::BadMagic)
        ));
    }

    #[test]
    fn rejects_future_version() {
        let mut bytes = sample().to_bytes();
        bytes[4] = 9;
        assert!(matches!(
            Envelope::from_bytes(&bytes).unwrap_err(),
            CryptoError::Envelope(crate::EnvelopeError::UnsupportedVersion(9))
        ));
    }
}
