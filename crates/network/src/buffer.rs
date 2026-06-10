//! Adaptive outbound packet buffering / batching.
//!
//! Small messages (input deltas, control) benefit from being **coalesced** into
//! fewer transport writes to amortize per-packet overhead — but batching adds
//! latency, which is exactly what the real-time input path cannot afford. This
//! buffer resolves the tension by adapting the flush window to the measured link
//! latency:
//!
//! - **Low RTT (fast LAN)** → tiny window: flush almost immediately, favor
//!   latency.
//! - **Higher RTT** → larger window: batch more to cut packet count, since a few
//!   extra ms of batching is negligible against the link delay.
//!
//! It is a **sans-IO state machine**: [`push`](AdaptiveBuffer::push) enqueues,
//! [`deadline`](AdaptiveBuffer::deadline) reports when the caller should flush,
//! and [`drain`](AdaptiveBuffer::drain) yields the batch. The caller owns the
//! timer (e.g. `tokio::time::sleep_until`) so no hidden tasks are spawned and no
//! lock is held across `.await`.

use std::time::{Duration, Instant};

use nexkvm_protocol::Envelope;

/// Tunables for adaptive batching.
#[derive(Debug, Clone, Copy)]
pub struct BufferPolicy {
    /// Flush immediately once this many messages are queued.
    pub max_batch: usize,
    /// Lower bound on the flush window (latency floor).
    pub min_window: Duration,
    /// Upper bound on the flush window (never delay more than this).
    pub max_window: Duration,
    /// Fraction of measured RTT used as the window (e.g. 0.25 = quarter-RTT).
    pub rtt_fraction: f64,
}

impl Default for BufferPolicy {
    fn default() -> Self {
        Self {
            max_batch: 32,
            min_window: Duration::from_micros(200),
            max_window: Duration::from_millis(5),
            rtt_fraction: 0.25,
        }
    }
}

impl BufferPolicy {
    /// Compute the flush window for a given (optional) smoothed RTT, clamped to
    /// `[min_window, max_window]`.
    #[must_use]
    pub fn window_for(&self, rtt: Option<Duration>) -> Duration {
        match rtt {
            Some(rtt) => rtt
                .mul_f64(self.rtt_fraction)
                .clamp(self.min_window, self.max_window),
            // No estimate yet: bias toward low latency.
            None => self.min_window,
        }
    }
}

/// Accumulates outbound envelopes and flushes them as adaptive batches.
#[derive(Debug)]
pub struct AdaptiveBuffer {
    policy: BufferPolicy,
    queue: Vec<Envelope>,
    /// When the oldest currently-queued message was enqueued.
    first_enqueued: Option<Instant>,
    /// Current flush window, refreshed from RTT via [`Self::update_rtt`].
    window: Duration,
}

impl AdaptiveBuffer {
    /// Create a buffer with the given policy.
    #[must_use]
    pub fn new(policy: BufferPolicy) -> Self {
        let window = policy.window_for(None);
        Self {
            policy,
            queue: Vec::new(),
            first_enqueued: None,
            window,
        }
    }

    /// Update the adaptive window from a fresh smoothed RTT estimate.
    pub fn update_rtt(&mut self, rtt: Option<Duration>) {
        self.window = self.policy.window_for(rtt);
    }

    /// Enqueue an envelope, stamping the batch start if it is the first.
    pub fn push(&mut self, env: Envelope, now: Instant) {
        if self.queue.is_empty() {
            self.first_enqueued = Some(now);
        }
        self.queue.push(env);
    }

    /// Number of queued messages.
    #[must_use]
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Whether the buffer is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// The instant at which the caller should flush, or `None` if nothing is
    /// queued. Equals the enqueue time of the oldest message plus the window.
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        self.first_enqueued.map(|t| t + self.window)
    }

    /// Whether a flush is due as of `now`: either the batch is full or the
    /// window has elapsed.
    #[must_use]
    pub fn should_flush(&self, now: Instant) -> bool {
        if self.queue.is_empty() {
            return false;
        }
        self.queue.len() >= self.policy.max_batch || self.deadline().is_some_and(|d| now >= d)
    }

    /// Take all queued messages, resetting the buffer.
    #[must_use]
    pub fn drain(&mut self) -> Vec<Envelope> {
        self.first_enqueued = None;
        std::mem::take(&mut self.queue)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use nexkvm_protocol::{MessageId, MessageKind, PROTOCOL_VERSION};

    fn env(id: u64) -> Envelope {
        Envelope::new(
            PROTOCOL_VERSION,
            MessageId(id),
            MessageKind::Input,
            Bytes::new(),
        )
    }

    #[test]
    fn window_scales_with_rtt_and_clamps() {
        let p = BufferPolicy::default();
        // No RTT -> min window.
        assert_eq!(p.window_for(None), p.min_window);
        // Quarter of 8ms = 2ms, within bounds.
        assert_eq!(
            p.window_for(Some(Duration::from_millis(8))),
            Duration::from_millis(2)
        );
        // Large RTT clamps to max_window.
        assert_eq!(p.window_for(Some(Duration::from_secs(1))), p.max_window);
    }

    #[test]
    fn flushes_when_batch_full() {
        let mut b = AdaptiveBuffer::new(BufferPolicy {
            max_batch: 3,
            ..BufferPolicy::default()
        });
        let now = Instant::now();
        b.push(env(0), now);
        b.push(env(1), now);
        assert!(!b.should_flush(now));
        b.push(env(2), now);
        assert!(b.should_flush(now));
        assert_eq!(b.drain().len(), 3);
        assert!(b.is_empty());
    }

    #[test]
    fn flushes_when_window_elapses() {
        let mut b = AdaptiveBuffer::new(BufferPolicy {
            min_window: Duration::from_millis(2),
            ..BufferPolicy::default()
        });
        let now = Instant::now();
        b.push(env(0), now);
        assert!(!b.should_flush(now));
        assert!(b.should_flush(now + Duration::from_millis(3)));
    }

    #[test]
    fn deadline_tracks_oldest_message() {
        let mut b = AdaptiveBuffer::new(BufferPolicy::default());
        assert!(b.deadline().is_none());
        let now = Instant::now();
        b.push(env(0), now);
        assert_eq!(b.deadline(), Some(now + b.window));
    }
}
