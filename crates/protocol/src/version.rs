//! Protocol versioning strategy.
//!
//! coklu uses **semantic, negotiated versioning** at connection setup:
//!
//! - `major` bumps on wire-breaking changes (frame layout, envelope fields,
//!   removal of a [`MessageKind`](crate::MessageKind)). Peers with mismatched
//!   majors refuse to connect.
//! - `minor` bumps on backward-compatible additions (new message kinds,
//!   optional fields). A newer peer may talk to an older one by restricting
//!   itself to the lower negotiated minor.
//!
//! During the handshake each side sends its [`ProtocolVersion`] and the
//! supported [`VersionRange`]. The effective version is the highest minor that
//! both peers support at a shared major; if no major matches the connection is
//! rejected with [`ProtocolError::IncompatibleVersion`](crate::ProtocolError).

use std::fmt;

use serde::{Deserialize, Serialize};

/// The protocol version this build implements.
pub const PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion { major: 1, minor: 0 };

/// A semantic protocol version (`major.minor`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ProtocolVersion {
    /// Wire-breaking version. Must match exactly between peers.
    pub major: u16,
    /// Backward-compatible feature level.
    pub minor: u16,
}

impl ProtocolVersion {
    /// Returns `true` if `self` can interoperate with `other` (same major).
    #[must_use]
    pub const fn is_compatible_with(self, other: ProtocolVersion) -> bool {
        self.major == other.major
    }
}

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// An inclusive range of protocol versions a peer is willing to speak.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionRange {
    /// Lowest supported version.
    pub min: ProtocolVersion,
    /// Highest supported version (typically [`PROTOCOL_VERSION`]).
    pub max: ProtocolVersion,
}

impl VersionRange {
    /// The range supported by this build. Currently a single major line.
    #[must_use]
    pub const fn current() -> Self {
        Self {
            min: ProtocolVersion { major: 1, minor: 0 },
            max: PROTOCOL_VERSION,
        }
    }

    /// Negotiate the effective version against a peer's advertised version.
    ///
    /// Returns the highest mutually supported version, or `None` if the peer's
    /// major is outside this range.
    #[must_use]
    pub fn negotiate(self, peer: ProtocolVersion) -> Option<ProtocolVersion> {
        if peer.major != self.max.major {
            return None;
        }
        // Same major: cap the minor at whatever both sides understand.
        let minor = peer.minor.min(self.max.minor);
        Some(ProtocolVersion {
            major: self.max.major,
            minor,
        })
    }
}

impl fmt::Display for VersionRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}..={}", self.min, self.max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiates_down_to_shared_minor() {
        let range = VersionRange::current();
        let peer = ProtocolVersion { major: 1, minor: 5 };
        assert_eq!(range.negotiate(peer), Some(PROTOCOL_VERSION));
    }

    #[test]
    fn rejects_mismatched_major() {
        let range = VersionRange::current();
        let peer = ProtocolVersion { major: 2, minor: 0 };
        assert_eq!(range.negotiate(peer), None);
    }
}
