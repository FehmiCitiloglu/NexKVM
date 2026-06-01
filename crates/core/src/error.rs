//! Core error type.

use thiserror::Error;

/// Errors surfaced by core orchestration.
#[derive(Debug, Error)]
pub enum CoreError {
    /// A protocol-level failure bubbled up.
    #[error(transparent)]
    Protocol(#[from] coklu_protocol::ProtocolError),

    /// A security/pairing failure bubbled up.
    #[error(transparent)]
    Crypto(#[from] coklu_crypto::CryptoError),

    /// The event bus has no remaining receivers (all consumers dropped).
    #[error("event bus has no active subscribers")]
    NoSubscribers,

    /// A requested capability is unavailable on this platform.
    #[error("capability unavailable on this platform: {0}")]
    Unsupported(&'static str),
}
