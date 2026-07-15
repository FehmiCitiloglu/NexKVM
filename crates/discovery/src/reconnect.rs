//! Auto-reconnect planner for trusted devices.
//!
//! Bridges discovery and trust: given the set of *currently visible* peers and a
//! predicate identifying which are *trusted*, it decides which devices are due
//! for a (re)connection attempt and paces retries with per-device exponential
//! backoff. This keeps a flapping or briefly-offline peer from being hammered
//! while still reconnecting promptly once it reappears.
//!
//! The planner is sans-IO and clock-injected: it never dials anything itself; it
//! emits [`ReconnectTarget`]s for a driver task to act on, and the driver feeds
//! results back via [`ReconnectPlanner::record_success`] /
//! [`ReconnectPlanner::record_failure`]. This separation keeps the policy unit
//! testable and decoupled from the transport crate.

use std::collections::HashMap;
use std::collections::HashSet;
use std::time::{Duration, Instant};

use nexkvm_core::identity::DeviceId;

use crate::DiscoveredDevice;

/// Backoff schedule for reconnection attempts.
#[derive(Debug, Clone)]
pub struct ReconnectPolicy {
    /// Delay before the first retry after a failure.
    pub base: Duration,
    /// Maximum delay between retries.
    pub max: Duration,
    /// Geometric growth factor per failed attempt (clamped to `>= 1.0`).
    pub multiplier: f64,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            base: Duration::from_secs(1),
            max: Duration::from_secs(30),
            multiplier: 2.0,
        }
    }
}

impl ReconnectPolicy {
    /// Delay after `attempts` consecutive failures (`attempts == 0` → `base`).
    #[must_use]
    fn delay_for(&self, attempts: u32) -> Duration {
        let mult = self.multiplier.max(1.0);
        let factor = mult.powi(attempts as i32);
        let scaled = self.base.as_secs_f64() * factor;
        let capped = scaled.min(self.max.as_secs_f64());
        Duration::from_secs_f64(capped)
    }
}

/// A device the planner recommends (re)connecting to now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconnectTarget {
    /// The discovered peer to dial.
    pub device: DiscoveredDevice,
    /// How many consecutive failures preceded this attempt (0 = first try).
    pub attempt: u32,
}

#[derive(Debug)]
enum DeviceState {
    Connected,
    Backoff {
        attempts: u32,
        next_eligible: Instant,
    },
}

/// Decides which trusted, visible devices to (re)connect to, with backoff.
#[derive(Debug)]
pub struct ReconnectPlanner {
    policy: ReconnectPolicy,
    state: HashMap<DeviceId, DeviceState>,
}

impl ReconnectPlanner {
    /// Create a planner with the given backoff policy.
    #[must_use]
    pub fn new(policy: ReconnectPolicy) -> Self {
        Self {
            policy,
            state: HashMap::new(),
        }
    }

    /// Targets among `visible` that are trusted and due for an attempt at `now`.
    ///
    /// `is_trusted` decides trust (typically a trust-store lookup keyed on the
    /// device's advertised fingerprint). Emitting a target arms a cooldown so
    /// the same device is not re-emitted until its backoff window elapses; call
    /// [`record_success`](Self::record_success) or
    /// [`record_failure`](Self::record_failure) once the attempt resolves.
    pub fn due<F>(
        &mut self,
        visible: &[DiscoveredDevice],
        is_trusted: F,
        now: Instant,
    ) -> Vec<ReconnectTarget>
    where
        F: Fn(&DiscoveredDevice) -> bool,
    {
        let visible_ids = visible
            .iter()
            .map(|device| device.info.id)
            .collect::<HashSet<_>>();
        self.state.retain(|id, state| {
            !matches!(state, DeviceState::Connected) || visible_ids.contains(id)
        });

        let mut targets = Vec::new();
        for device in visible.iter().filter(|d| is_trusted(d)) {
            let id = device.info.id;
            let attempts = match self.state.get(&id) {
                Some(DeviceState::Connected) => continue,
                Some(DeviceState::Backoff { next_eligible, .. }) if now < *next_eligible => {
                    continue;
                }
                Some(DeviceState::Backoff { attempts, .. }) => *attempts,
                None => 0,
            };
            targets.push(ReconnectTarget {
                device: device.clone(),
                attempt: attempts,
            });
            // Arm a cooldown so we don't re-emit while the attempt is in flight.
            self.state.insert(
                id,
                DeviceState::Backoff {
                    attempts,
                    next_eligible: now + self.policy.delay_for(attempts),
                },
            );
        }
        targets
    }

