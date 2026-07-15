//! Transfer encryption boundary.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use nexkvm_crypto::SessionSecurity;

use crate::TransferError;

/// Authenticated encryption boundary for transfer chunks.
pub trait TransferCipher: std::fmt::Debug + Send + Sync {
    /// Seal plaintext.
    ///
    /// # Errors
    /// Returns [`TransferError::Encryption`] on backend failures.
    fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, TransferError>;

    /// Open ciphertext.
    ///
    /// # Errors
    /// Returns [`TransferError::Encryption`] on authentication/decode failure.
    fn open(&self, ciphertext: &[u8]) -> Result<Vec<u8>, TransferError>;
}

/// No-op cipher for testing and local integration only.
#[derive(Debug, Clone, Copy, Default)]
pub struct PlaintextTransferCipher;

impl TransferCipher for PlaintextTransferCipher {
    fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, TransferError> {
        Ok(plaintext.to_vec())
    }

    fn open(&self, ciphertext: &[u8]) -> Result<Vec<u8>, TransferError> {
        Ok(ciphertext.to_vec())
    }
}

/// Length of the message-id frame prepended to each sealed chunk payload.
const ID_FRAME_LEN: usize = 8;

/// Production [`TransferCipher`] backed by an authenticated
/// [`SessionSecurity`] session.
///
/// `SessionSecurity` seals/opens against a caller-supplied message `id` (used
/// for nonce derivation and replay rejection), but the [`TransferCipher`]
/// boundary is id-less. This adapter allocates a strictly increasing id per
/// chunk from an internal counter and frames it as an 8-byte big-endian prefix
/// on the ciphertext, so `open` recovers the id and lets the session enforce
/// replay protection. Each chunk therefore seals to a distinct ciphertext even
/// when plaintext repeats, and a re-sent chunk is rejected as a replay.
///
/// The session is shared (`Arc`) because it secures other message lanes too;
/// the transfer cipher only borrows it.
#[derive(Clone)]
pub struct SessionTransferCipher {
    session: Arc<dyn SessionSecurity>,
    next_id: Arc<AtomicU64>,
}

impl SessionTransferCipher {
    /// Wrap `session`, starting chunk message ids at `base_id`.
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

impl std::fmt::Debug for SessionTransferCipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never expose key material or the live counter value.
        f.debug_struct("SessionTransferCipher")
            .finish_non_exhaustive()
    }
}

impl TransferCipher for SessionTransferCipher {
    fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, TransferError> {
        let id = self
            .next_id
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_| TransferError::Encryption("message id space exhausted".into()))?;
        let ciphertext = self
            .session
            .seal(id, plaintext)
            .map_err(|e| TransferError::Encryption(e.to_string()))?;
        let mut framed = Vec::with_capacity(ID_FRAME_LEN + ciphertext.len());
        framed.extend_from_slice(&id.to_be_bytes());
        framed.extend_from_slice(&ciphertext);
        Ok(framed)
    }

    fn open(&self, ciphertext: &[u8]) -> Result<Vec<u8>, TransferError> {
        if ciphertext.len() < ID_FRAME_LEN {
            return Err(TransferError::Encryption("missing id frame".into()));
        }
        let (id_bytes, body) = ciphertext.split_at(ID_FRAME_LEN);
        let mut id_frame = [0u8; ID_FRAME_LEN];
        id_frame.copy_from_slice(id_bytes);
        let id = u64::from_be_bytes(id_frame);
        self.session
            .open(id, body)
            .map_err(|e| TransferError::Encryption(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexkvm_crypto::CryptoError;
    use std::collections::HashSet;
    use std::sync::Mutex;

    #[test]
    fn plaintext_cipher_round_trips() {
        let c = PlaintextTransferCipher;
        let sealed = c.seal(b"data").unwrap();
        assert_eq!(c.open(&sealed).unwrap(), b"data");
    }

    /// Mock session: XORs with a per-id keystream byte and rejects replayed ids.
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
        let cipher = SessionTransferCipher::new(Arc::new(MockSession::default()), 0);
        let sealed = cipher.seal(b"secret chunk").unwrap();
        assert_ne!(&sealed[ID_FRAME_LEN..], b"secret chunk");
        assert_eq!(cipher.open(&sealed).unwrap(), b"secret chunk");
    }

    #[test]
    fn identical_chunks_seal_differently() {
        let cipher = SessionTransferCipher::new(Arc::new(MockSession::default()), 0);
        let a = cipher.seal(b"chunk").unwrap();
        let b = cipher.seal(b"chunk").unwrap();
        assert_ne!(a, b, "monotonic id must diversify ciphertext");
    }

    #[test]
    fn replayed_chunk_is_rejected() {
        let cipher = SessionTransferCipher::new(Arc::new(MockSession::default()), 0);
        let sealed = cipher.seal(b"once").unwrap();
        assert!(cipher.open(&sealed).is_ok());
        assert!(matches!(
            cipher.open(&sealed),
            Err(TransferError::Encryption(_))
        ));
    }

    #[test]
    fn truncated_frame_is_rejected() {
        let cipher = SessionTransferCipher::new(Arc::new(MockSession::default()), 0);
        assert!(matches!(
            cipher.open(&[0u8; 4]),
            Err(TransferError::Encryption(_))
        ));
    }

    #[test]
    fn message_id_exhaustion_never_wraps_or_reuses_a_nonce() {
        let cipher = SessionTransferCipher::new(Arc::new(MockSession::default()), u64::MAX - 1);

        assert!(cipher.seal(b"last safe id").is_ok());
        assert!(matches!(
            cipher.seal(b"must not wrap"),
            Err(TransferError::Encryption(_))
        ));
        assert!(matches!(
            cipher.seal(b"must stay exhausted"),
            Err(TransferError::Encryption(_))
        ));
    }
}
