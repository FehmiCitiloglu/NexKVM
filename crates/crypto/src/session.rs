//! Per-session security state.

use std::sync::Mutex;

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use hkdf::Hkdf;
use sha2::Sha256;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::error::CryptoError;

const SESSION_KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;
const NONCE_PREFIX_LEN: usize = NONCE_LEN - std::mem::size_of::<u64>();
const HKDF_SALT: &[u8] = b"nexkvm session security v1";
const AEAD_AAD_PREFIX: &[u8] = b"nexkvm-session-message-v1";

/// Symmetric keys derived for a single authenticated session.
///
/// Held opaquely at the model layer. A real backend derives independent
/// send/receive keys via HKDF over the key-agreement output so the two
/// directions never share a keystream.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct SessionKeys {
    /// Key for data sent by this device.
    pub tx_key: Vec<u8>,
    /// Key for data received from the peer.
    pub rx_key: Vec<u8>,
}

impl SessionKeys {
    /// Derive complementary endpoint keys from authenticated key-agreement output.
    ///
    /// `shared_secret` must be the output of an authenticated key agreement;
    /// `context` should bind the transcript and the ordered peer identities.
    /// The returned pair is ordered: endpoint A's transmit key is endpoint B's
    /// receive key, and vice versa.
    ///
    /// # Errors
    /// Returns [`CryptoError::KeyExchange`] if key material is too short or HKDF
    /// expansion fails.
    pub fn derive_pair(shared_secret: &[u8], context: &[u8]) -> Result<(Self, Self), CryptoError> {
        if shared_secret.len() < SESSION_KEY_LEN {
            return Err(CryptoError::KeyExchange(
                "shared secret must be at least 32 bytes".into(),
            ));
        }

        let hk = Hkdf::<Sha256>::new(Some(HKDF_SALT), shared_secret);
        let a_to_b = expand_key(&hk, b"endpoint-a-to-b", context)?;
        let b_to_a = expand_key(&hk, b"endpoint-b-to-a", context)?;

        Ok((
            Self::new(a_to_b.to_vec(), b_to_a.to_vec())?,
            Self::new(b_to_a.to_vec(), a_to_b.to_vec())?,
        ))
    }

    /// Construct session keys from already-derived send/receive key material.
    ///
    /// # Errors
    /// Returns [`CryptoError::KeyExchange`] unless both keys are exactly 32
    /// bytes, the key size required by ChaCha20-Poly1305.
    pub fn new(tx_key: Vec<u8>, rx_key: Vec<u8>) -> Result<Self, CryptoError> {
        if tx_key.len() != SESSION_KEY_LEN || rx_key.len() != SESSION_KEY_LEN {
            return Err(CryptoError::KeyExchange(
                "session keys must be 32 bytes each".into(),
            ));
        }
        Ok(Self { tx_key, rx_key })
    }
}

// Keys must never be printed.
impl std::fmt::Debug for SessionKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionKeys").finish_non_exhaustive()
    }
}

/// Authenticated-encryption operations for an established session.
///
/// Implementors enforce **replay protection** by tracking accepted message ids
/// and rejecting duplicates/out-of-window values via [`CryptoError::Replay`].
pub trait SessionSecurity: Send + Sync {
    /// Seal `plaintext` for message `id`, returning ciphertext + auth tag.
    ///
    /// # Errors
    /// Returns a [`CryptoError`] if encryption fails.
    fn seal(&self, id: u64, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError>;

    /// Open `ciphertext` received as message `id`.
    ///
    /// # Errors
    /// Returns [`CryptoError::Replay`] for replayed ids, [`CryptoError::BadSignature`]
    /// on authentication failure, or another [`CryptoError`] on decode failure.
    fn open(&self, id: u64, ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError>;
}

/// Production authenticated-encryption session.
///
/// The session uses independent ChaCha20-Poly1305 keys for each direction,
/// message-id-derived nonces, associated data binding, and a sliding replay
/// window for received message ids. It performs no identity or trust decisions;
/// callers must derive [`SessionKeys`] only after an authenticated handshake.
pub struct AeadSessionSecurity {
    tx: ChaCha20Poly1305,
    rx: ChaCha20Poly1305,
    replay: Mutex<ReplayWindow>,
}

impl AeadSessionSecurity {
    /// Build a session from derived directional keys.
    ///
    /// # Errors
    /// Returns [`CryptoError::KeyExchange`] if either key is not 32 bytes.
    pub fn new(keys: SessionKeys) -> Result<Self, CryptoError> {
        let tx = cipher_from_key(&keys.tx_key)?;
        let rx = cipher_from_key(&keys.rx_key)?;
        Ok(Self {
            tx,
            rx,
            replay: Mutex::new(ReplayWindow::default()),
        })
    }
}

impl std::fmt::Debug for AeadSessionSecurity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AeadSessionSecurity")
            .finish_non_exhaustive()
    }
}

impl SessionSecurity for AeadSessionSecurity {
    fn seal(&self, id: u64, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        self.tx
            .encrypt(
                Nonce::from_slice(&nonce_for(id)),
                Payload {
                    msg: plaintext,
                    aad: &aad_for(id),
                },
            )
            .map_err(|_| CryptoError::KeyExchange("AEAD seal failed".into()))
    }

