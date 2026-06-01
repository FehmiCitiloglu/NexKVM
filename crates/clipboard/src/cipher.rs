//! Clipboard encryption boundary.
//!
//! Clipboard payloads are sealed by an authenticated cipher *before* they reach
//! the transport, so the plaintext is never exposed to relays, the future
//! WebRTC path, or at-rest persistence. This crate deliberately does **not**
//! implement any cryptographic primitive: it defines the [`ClipboardCipher`]
//! boundary and the production implementation is an adapter over a
//! [`coklu_crypto::SessionSecurity`] session, injected by the orchestration
//! layer. Keeping crypto in one audited crate is a security requirement.
//!
//! [`coklu_crypto::SessionSecurity`]: https://docs.rs/coklu-crypto

use std::fmt::Debug;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use coklu_crypto::SessionSecurity;

use crate::ClipboardError;

/// Authenticated sealing/opening of opaque clipboard payload bytes.
///
/// Implementations MUST provide confidentiality and integrity (AEAD) and SHOULD
/// bind a nonce/sequence so identical plaintexts seal to distinct ciphertexts
/// and replays are detectable upstream.
pub trait ClipboardCipher: Debug + Send + Sync {
    /// Seal `plaintext`, returning ciphertext + authentication tag.
    ///
    /// # Errors
    /// Returns [`ClipboardError::Encryption`] on backend failure.
    fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, ClipboardError>;

    /// Open `ciphertext`, verifying its authentication tag.
    ///
    /// # Errors
    /// Returns [`ClipboardError::Encryption`] on authentication or decode failure.
    fn open(&self, ciphertext: &[u8]) -> Result<Vec<u8>, ClipboardError>;
}

/// A no-op cipher that passes bytes through unchanged.
///
/// **Insecure — for local development and tests only.** It exists so the sync
/// pipeline is exercisable without a live crypto session; production code must
/// inject a real [`ClipboardCipher`] backed by an authenticated session.
#[derive(Debug, Clone, Copy, Default)]
pub struct PlaintextCipher;

impl ClipboardCipher for PlaintextCipher {
    fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, ClipboardError> {
        Ok(plaintext.to_vec())
    }

    fn open(&self, ciphertext: &[u8]) -> Result<Vec<u8>, ClipboardError> {
        Ok(ciphertext.to_vec())
    }
}

/// Length of the message-id frame prepended to each sealed payload.
const ID_FRAME_LEN: usize = 8;

/// Production [`ClipboardCipher`] backed by an authenticated
/// [`SessionSecurity`] session.
///
/// The `SessionSecurity` API seals/opens per a caller-supplied message `id`
/// (which it uses for nonce derivation and replay rejection), but the
/// [`ClipboardCipher`] boundary is id-less. This adapter bridges the two by
/// allocating a strictly increasing id from an internal counter on seal and
/// framing it as an 8-byte big-endian prefix on the ciphertext, so `open` can
/// recover the id and let the session enforce replay protection. Identical
/// clipboard payloads therefore seal to distinct ciphertexts.
///
/// The session is shared (`Arc`) because the same authenticated session secures
/// other message lanes; the clipboard cipher only borrows it.
#[derive(Clone)]
pub struct SessionClipboardCipher {
    session: Arc<dyn SessionSecurity>,
    next_id: Arc<AtomicU64>,
}

impl SessionClipboardCipher {
    /// Wrap `session`, starting message ids at `base_id`.
    ///
    /// `base_id` should be distinct from the id space used by other lanes on
    /// the same session (or the session must namespace lanes internally) so
    /// ids never collide and trip replay rejection.
    #[must_use]
    pub fn new(session: Arc<dyn SessionSecurity>, base_id: u64) -> Self {
        Self {
            session,
            next_id: Arc::new(AtomicU64::new(base_id)),
        }
    }
}

