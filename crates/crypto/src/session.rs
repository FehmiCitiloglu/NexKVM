//! Per-session security state.

use crate::error::CryptoError;

/// Symmetric keys derived for a single authenticated session.
///
/// Held opaquely at the model layer. A real backend derives independent
/// send/receive keys via HKDF over the key-agreement output so the two
/// directions never share a keystream.
#[derive(Clone)]
pub struct SessionKeys {
    /// Key for data sent by this device.
    pub tx_key: Vec<u8>,
    /// Key for data received from the peer.
    pub rx_key: Vec<u8>,
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