    fn open(&self, id: u64, ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let plaintext = self
            .rx
            .decrypt(
                Nonce::from_slice(&nonce_for(id)),
                Payload {
                    msg: ciphertext,
                    aad: &aad_for(id),
                },
            )
            .map_err(|_| CryptoError::BadSignature)?;

        self.replay
            .lock()
            .map_err(|_| CryptoError::KeyExchange("replay window lock poisoned".into()))?
            .check_and_mark(id)?;

        Ok(plaintext)
    }
}

#[derive(Debug, Default)]
struct ReplayWindow {
    highest: Option<u64>,
    seen: u128,
}

impl ReplayWindow {
    fn check_and_mark(&mut self, id: u64) -> Result<(), CryptoError> {
        match self.highest {
            None => {
                self.highest = Some(id);
                self.seen = 1;
                Ok(())
            }
            Some(highest) if id > highest => {
                let shift = id - highest;
                self.seen = if shift >= u128::BITS as u64 {
                    1
                } else {
                    (self.seen << shift) | 1
                };
                self.highest = Some(id);
                Ok(())
            }
            Some(highest) => {
                let offset = highest - id;
                if offset >= u128::BITS as u64 {
                    return Err(CryptoError::Replay(id));
                }
                let bit = 1u128 << offset;
                if self.seen & bit != 0 {
                    return Err(CryptoError::Replay(id));
                }
                self.seen |= bit;
                Ok(())
            }
        }
    }
}

fn expand_key(
    hk: &Hkdf<Sha256>,
    label: &[u8],
    context: &[u8],
) -> Result<[u8; SESSION_KEY_LEN], CryptoError> {
    let mut info = Vec::with_capacity(label.len() + 1 + context.len());
    info.extend_from_slice(label);
    info.push(0);
    info.extend_from_slice(context);

    let mut key = [0u8; SESSION_KEY_LEN];
    hk.expand(&info, &mut key)
        .map_err(|_| CryptoError::KeyExchange("HKDF expansion failed".into()))?;
    Ok(key)
}

fn cipher_from_key(key: &[u8]) -> Result<ChaCha20Poly1305, CryptoError> {
    if key.len() != SESSION_KEY_LEN {
        return Err(CryptoError::KeyExchange(
            "session key must be 32 bytes".into(),
        ));
    }
    ChaCha20Poly1305::new_from_slice(key)
        .map_err(|_| CryptoError::KeyExchange("invalid AEAD key".into()))
}

fn nonce_for(id: u64) -> [u8; NONCE_LEN] {
    let mut nonce = [0u8; NONCE_LEN];
    nonce[NONCE_PREFIX_LEN..].copy_from_slice(&id.to_be_bytes());
    nonce
}

fn aad_for(id: u64) -> Vec<u8> {
    let mut aad = Vec::with_capacity(AEAD_AAD_PREFIX.len() + std::mem::size_of::<u64>());
    aad.extend_from_slice(AEAD_AAD_PREFIX);
    aad.extend_from_slice(&id.to_be_bytes());
    aad
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sessions() -> (AeadSessionSecurity, AeadSessionSecurity) {
        let (a, b) = SessionKeys::derive_pair(
            b"shared secret from authenticated key agreement",
            b"pairing transcript and peer identity binding",
        )
        .expect("keys");
        (
            AeadSessionSecurity::new(a).expect("session a"),
            AeadSessionSecurity::new(b).expect("session b"),
        )
    }

    #[test]
    fn aead_session_round_trips_between_peers() {
        let (a, b) = sessions();

        let sealed = a.seal(7, b"hello peer").expect("seal");

        assert_ne!(sealed, b"hello peer");
        assert_eq!(b.open(7, &sealed).expect("open"), b"hello peer");
    }

    #[test]
    fn aead_session_rejects_tampered_ciphertext() {
        let (a, b) = sessions();
        let mut sealed = a.seal(8, b"authenticated").expect("seal");
        let last = sealed.last_mut().expect("tag byte");
        *last ^= 0x80;

        assert!(matches!(b.open(8, &sealed), Err(CryptoError::BadSignature)));
    }

    #[test]
    fn aead_session_rejects_replayed_message_ids() {
        let (a, b) = sessions();
        let sealed = a.seal(9, b"only once").expect("seal");

        assert_eq!(b.open(9, &sealed).expect("first open"), b"only once");
        assert!(matches!(b.open(9, &sealed), Err(CryptoError::Replay(9))));
    }

    #[test]
    fn derived_pair_separates_directions() {
        let (a, b) = SessionKeys::derive_pair(b"0123456789abcdef0123456789abcdef", b"context")
            .expect("keys");

        assert_eq!(a.tx_key, b.rx_key);
        assert_eq!(a.rx_key, b.tx_key);
        assert_ne!(a.tx_key, a.rx_key);
    }

    #[test]
    fn debug_output_does_not_leak_key_material() {
        let (keys, _) = SessionKeys::derive_pair(b"debug-secret-32-byte-test-input!", b"context")
            .expect("keys");
        let session = AeadSessionSecurity::new(keys.clone()).expect("session");

        let key_debug = format!("{keys:?}");
        let session_debug = format!("{session:?}");
        let tx_hex = keys
            .tx_key
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let rx_hex = keys
            .rx_key
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();

        assert!(!key_debug.contains(&tx_hex));
        assert!(!key_debug.contains(&rx_hex));
        assert!(!session_debug.contains(&tx_hex));
        assert!(!session_debug.contains(&rx_hex));
    }
}
