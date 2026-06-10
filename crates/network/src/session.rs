//! Session state & persistence.
//!
//! A [`Session`] is the logical, transport-independent association with a peer.
//! It **survives reconnects**: when the underlying [`Connection`](crate::Connection)
//! drops and the [`retry`](crate::retry) logic re-establishes a link, the same
//! `Session` is resumed, preserving the monotonic message-id counter and peer
//! identity. This is what makes replay protection and ordering coherent across a
//! flaky link rather than resetting on every reconnect.
//!
//! Persistence here is **in-process** for this phase: the live association and
//! its resumption token. Durable on-disk session caching (for fast 0-RTT-style
//! resume across app restarts) layers on top via the `storage` crate later.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use nexkvm_core::DeviceId;
use nexkvm_protocol::{MessageId, ProtocolVersion};

/// Opaque token identifying a resumable session with a peer.
///
/// Exchanged at handshake; presenting a known token on reconnect lets both
/// sides resume id sequencing instead of starting a fresh session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionToken(pub u128);

/// Live, resumable association with a single peer device.
///
/// Cloneable: all clones share the same id counter and resumption state via an
/// inner `Arc`, so multiple tasks (sender, heartbeat, buffer flusher) allocate
/// ids from one monotonic sequence without a lock held across `.await`.
#[derive(Debug, Clone)]
pub struct Session {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    peer: DeviceId,
    token: SessionToken,
    version: ProtocolVersion,
    /// Next id to hand out; `AtomicU64` for lock-free allocation on hot paths.
    next_id: AtomicU64,
    established_at: Instant,
}

impl Session {
    /// Begin a fresh session with `peer` at the negotiated `version`.
    #[must_use]
    pub fn new(peer: DeviceId, token: SessionToken, version: ProtocolVersion) -> Self {
        Self {
            inner: Arc::new(Inner {
                peer,
                token,
                version,
                next_id: AtomicU64::new(0),
                established_at: Instant::now(),
            }),
        }
    }

    /// Resume a previously-established session, continuing the id sequence from
    /// `resume_from` (the highest id known to have been used).
    ///
    /// Keeping ids monotonic across reconnects preserves the replay-protection
    /// invariant in [`nexkvm_crypto`]: the peer never sees an id
    /// rewind.
    #[must_use]
    pub fn resume(
        peer: DeviceId,
        token: SessionToken,
        version: ProtocolVersion,
        resume_from: MessageId,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                peer,
                token,
                version,
                next_id: AtomicU64::new(resume_from.0.wrapping_add(1)),
                established_at: Instant::now(),
            }),
        }
    }

    /// Allocate the next monotonic [`MessageId`] (lock-free).
    #[must_use]
    pub fn alloc_id(&self) -> MessageId {
        MessageId(self.inner.next_id.fetch_add(1, Ordering::Relaxed))
    }

    /// The highest id allocated so far (for building a resumption checkpoint).
    #[must_use]
    pub fn high_water_mark(&self) -> MessageId {
        let next = self.inner.next_id.load(Ordering::Relaxed);
        MessageId(next.saturating_sub(1))
    }

    /// Peer device id.
    #[must_use]
    pub fn peer(&self) -> DeviceId {
        self.inner.peer
    }

    /// Resumption token.
    #[must_use]
    pub fn token(&self) -> SessionToken {
        self.inner.token
    }

    /// Negotiated protocol version for this session.
    #[must_use]
    pub fn version(&self) -> ProtocolVersion {
        self.inner.version
    }

    /// When this session instance was established (monotonic).
    #[must_use]
    pub fn established_at(&self) -> Instant {
        self.inner.established_at
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexkvm_protocol::PROTOCOL_VERSION;

    fn peer() -> DeviceId {
        DeviceId::generate()
    }

    #[test]
    fn allocates_monotonic_ids() {
        let s = Session::new(peer(), SessionToken(1), PROTOCOL_VERSION);
        assert_eq!(s.alloc_id(), MessageId(0));
        assert_eq!(s.alloc_id(), MessageId(1));
        assert_eq!(s.alloc_id(), MessageId(2));
        assert_eq!(s.high_water_mark(), MessageId(2));
    }

    #[test]
    fn resume_continues_sequence() {
        let p = peer();
        let s = Session::resume(p, SessionToken(9), PROTOCOL_VERSION, MessageId(41));
        // Next id must be one past the resume point — no rewind.
        assert_eq!(s.alloc_id(), MessageId(42));
        assert_eq!(s.peer(), p);
        assert_eq!(s.token(), SessionToken(9));
    }

    #[test]
    fn clones_share_counter() {
        let s = Session::new(peer(), SessionToken(2), PROTOCOL_VERSION);
        let c = s.clone();
        assert_eq!(s.alloc_id(), MessageId(0));
        assert_eq!(c.alloc_id(), MessageId(1));
        assert_eq!(s.alloc_id(), MessageId(2));
    }
}
