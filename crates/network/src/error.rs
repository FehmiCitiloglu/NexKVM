//! Network error types.

use thiserror::Error;

/// Errors from transport setup or message exchange.
#[derive(Debug, Error)]
pub enum NetworkError {
    /// No configured transport could establish a connection.
    #[error("all transports failed to connect")]
    AllTransportsFailed,

    /// The selected transport backend was not compiled into this build.
    #[error("transport {0:?} is not enabled in this build")]
    TransportDisabled(crate::TransportKind),

    /// Underlying I/O failure.
    #[error("transport io error: {0}")]
    Io(#[from] std::io::Error),

    /// A protocol framing/encoding failure.
    #[error(transparent)]
    Protocol(#[from] nexkvm_protocol::ProtocolError),

    /// A security failure (auth, decrypt, replay) on the link.
    #[error(transparent)]
    Crypto(#[from] nexkvm_crypto::CryptoError),

    /// An operation exceeded its deadline.
    #[error("network operation timed out")]
    Timeout,

    /// The peer closed the connection.
    #[error("connection closed by peer")]
    Closed,
}
