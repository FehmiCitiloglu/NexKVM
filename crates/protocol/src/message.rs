//! The message envelope that wraps every payload crossing the wire.

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::ProtocolVersion;

/// Monotonic per-connection message identifier.
///
/// Combined with a per-session nonce at the crypto layer, the `MessageId`
/// supports **replay-attack prevention**: a receiver tracks the highest id seen
/// and rejects out-of-window duplicates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct MessageId(pub u64);

impl MessageId {
    /// The first id on a fresh connection.
    pub const ZERO: MessageId = MessageId(0);

    /// Returns the next sequential id.
    #[must_use]
    pub const fn next(self) -> MessageId {
        MessageId(self.0.wrapping_add(1))
    }
}

/// Routes an [`Envelope`] body to the crate that owns its semantics.
///
/// Discriminants are stable and serialized as `u16`; **never renumber** an
/// existing variant (that is a major-version break). New variants may be added
/// in a minor version and unknown discriminants are surfaced as
/// [`ProtocolError::UnknownKind`](crate::ProtocolError::UnknownKind).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u16)]
#[non_exhaustive]
pub enum MessageKind {
    /// Version negotiation + device identity exchange. (`crypto`/`core`)
    Handshake = 0,
    /// Encrypted pairing handshake. (`crypto`)
    Pairing = 1,
    /// Keep-alive / connection liveness. (`network`)
    Heartbeat = 2,
    /// Input events: pointer, key, scroll. (`input`)
    Input = 10,
    /// Clipboard synchronization. (`clipboard`)
    Clipboard = 11,
    /// File / drag-and-drop transfer. (`clipboard`/`streaming`)
    FileTransfer = 12,
    /// Discovery announcements relayed over an established link. (`discovery`)
    Discovery = 13,
    /// Media / audio stream control. (`streaming`)
    Stream = 14,
    /// Plugin-addressed message. (`plugins`)
    Plugin = 20,
    /// Shared workspace command/control payload. (`core`)
    Workspace = 30,
    /// Cross-device notification payload. (`core`)
    Notification = 31,
    /// Universal quick command palette payload. (`core`)
    Command = 32,
    /// Decentralized mesh topology/routing payload. (`network`)
    Mesh = 40,
    /// Relay control-plane payload. (`network`)
    Relay = 41,
    /// Optional cloud-sync control payload. (`core`)
    CloudSync = 42,
    /// Enterprise management policy payload. (`core`)
    Enterprise = 43,
    /// Team collaboration management payload. (`core`)
    Team = 44,
    /// Browser remote session control payload. (`network`)
    BrowserSession = 45,
    /// Graceful shutdown / control. (`core`)
    Control = 100,
}

impl MessageKind {
    /// Parse a wire discriminant.
    #[must_use]
    pub fn from_u16(raw: u16) -> Option<Self> {
        match raw {
            0 => Some(Self::Handshake),
            1 => Some(Self::Pairing),
            2 => Some(Self::Heartbeat),
            10 => Some(Self::Input),
            11 => Some(Self::Clipboard),
            12 => Some(Self::FileTransfer),
            13 => Some(Self::Discovery),
            14 => Some(Self::Stream),
            20 => Some(Self::Plugin),
            30 => Some(Self::Workspace),
            31 => Some(Self::Notification),
            32 => Some(Self::Command),
            40 => Some(Self::Mesh),
            41 => Some(Self::Relay),
            42 => Some(Self::CloudSync),
            43 => Some(Self::Enterprise),
            44 => Some(Self::Team),
            45 => Some(Self::BrowserSession),
            100 => Some(Self::Control),
            _ => None,
        }
    }
}

/// The framed unit of communication.
///
/// The `body` is an opaque, possibly-encrypted blob owned by the crate matching
/// `kind`. Keeping it as [`Bytes`] enables zero-copy fan-out: a decoded frame
/// can be cheaply cloned to multiple consumers without copying the payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    /// Negotiated protocol version in effect for this connection.
    pub version: ProtocolVersion,
    /// Monotonic message id (replay protection + ordering).
    pub id: MessageId,
    /// Routing discriminant for the body.
    pub kind: MessageKind,
    /// Opaque payload owned by the `kind`'s crate.
    pub body: Bytes,
}

impl Envelope {
    /// Construct a new envelope at the current negotiated version.
    #[must_use]
    pub fn new(version: ProtocolVersion, id: MessageId, kind: MessageKind, body: Bytes) -> Self {
        Self {
            version,
            id,
            kind,
            body,
        }
    }
}
