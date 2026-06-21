//! Protocol error types.

use thiserror::Error;

/// Errors produced while encoding, decoding, or validating protocol data.
#[derive(Debug, Error)]
pub enum ProtocolError {
    /// A frame advertised a length larger than [`crate::MAX_FRAME_LEN`].
    #[error("frame length {len} exceeds maximum {max}")]
    FrameTooLarge {
        /// Advertised length.
        len: usize,
        /// Configured maximum.
        max: usize,
    },

    /// The inbound bytes are not a nexkvm frame stream (for example HTTP/TLS probes).
    #[error("protocol mismatch: {0}")]
    ProtocolMismatch(&'static str),

    /// The buffer did not yet contain a full frame; the caller should read more.
    #[error("incomplete frame: need {needed} more bytes")]
    Incomplete {
        /// Number of additional bytes required before a frame can be decoded.
        needed: usize,
    },

    /// The peer's protocol version is not compatible with ours.
    #[error("incompatible protocol version: peer={peer}, local supports {supported}")]
    IncompatibleVersion {
        /// The version advertised by the peer.
        peer: crate::ProtocolVersion,
        /// The local supported range, rendered for diagnostics.
        supported: crate::VersionRange,
    },

    /// Payload (de)serialization failed.
    #[error("payload codec error: {0}")]
    Codec(String),

    /// An unknown or unsupported message kind discriminant was received.
    #[error("unknown message kind: {0}")]
    UnknownKind(u16),
}
