//! Crypto layer error types.

use thiserror::Error;

/// Errors from pairing, authentication, or session establishment.
#[derive(Debug, Error)]
pub enum CryptoError {
    /// The peer presented a key that is not in the local [`crate::TrustStore`].
    #[error("device is not paired/trusted")]
    Untrusted,

    /// Pairing confirmation (numeric code / QR) did not match.
    #[error("pairing verification failed")]
    PairingMismatch,

    /// A pairing payload (e.g. a QR bootstrap URI) was malformed.
    #[error("invalid pairing data: {0}")]
    Pairing(String),

    /// The pairing attempt expired before completion.
    #[error("pairing timed out")]
    PairingTimeout,

    /// A signature failed to verify against the claimed public key.
    #[error("signature verification failed")]
    BadSignature,

    /// Key agreement / handshake failure.
    #[error("key exchange failed: {0}")]
    KeyExchange(String),

    /// A replayed or out-of-window message was detected.
    #[error("replay detected for message id {0}")]
    Replay(u64),
}
