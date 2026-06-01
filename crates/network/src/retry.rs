//! Connection retry with exponential backoff + jitter.
//!
//! Used by reconnection logic to recover from transient link failures without
//! hammering a peer. The backoff grows geometrically up to a cap, with full
//! jitter to avoid thundering-herd reconnection when many devices drop at once
//! (e.g. an AP reboot).

use std::time::Duration;

/// Exponential backoff schedule with full jitter.
#[derive(Debug, Clone)]
pub struct Backoff {
    initial: Duration,
    max: Duration,
    multiplier: f64,
    /// Current (un-jittered) delay; advances on each `next_delay`.
    current: Duration,
    /// Pseudo-random state for jitter (xorshift; no crypto need here).
    rng: u64,
}

impl Backoff {
    /// Create a backoff schedule.
    ///
    /// `seed` diversifies jitter across connections; pass any non-zero value
    /// (e.g. a peer id hash or a clock sample).
    #[must_use]
    pub fn new(initial: Duration, max: Duration, multiplier: f64, seed: u64) -> Self {
        Self {
            initial,
            max,
            multiplier: multiplier.max(1.0),
            current: initial,
            rng: seed | 1,
        }
    }

    /// Sensible LAN defaults: 250 ms initial, 30 s cap, doubling.
    #[must_use]
    pub fn lan_default(seed: u64) -> Self {
        Self::new(
            Duration::from_millis(250),
            Duration::from_secs(30),
            2.0,
            seed,
        )
    }

    /// Reset the schedule after a successful connection.
    pub fn reset(&mut self) {
        self.current = self.initial;
    }

    /// Return the next delay (with full jitter) and advance the schedule.
    ///
    /// The returned delay is uniformly sampled from `[0, current]` (full
    /// jitter), then `current` grows by `multiplier`, capped at `max`.
    pub fn next_delay(&mut self) -> Duration {
        let capped = self.current.min(self.max);
        let jittered = self.jitter(capped);

        // Advance for next time.
        let next = capped.mul_f64(self.multiplier).min(self.max);
        self.current = next;

        jittered
    }

    fn jitter(&mut self, ceiling: Duration) -> Duration {
        // xorshift64* for a cheap uniform sample.
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        let r = (self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64;
        ceiling.mul_f64(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grows_and_caps() {
        let mut b = Backoff::new(
            Duration::from_millis(100),
            Duration::from_millis(800),
            2.0,
            12345,
        );
        // Jitter means each sample is <= the (growing) ceiling.
        let mut last_ceiling = Duration::from_millis(100);
        for _ in 0..10 {
            let d = b.next_delay();
            assert!(d <= last_ceiling.max(Duration::from_millis(800)));
            last_ceiling = b.current;
        }
        // Ceiling must have saturated at max.
        assert_eq!(b.current, Duration::from_millis(800));
    }

    #[test]
    fn reset_restores_initial() {
        let mut b = Backoff::lan_default(7);
        let _ = b.next_delay();
        let _ = b.next_delay();
        b.reset();
        assert_eq!(b.current, Duration::from_millis(250));
    }

    #[test]
    fn jitter_within_bounds() {
        let mut b = Backoff::new(Duration::from_secs(10), Duration::from_secs(10), 1.0, 999);
        for _ in 0..100 {
            assert!(b.next_delay() <= Duration::from_secs(10));
        }
    }
}