    /// Mark a device connected so discovery does not dial it again while it
    /// remains visible. Its state is cleared after it disappears.
    pub fn record_success(&mut self, id: DeviceId) {
        self.state.insert(id, DeviceState::Connected);
    }

    /// Record a failed attempt, growing the device's backoff window.
    pub fn record_failure(&mut self, id: DeviceId, now: Instant) {
        let attempts = match self.state.get(&id) {
            Some(DeviceState::Backoff { attempts, .. }) => attempts.saturating_add(1),
            Some(DeviceState::Connected) | None => 1,
        };
        self.state.insert(
            id,
            DeviceState::Backoff {
                attempts,
                next_eligible: now + self.policy.delay_for(attempts),
            },
        );
    }

    /// Forget all per-device backoff state.
    pub fn clear(&mut self) {
        self.state.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexkvm_core::identity::{DeviceInfo, OsKind};
    use std::net::SocketAddr;

    fn device(name: &str, fp: Option<&str>) -> DiscoveredDevice {
        DiscoveredDevice {
            info: DeviceInfo::new(name, OsKind::MacOs),
            addr: "127.0.0.1:47654".parse::<SocketAddr>().unwrap(),
            fingerprint: fp.map(str::to_string),
        }
    }

    #[test]
    fn only_trusted_devices_are_targeted() {
        let mut planner = ReconnectPlanner::new(ReconnectPolicy::default());
        let trusted = device("trusted", Some("aa:bb"));
        let stranger = device("stranger", Some("zz:zz"));
        let visible = vec![trusted.clone(), stranger];
        let now = Instant::now();

        let targets = planner.due(&visible, |d| d.fingerprint.as_deref() == Some("aa:bb"), now);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].device.info.id, trusted.info.id);
        assert_eq!(targets[0].attempt, 0);
    }

    #[test]
    fn due_arms_cooldown_to_avoid_immediate_reemit() {
        let mut planner = ReconnectPlanner::new(ReconnectPolicy::default());
        let dev = device("d", Some("fp"));
        let visible = vec![dev];
        let now = Instant::now();

        assert_eq!(planner.due(&visible, |_| true, now).len(), 1);
        // Immediately calling again yields nothing (still cooling down).
        assert!(planner.due(&visible, |_| true, now).is_empty());
    }

    #[test]
    fn failure_grows_backoff_then_eligible_again() {
        let policy = ReconnectPolicy {
            base: Duration::from_secs(1),
            max: Duration::from_secs(30),
            multiplier: 2.0,
        };
        let mut planner = ReconnectPlanner::new(policy);
        let dev = device("d", Some("fp"));
        let visible = vec![dev.clone()];
        let t0 = Instant::now();

        let first = planner.due(&visible, |_| true, t0);
        assert_eq!(first[0].attempt, 0);
        planner.record_failure(dev.info.id, t0);

        // After one failure, backoff is ~2s; not yet eligible at +1s.
        assert!(
            planner
                .due(&visible, |_| true, t0 + Duration::from_secs(1))
                .is_empty()
        );
        // Eligible after the window; this is the second attempt.
        let second = planner.due(&visible, |_| true, t0 + Duration::from_secs(3));
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].attempt, 1);
    }

    #[test]
    fn success_suppresses_reconnect_until_peer_disappears() {
        let mut planner = ReconnectPlanner::new(ReconnectPolicy::default());
        let dev = device("d", Some("fp"));
        let visible = vec![dev.clone()];
        let t0 = Instant::now();

        planner.due(&visible, |_| true, t0);
        planner.record_success(dev.info.id);

        assert!(
            planner
                .due(&visible, |_| true, t0 + Duration::from_secs(60))
                .is_empty(),
            "a connected peer must not be dialed again"
        );

        assert!(planner.due(&[], |_| true, t0).is_empty());
        let targets = planner.due(&visible, |_| true, t0 + Duration::from_secs(61));
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].attempt, 0);
    }
}