impl Debug for SessionClipboardCipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never expose key material or the live counter value.
        f.debug_struct("SessionClipboardCipher")
            .finish_non_exhaustive()
    }
}

impl ClipboardCipher for SessionClipboardCipher {
    fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, ClipboardError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let ciphertext = self
            .session
            .seal(id, plaintext)
            .map_err(|e| ClipboardError::Encryption(e.to_string()))?;
        let mut framed = Vec::with_capacity(ID_FRAME_LEN + ciphertext.len());
        framed.extend_from_slice(&id.to_be_bytes());
        framed.extend_from_slice(&ciphertext);
        Ok(framed)
    }

    fn open(&self, ciphertext: &[u8]) -> Result<Vec<u8>, ClipboardError> {
        if ciphertext.len() < ID_FRAME_LEN {
            return Err(ClipboardError::Encryption("missing id frame".into()));
        }
        let (id_bytes, body) = ciphertext.split_at(ID_FRAME_LEN);
        let id = u64::from_be_bytes(id_bytes.try_into().expect("ID_FRAME_LEN bytes"));
        self.session
            .open(id, body)
            .map_err(|e| ClipboardError::Encryption(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use coklu_crypto::CryptoError;
    use std::collections::HashSet;
    use std::sync::Mutex;

    #[test]
    fn plaintext_cipher_round_trips() {
        let c = PlaintextCipher;
        let sealed = c.seal(b"data").unwrap();
        assert_eq!(c.open(&sealed).unwrap(), b"data");
    }

    /// Mock session: XORs with a per-id keystream byte and rejects replayed ids,
    /// mirroring the real `SessionSecurity` contract closely enough to test the
    /// adapter's framing and replay surfacing.
    #[derive(Debug, Default)]
    struct MockSession {
        seen: Mutex<HashSet<u64>>,
    }

    fn transform(id: u64, data: &[u8]) -> Vec<u8> {
        let k = (id & 0xff) as u8 ^ 0x5a;
        data.iter().map(|b| b ^ k).collect()
    }

    impl SessionSecurity for MockSession {
        fn seal(&self, id: u64, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
            Ok(transform(id, plaintext))
        }

        fn open(&self, id: u64, ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError> {
            if !self.seen.lock().expect("poisoned").insert(id) {
                return Err(CryptoError::Replay(id));
            }
            Ok(transform(id, ciphertext))
        }
    }

    #[test]
    fn session_cipher_round_trips() {
        let session = Arc::new(MockSession::default());
        let cipher = SessionClipboardCipher::new(session, 0);
        let sealed = cipher.seal(b"secret clip").unwrap();
        assert_ne!(&sealed[8..], b"secret clip"); // genuinely transformed
        assert_eq!(cipher.open(&sealed).unwrap(), b"secret clip");
    }

    #[test]
    fn identical_plaintext_seals_differently() {
        let session = Arc::new(MockSession::default());
        let cipher = SessionClipboardCipher::new(session, 0);
        let a = cipher.seal(b"same").unwrap();
        let b = cipher.seal(b"same").unwrap();
        assert_ne!(a, b, "monotonic id must diversify ciphertext");
    }

    #[test]
    fn replayed_payload_is_rejected() {
        let session = Arc::new(MockSession::default());
        let cipher = SessionClipboardCipher::new(session, 0);
        let sealed = cipher.seal(b"once").unwrap();
        assert!(cipher.open(&sealed).is_ok());
        // Re-opening the same framed payload reuses its id → replay rejected.
        assert!(matches!(
            cipher.open(&sealed),
            Err(ClipboardError::Encryption(_))
        ));
    }

    #[test]
    fn truncated_frame_is_rejected() {
        let session = Arc::new(MockSession::default());
        let cipher = SessionClipboardCipher::new(session, 0);
        assert!(matches!(
            cipher.open(&[0u8; 4]),
            Err(ClipboardError::Encryption(_))
        ));
    }
}
