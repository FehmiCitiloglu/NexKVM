//! Adaptive input polling cadence.
//!
//! Platform input backends can use this pure state machine to choose a polling
//! interval from recent activity and network quality. Event-driven backends can
//! ignore it; polling backends should keep any blocking OS calls off Tokio's
//! async runtime.

use std::time::{Duration, Instant};

/// Tunables for adaptive polling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PollingPolicy {
    /// Fastest polling interval while active.
    pub min_interval: Duration,
    /// Slowest polling interval while idle.
    pub max_interval: Duration,
    /// How long after activity to stay at the minimum interval.
    pub active_grace: Duration,
    /// Backoff step when idle.
    pub idle_step: Duration,
}

impl Default for PollingPolicy {
    fn default() -> Self {
        Self {
            min_interval: Duration::from_millis(1),
            max_interval: Duration::from_millis(16),
            active_grace: Duration::from_millis(250),
            idle_step: Duration::from_millis(2),
        }
    }
}

/// Adaptive polling interval state.
#[derive(Debug, Clone)]
pub struct AdaptivePoller {
    policy: PollingPolicy,
    last_activity: Option<Instant>,
    current: Duration,
}

impl AdaptivePoller {
    /// Create a poller.
    #[must_use]
    pub fn new(policy: PollingPolicy) -> Self {
        Self {
            current: policy.max_interval,
            policy,
            last_activity: None,
        }
    }

    /// Record local input activity and return the new interval.
    pub fn record_activity(&mut self, now: Instant) -> Duration {
        self.last_activity = Some(now);
        self.current = self.policy.min_interval;
        self.current
    }

    /// Return the interval to use at `now`.
    pub fn interval(&mut self, now: Instant) -> Duration {
        if self
            .last_activity
            .is_some_and(|at| now.saturating_duration_since(at) <= self.policy.active_grace)
        {
            self.current = self.policy.min_interval;
        } else {
            self.current = (self.current + self.policy.idle_step).min(self.policy.max_interval);
        }
        self.current
    }

    /// Bias polling from network quality: high jitter/loss reduces send pressure.
    pub fn apply_network_pressure(&mut self, jitter: Duration, loss: f64) {
        if jitter > Duration::from_millis(30) || loss >= 0.05 {
            self.current = (self.current + self.policy.idle_step).min(self.policy.max_interval);
        } else {
            self.current = self
                .current
                .saturating_sub(self.policy.idle_step)
                .max(self.policy.min_interval);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_uses_minimum_interval_then_backs_off() {
        let mut poller = AdaptivePoller::new(PollingPolicy::default());
        let now = Instant::now();
        assert_eq!(poller.record_activity(now), Duration::from_millis(1));
        assert_eq!(
            poller.interval(now + Duration::from_millis(100)),
            Duration::from_millis(1)
        );
        assert!(poller.interval(now + Duration::from_secs(1)) > Duration::from_millis(1));
    }

    #[test]
    fn network_pressure_increases_interval() {
        let mut poller = AdaptivePoller::new(PollingPolicy::default());
        let now = Instant::now();
        poller.record_activity(now);
        poller.apply_network_pressure(Duration::from_millis(40), 0.0);
        assert!(poller.interval(now + Duration::from_secs(1)) > Duration::from_millis(1));
    }
}
