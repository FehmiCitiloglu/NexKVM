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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plaintext_cipher_round_trips() {
        let c = PlaintextCipher;
        let sealed = c.seal(b"data").unwrap();
        assert_eq!(c.open(&sealed).unwrap(), b"data");
    }
}
