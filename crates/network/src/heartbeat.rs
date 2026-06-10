//! Heartbeat & liveness.
//!
//! A periodic [`MessageKind::Heartbeat`](nexkvm_protocol::MessageKind::Heartbeat)
//! exchange keeps idle links warm (so cursor handoff is instant) and detects
//! dead peers. Each ping carries a monotonic send timestamp that the peer echoes
//! in a pong, yielding an RTT sample for [`RttTracker`](crate::latency::RttTracker).
//!
//! This module is transport-agnostic: it produces/parses heartbeat *payloads*
//! and tracks liveness state. The actual send timing loop is driven by the
//! connection owner (see [`HeartbeatConfig`]). Keeping the timer external avoids
//! spawning hidden tasks and lets the caller integrate it into its own
//! `tokio::select!` loop.

use std::time::{Duration, Instant};

use bytes::{Buf, BufMut, Bytes, BytesMut};
use nexkvm_protocol::ProtocolError;

/// Heartbeat payload: a ping or a pong carrying a nanosecond timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Heartbeat {
    /// Liveness probe; `t_nanos` is the sender's monotonic clock reading.
    Ping {
        /// Sender timestamp (opaque to the peer; echoed back verbatim).
        t_nanos: u64,
    },
    /// Reply echoing the ping's timestamp so the originator can compute RTT.
    Pong {
        /// The `t_nanos` copied from the corresponding ping.
        t_nanos: u64,
    },
}

const TAG_PING: u8 = 0;
const TAG_PONG: u8 = 1;

impl Heartbeat {
    /// Encode to a payload buffer (`tag:u8` + `t_nanos:u64 BE`).
    #[must_use]
    pub fn encode(self) -> Bytes {
        let mut buf = BytesMut::with_capacity(9);
        match self {
            Heartbeat::Ping { t_nanos } => {
                buf.put_u8(TAG_PING);
                buf.put_u64(t_nanos);
            }
            Heartbeat::Pong { t_nanos } => {
                buf.put_u8(TAG_PONG);
                buf.put_u64(t_nanos);
            }
        }
        buf.freeze()
    }

    /// Decode a heartbeat payload.
    ///
    /// # Errors
    /// Returns [`ProtocolError`] if the payload is malformed or the tag unknown.
    pub fn decode(mut payload: Bytes) -> Result<Self, ProtocolError> {
        if payload.len() < 9 {
            return Err(ProtocolError::Incomplete {
                needed: 9 - payload.len(),
            });
        }
        let tag = payload.get_u8();
        let t_nanos = payload.get_u64();
        match tag {
            TAG_PING => Ok(Heartbeat::Ping { t_nanos }),
            TAG_PONG => Ok(Heartbeat::Pong { t_nanos }),
            other => Err(ProtocolError::Codec(format!("bad heartbeat tag {other}"))),
        }
    }

    /// Build the pong that answers this message (no-op echo if already a pong).
    #[must_use]
    pub fn reply(self) -> Heartbeat {
        match self {
            Heartbeat::Ping { t_nanos } => Heartbeat::Pong { t_nanos },
            pong => pong,
        }
    }
}

/// Heartbeat timing policy.
#[derive(Debug, Clone, Copy)]
pub struct HeartbeatConfig {
    /// How often to send a ping on an otherwise idle link.
    pub interval: Duration,
    /// Mark the peer dead if nothing is received for this long.
    pub timeout: Duration,
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        // Keepalive well below timeout so a single lost ping doesn't drop a link.
        Self {
            interval: Duration::from_secs(2),
            timeout: Duration::from_secs(8),
        }
    }
}

/// Tracks the last time anything was received from a peer to judge liveness.
///
/// Uses a monotonic [`Instant`] reference clock, so it is unaffected by wall-clock
/// changes (NTP steps, suspend/resume).
#[derive(Debug, Clone)]
pub struct LivenessMonitor {
    timeout: Duration,
    last_seen: Instant,
}

impl LivenessMonitor {
    /// Create a monitor, treating "now" as the initial last-seen time.
    #[must_use]
    pub fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            last_seen: Instant::now(),
        }
    }

    /// Record that traffic (any message) was received from the peer.
    pub fn record_activity(&mut self) {
        self.last_seen = Instant::now();
    }

    /// Whether the peer is considered alive as of `now`.
    #[must_use]
    pub fn is_alive_at(&self, now: Instant) -> bool {
        now.duration_since(self.last_seen) < self.timeout
    }

    /// Convenience: liveness as of the current instant.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.is_alive_at(Instant::now())
    }

    /// Time since the last received activity, as of `now`.
    #[must_use]
    pub fn idle_for(&self, now: Instant) -> Duration {
        now.duration_since(self.last_seen)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_pong_round_trips() {
        let ping = Heartbeat::Ping { t_nanos: 123456 };
        let decoded = Heartbeat::decode(ping.encode()).unwrap();
        assert_eq!(decoded, ping);

        let pong = decoded.reply();
        assert_eq!(pong, Heartbeat::Pong { t_nanos: 123456 });
        assert_eq!(Heartbeat::decode(pong.encode()).unwrap(), pong);
    }

    #[test]
    fn rejects_short_and_bad_tag() {
        assert!(Heartbeat::decode(Bytes::from_static(&[0u8; 3])).is_err());
        let mut bad = BytesMut::new();
        bad.put_u8(99);
        bad.put_u64(0);
        assert!(Heartbeat::decode(bad.freeze()).is_err());
    }

    #[test]
    fn liveness_expires_after_timeout() {
        let m = LivenessMonitor::new(Duration::from_millis(100));
        let now = Instant::now();
        assert!(m.is_alive_at(now));
        assert!(!m.is_alive_at(now + Duration::from_millis(150)));
    }

    #[test]
    fn activity_keeps_alive() {
        let mut m = LivenessMonitor::new(Duration::from_secs(5));
        m.record_activity();
        assert!(m.is_alive());
    }
}
