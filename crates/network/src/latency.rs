//! Latency measurement ⚡
//!
//! Round-trip time is sampled by the [`heartbeat`](crate::heartbeat) ping/pong
//! exchange and fed into an exponentially-weighted moving average (EWMA), the
//! same smoothing TCP uses for its RTT estimator (RFC 6298). The smoothed RTT
//! and jitter drive:
//! - the [`heartbeat`](crate::heartbeat) liveness timeout,
//! - the [`buffer`](crate::buffer) adaptive batching window,
//! - UI/telemetry connection-quality reporting.

use std::time::Duration;

/// EWMA round-trip-time estimator with jitter (mean deviation).
///
/// Mirrors RFC 6298: `srtt = (1-a)·srtt + a·sample`,
/// `rttvar = (1-b)·rttvar + b·|srtt - sample|`.
#[derive(Debug, Clone)]
pub struct RttTracker {
    alpha: f64,
    beta: f64,
    srtt: Option<f64>,
    rttvar: f64,
    last: Option<f64>,
    samples: u64,
}

impl Default for RttTracker {
    fn default() -> Self {
        // RFC 6298 recommended gains.
        Self::new(0.125, 0.25)
    }
}

impl RttTracker {
    /// Create a tracker with custom smoothing gains (`alpha` for srtt, `beta`
    /// for jitter). Both are clamped to `(0, 1]`.
    #[must_use]
    pub fn new(alpha: f64, beta: f64) -> Self {
        Self {
            alpha: alpha.clamp(f64::EPSILON, 1.0),
            beta: beta.clamp(f64::EPSILON, 1.0),
            srtt: None,
            rttvar: 0.0,
            last: None,
            samples: 0,
        }
    }

    /// Record an RTT sample.
    pub fn record(&mut self, sample: Duration) {
        let r = sample.as_secs_f64();
        self.last = Some(r);
        self.samples += 1;
        match self.srtt {
            None => {
                // First sample initializes the estimators (RFC 6298 §2.2).
                self.srtt = Some(r);
                self.rttvar = r / 2.0;
            }
            Some(srtt) => {
                let delta = (srtt - r).abs();
                self.rttvar = (1.0 - self.beta) * self.rttvar + self.beta * delta;
                self.srtt = Some((1.0 - self.alpha) * srtt + self.alpha * r);
            }
        }
    }

    /// Smoothed round-trip time, if at least one sample was recorded.
    #[must_use]
    pub fn smoothed(&self) -> Option<Duration> {
        self.srtt.map(Duration::from_secs_f64)
    }

    /// Jitter estimate (RTT variation), if sampled.
    #[must_use]
    pub fn jitter(&self) -> Option<Duration> {
        if self.samples == 0 {
            None
        } else {
            Some(Duration::from_secs_f64(self.rttvar))
        }
    }

    /// Most recent raw sample, if any.
    #[must_use]
    pub fn last(&self) -> Option<Duration> {
        self.last.map(Duration::from_secs_f64)
    }

    /// A liveness/retransmit timeout derived from the estimate:
    /// `srtt + 4·rttvar` (RFC 6298), floored at `min`.
    #[must_use]
    pub fn timeout(&self, min: Duration) -> Duration {
        match self.srtt {
            Some(srtt) => Duration::from_secs_f64(srtt + 4.0 * self.rttvar).max(min),
            None => min,
        }
    }

    /// Number of samples recorded.
    #[must_use]
    pub fn sample_count(&self) -> u64 {
        self.samples
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_tracker_has_no_estimate() {
        let t = RttTracker::default();
        assert!(t.smoothed().is_none());
        assert!(t.jitter().is_none());
        assert_eq!(t.timeout(Duration::from_secs(1)), Duration::from_secs(1));
    }

    #[test]
    fn first_sample_seeds_estimate() {
        let mut t = RttTracker::default();
        t.record(Duration::from_millis(20));
        assert_eq!(t.smoothed(), Some(Duration::from_millis(20)));
        assert_eq!(t.sample_count(), 1);
    }

    #[test]
    fn converges_toward_steady_state() {
        let mut t = RttTracker::default();
        for _ in 0..200 {
            t.record(Duration::from_millis(10));
        }
        let s = t.smoothed().unwrap();
        // Should be very close to 10ms after many identical samples.
        assert!((s.as_secs_f64() - 0.010).abs() < 0.001);
        // Jitter collapses toward zero for a constant signal.
        assert!(t.jitter().unwrap() < Duration::from_millis(1));
    }

    #[test]
    fn timeout_exceeds_srtt_under_jitter() {
        let mut t = RttTracker::default();
        t.record(Duration::from_millis(10));
        t.record(Duration::from_millis(50));
        t.record(Duration::from_millis(10));
        let to = t.timeout(Duration::from_millis(1));
        assert!(to > t.smoothed().unwrap());
    }
}
