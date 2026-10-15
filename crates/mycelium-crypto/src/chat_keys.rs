//! Chat-scoped key derivation and sealing.
//!
//! Chat transcripts are content: SQLite keeps only metadata (ids, roles,
//! sequence numbers, timestamps) while titles and message bodies travel as
//! AEAD envelopes under a DEK derived from the user's master key. The DEK is
//! purpose-bound by HKDF domain separation (`mycelium2/chat-dek/v1`), so chat
//! ciphertext is unreadable with the file, index, or metadata keys and vice
//! versa — compromising one store's key does not decrypt another's content.

use crate::CryptoError;
use crate::aead::{aead_open, aead_seal};
use crate::keys::{Dek, MasterKey};
use crate::store_keys::derive_dek_from;

/// HKDF info string for the chat DEK (domain separator from every other
/// purpose-bound derivation in the crate).
const CHAT_DEK_INFO: &[u8] = b"mycelium2/chat-dek/v1";

/// Keys for chat session transcripts.
///
/// - `chat_dek`: seals session titles and message content. Derived from the
///   user's master key, so a chat row is readable only by the user who owns
///   the session (and never by the service key).
#[derive(Clone)]
pub struct ChatKeys {
    chat_dek: Dek,
}

impl ChatKeys {
    /// Derive chat keys from a user's master key.
    ///
    /// Deterministic: the same master key always yields the same chat DEK,
    /// which is what makes sealed transcripts readable across restarts.
    pub fn from_master_key(master: &MasterKey) -> Self {
        Self {
            chat_dek: derive_dek_from(master, CHAT_DEK_INFO),
        }
    }

    /// The chat content DEK.
    pub fn chat_dek(&self) -> &Dek {
        &self.chat_dek
    }
}

impl std::fmt::Debug for ChatKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ChatKeys(<redacted>)")
    }
}

/// Seal chat content (a session title or a message body) under `dek`.
///
/// `aad` is the binding context — the row's own identity (`chat:<session>:<seq>`
/// for messages, `chat:<session>:title` for a session title) — so a sealed
/// blob cannot be replayed into another row and still open. Fresh nonce per
/// call.
pub fn seal_chat(plain: &[u8], aad: &str, dek: &Dek) -> Result<Vec<u8>, CryptoError> {
    aead_seal(plain, aad.as_bytes(), dek)
}

/// Open a blob sealed by [`seal_chat`].
///
/// Fails on tampering, a wrong key, or an `aad` other than the one it was
/// sealed with.
pub fn open_chat(sealed: &[u8], aad: &str, dek: &Dek) -> Result<Vec<u8>, CryptoError> {
    aead_open(sealed, aad.as_bytes(), dek)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::MasterKey;
    use crate::store_keys::{FileKeys, IndexKeys};

    fn master(seed: u8) -> MasterKey {
        MasterKey::from_bytes(&[seed; 32]).unwrap()
    }

    #[test]
    fn chat_dek_is_distinct_from_master_and_deterministic() {
        let master = MasterKey::from_bytes(&[7u8; 32]).unwrap();
        let a = ChatKeys::from_master_key(&master);
        let b = ChatKeys::from_master_key(&master);
        assert_eq!(a.chat_dek().as_bytes(), b.chat_dek().as_bytes());
        let other = MasterKey::from_bytes(&[8u8; 32]).unwrap();
        let c = ChatKeys::from_master_key(&other);
        assert_ne!(a.chat_dek().as_bytes(), c.chat_dek().as_bytes());
    }

    #[test]
    fn seal_open_roundtrip_and_aad_binding() {
        let keys = ChatKeys::from_master_key(&master(7));
        let sealed = seal_chat(b"hello", "msg:abc", keys.chat_dek()).unwrap();
        assert_eq!(
            open_chat(&sealed, "msg:abc", keys.chat_dek()).unwrap(),
            b"hello"
        );
        // Wrong AAD: undecryptable.
        assert!(open_chat(&sealed, "msg:other", keys.chat_dek()).is_err());
        // Wrong key: undecryptable.
        let wrong = ChatKeys::from_master_key(&master(9));
        assert!(open_chat(&sealed, "msg:abc", wrong.chat_dek()).is_err());
    }

    #[test]
    fn chat_dek_is_domain_separated_from_other_purposes() {
        // Purpose binding: the same master key must not produce a chat DEK
        // equal to the master key itself or to any other store's DEK.
        let master = master(7);
        let chat = ChatKeys::from_master_key(&master);
        assert_ne!(chat.chat_dek().as_bytes(), master.as_bytes());
        assert_ne!(
            chat.chat_dek().as_bytes(),
            FileKeys::from_master_key(&master).meta_dek().as_bytes()
        );
        assert_ne!(
            chat.chat_dek().as_bytes(),
            IndexKeys::from_master_key(&master).index_dek().as_bytes()
        );
        // And chat ciphertext does not open with the index DEK.
        let sealed = seal_chat(b"transcript", "title:s", chat.chat_dek()).unwrap();
        let index_dek = IndexKeys::from_master_key(&master).index_dek().clone();
        assert!(open_chat(&sealed, "title:s", &index_dek).is_err());
    }

    #[test]
    fn empty_content_round_trips() {
        let keys = ChatKeys::from_master_key(&master(3));
        let sealed = seal_chat(b"", "", keys.chat_dek()).unwrap();
        assert!(open_chat(&sealed, "", keys.chat_dek()).unwrap().is_empty());
    }

    #[test]
    fn fresh_nonce_per_call() {
        let keys = ChatKeys::from_master_key(&master(3));
        let a = seal_chat(b"same", "msg:x", keys.chat_dek()).unwrap();
        let b = seal_chat(b"same", "msg:x", keys.chat_dek()).unwrap();
        assert_ne!(a, b);
        assert_eq!(open_chat(&a, "msg:x", keys.chat_dek()).unwrap(), b"same");
    }

    #[test]
    fn tampered_seal_fails() {
        let keys = ChatKeys::from_master_key(&master(3));
        let mut sealed = seal_chat(b"secret", "msg:x", keys.chat_dek()).unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 0xFF;
        assert!(open_chat(&sealed, "msg:x", keys.chat_dek()).is_err());
    }

    #[test]
    fn debug_is_redacted() {
        let keys = ChatKeys::from_master_key(&master(7));
        let rendered = format!("{keys:?}");
        assert!(rendered.contains("<redacted>"));
        assert!(!rendered.contains("7, 7, 7"));
    }
}
