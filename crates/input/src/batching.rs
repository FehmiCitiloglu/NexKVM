//! Latency-aware input batching.
//!
//! This wraps [`crate::InputCoalescer`] with a small deadline/max-count policy.
//! The caller owns the timer and transport write; this type only decides when a
//! batch is ready, avoiding hidden async tasks or locks across `.await`.

use std::time::{Duration, Instant};

use crate::{InputCoalescer, InputEvent};

/// Tunables for input event batching.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InputBatchPolicy {
    /// Flush immediately once this many source events have been observed.
    pub max_source_events: usize,
    /// Minimum batching window.
    pub min_delay: Duration,
    /// Maximum batching window.
    pub max_delay: Duration,
    /// Fraction of smoothed RTT used as the batching delay.
    pub rtt_fraction: f64,
}

impl InputBatchPolicy {
    /// Low-latency preset for LAN/control sessions.
    #[must_use]
    pub const fn low_latency() -> Self {
        Self {
            max_source_events: 4,
            min_delay: Duration::ZERO,
            max_delay: Duration::from_millis(2),
            rtt_fraction: 0.05,
        }
    }

    /// Balanced preset for general desktop continuity.
    #[must_use]
    pub const fn balanced() -> Self {
        Self {
            max_source_events: 12,
            min_delay: Duration::from_micros(500),
            max_delay: Duration::from_millis(5),
            rtt_fraction: 0.10,
        }
    }

    /// Compute a delay from an optional RTT estimate.
    #[must_use]
    pub fn delay_for(self, rtt: Option<Duration>) -> Duration {
        match rtt {
            Some(rtt) => rtt
                .mul_f64(self.rtt_fraction)
                .clamp(self.min_delay, self.max_delay),
            None => self.min_delay,
        }
    }
}

impl Default for InputBatchPolicy {
    fn default() -> Self {
        Self::balanced()
    }
}

/// Input batcher that coalesces motion and preserves ordered non-motion events.
#[derive(Debug)]
pub struct InputBatcher {
    policy: InputBatchPolicy,
    coalescer: InputCoalescer,
    first_event_at: Option<Instant>,
    source_events: usize,
    delay: Duration,
}

impl InputBatcher {
    /// Create a batcher.
    #[must_use]
    pub fn new(policy: InputBatchPolicy) -> Self {
        Self {
            policy,
            coalescer: InputCoalescer::new(),
            first_event_at: None,
            source_events: 0,
            delay: policy.delay_for(None),
        }
    }

    /// Update the adaptive delay from smoothed RTT.
    pub fn update_rtt(&mut self, rtt: Option<Duration>) {
        self.delay = self.policy.delay_for(rtt);
    }

    /// Push one source event.
    pub fn push(&mut self, event: InputEvent, now: Instant) {
        if self.first_event_at.is_none() {
            self.first_event_at = Some(now);
        }
        self.source_events = self.source_events.saturating_add(1);
        self.coalescer.push(event);
    }

    /// Current flush deadline.
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        self.first_event_at.map(|at| at + self.delay)
    }

    /// Whether the current batch should flush at `now`.
    #[must_use]
    pub fn should_flush(&self, now: Instant) -> bool {
        if self.source_events == 0 {
            return false;
        }
        self.source_events >= self.policy.max_source_events
            || self.deadline().is_some_and(|deadline| now >= deadline)
    }

    /// Drain the coalesced output batch.
    pub fn drain(&mut self) -> Vec<InputEvent> {
        self.first_event_at = None;
        self.source_events = 0;
        self.coalescer.drain()
    }

    /// Whether nothing is buffered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.source_events == 0 && self.coalescer.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_source_events_triggers_flush() {
        let mut batcher = InputBatcher::new(InputBatchPolicy {
            max_source_events: 2,
            ..InputBatchPolicy::balanced()
        });
        let now = Instant::now();
        batcher.push(InputEvent::RelativeMove { dx: 1.0, dy: 0.0 }, now);
        assert!(!batcher.should_flush(now));
        batcher.push(InputEvent::RelativeMove { dx: 2.0, dy: 0.0 }, now);
        assert!(batcher.should_flush(now));
        assert_eq!(
            batcher.drain(),
            vec![InputEvent::RelativeMove { dx: 3.0, dy: 0.0 }]
        );
    }

    #[test]
    fn deadline_scales_with_rtt() {
        let policy = InputBatchPolicy::balanced();
        assert_eq!(
            policy.delay_for(Some(Duration::from_millis(20))),
            Duration::from_millis(2)
        );
        assert_eq!(
            policy.delay_for(Some(Duration::from_secs(1))),
            policy.max_delay
        );
    }
}
