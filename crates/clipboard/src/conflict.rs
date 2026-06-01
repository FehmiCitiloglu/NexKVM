//! Clipboard conflict handling.
//!
//! When two devices copy at nearly the same time, both will try to push their
//! selection. Without coordination this produces a ping-pong: A applies B's
//! content, which A's own watcher then re-broadcasts, and so on. The
//! [`ConflictResolver`] solves both problems with a small amount of pure state:
//!
//! - **Echo suppression** — a content fingerprint of the currently-held
//!   selection. A local change whose fingerprint matches what we last applied
//!   (including content we just received) is *not* rebroadcast.
//! - **Last-writer-wins** — each update carries an [`OriginStamp`] ordered by a
//!   Lamport-style sequence, then wall-clock, then origin id as a deterministic
//!   tie-breaker. A stale inbound update (older than what we hold) is rejected,
//!   so all peers converge on the same selection regardless of arrival order.
//!
//! The resolver is sans-IO and fully deterministic, which makes the conflict
//! semantics unit-testable in isolation.

use coklu_core::identity::DeviceId;

use crate::content::ContentFingerprint;

/// Identifies *which* device produced a clipboard state and *when*, for
/// total-ordering concurrent updates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OriginStamp {
    /// Device that produced the selection.
    pub origin: DeviceId,
    /// Lamport-style logical sequence (monotonic per mesh as seen locally).
    pub seq: u64,
    /// Wall-clock millis at production (tie-breaker / diagnostics only).
    pub at_millis: u64,
}

impl OriginStamp {
    /// Whether `self` is strictly newer than `other` under the total order
    /// `(seq, at_millis, origin)`.
    #[must_use]
    pub fn supersedes(&self, other: &OriginStamp) -> bool {
        (self.seq, self.at_millis, self.origin.0) > (other.seq, other.at_millis, other.origin.0)
    }
}

/// What to do with an inbound update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboundDecision {
    /// Apply the update to the local clipboard and record it as current.
    Apply,
    /// Drop it: it is older than the selection we already hold.
    IgnoreStale,
    /// Drop it: it is identical to what we already hold (an echo).
    IgnoreEcho,
}

/// What to do with a locally observed clipboard change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalDecision {
    /// Broadcast it to peers under the given freshly-assigned stamp.
    Broadcast(OriginStamp),
    /// Suppress it: it merely echoes the selection we already hold.
    Suppress,
}

/// Tracks the currently-held selection and assigns/orders stamps.
#[derive(Debug)]
pub struct ConflictResolver {
    local: DeviceId,
    clock: u64,
    current: Option<(OriginStamp, ContentFingerprint)>,
}

impl ConflictResolver {
    /// Create a resolver for the local device, holding no selection yet.
    #[must_use]
    pub fn new(local: DeviceId) -> Self {
        Self {
            local,
            clock: 0,
            current: None,
        }
    }

    /// The stamp of the selection currently held, if any.
    #[must_use]
    pub fn current_stamp(&self) -> Option<OriginStamp> {
        self.current.map(|(s, _)| s)
    }

    /// Decide how to handle a locally observed clipboard change.
    ///
    /// `now_millis` is the current wall-clock used only for the stamp.
    pub fn on_local_change(
        &mut self,
        fingerprint: ContentFingerprint,
        now_millis: u64,
    ) -> LocalDecision {
        if let Some((_, fp)) = self.current {
            if fp == fingerprint {
                return LocalDecision::Suppress;
            }
        }
        self.clock += 1;
        let stamp = OriginStamp {
            origin: self.local,
            seq: self.clock,
            at_millis: now_millis,
        };
        self.current = Some((stamp, fingerprint));
        LocalDecision::Broadcast(stamp)
    }

    /// Decide how to handle an inbound update from a peer.
    ///
    /// Advances the logical clock past the peer's sequence so a subsequent local
    /// change is ordered after everything seen so far.
    pub fn on_inbound(
        &mut self,
        stamp: OriginStamp,
        fingerprint: ContentFingerprint,
    ) -> InboundDecision {
        self.clock = self.clock.max(stamp.seq);

        match self.current {
            Some((_, fp)) if fp == fingerprint => InboundDecision::IgnoreEcho,
            Some((cur, _)) if cur.supersedes(&stamp) => InboundDecision::IgnoreStale,
            _ => {
                self.current = Some((stamp, fingerprint));
                InboundDecision::Apply
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev() -> DeviceId {
        DeviceId::generate()
    }

    fn fp(n: u64) -> ContentFingerprint {
        ContentFingerprint(n)
    }

    #[test]
    fn local_change_broadcasts_then_suppresses_echo() {
        let mut r = ConflictResolver::new(dev());
        assert!(matches!(
            r.on_local_change(fp(1), 100),
            LocalDecision::Broadcast(_)
        ));
        // Same fingerprint observed again -> suppressed.
        assert_eq!(r.on_local_change(fp(1), 101), LocalDecision::Suppress);
    }

    #[test]
    fn applied_inbound_is_not_rebroadcast() {
        let local = dev();
        let peer = dev();
        let mut r = ConflictResolver::new(local);

        let stamp = OriginStamp {
            origin: peer,
            seq: 5,
            at_millis: 10,
        };
        assert_eq!(r.on_inbound(stamp, fp(42)), InboundDecision::Apply);
        // Our watcher now observes the applied content locally -> must suppress.
        assert_eq!(r.on_local_change(fp(42), 11), LocalDecision::Suppress);
    }

    #[test]
    fn stale_inbound_rejected() {
        let mut r = ConflictResolver::new(dev());
        let newer = OriginStamp {
            origin: dev(),
            seq: 10,
            at_millis: 5,
        };
        let older = OriginStamp {
            origin: dev(),
            seq: 3,
            at_millis: 5,
        };
        assert_eq!(r.on_inbound(newer, fp(1)), InboundDecision::Apply);
        assert_eq!(r.on_inbound(older, fp(2)), InboundDecision::IgnoreStale);
    }

    #[test]
    fn duplicate_inbound_is_echo() {
        let mut r = ConflictResolver::new(dev());
        let stamp = OriginStamp {
            origin: dev(),
            seq: 1,
            at_millis: 1,
        };
        assert_eq!(r.on_inbound(stamp, fp(7)), InboundDecision::Apply);
        let again = OriginStamp {
            origin: dev(),
            seq: 2,
            at_millis: 2,
        };
        assert_eq!(r.on_inbound(again, fp(7)), InboundDecision::IgnoreEcho);
    }

    #[test]
    fn clock_advances_past_peer_sequence() {
        let local = dev();
        let mut r = ConflictResolver::new(local);
        let peer_stamp = OriginStamp {
            origin: dev(),
            seq: 100,
            at_millis: 1,
        };
        r.on_inbound(peer_stamp, fp(1));
        // Local change must now be ordered after seq 100.
        if let LocalDecision::Broadcast(s) = r.on_local_change(fp(2), 2) {
            assert!(s.seq > 100);
        } else {
            panic!("expected broadcast");
        }
    }
}
