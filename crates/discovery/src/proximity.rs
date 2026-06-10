//! Device proximity and presence scoring.
//!
//! Proximity is a UX signal, not an identity proof. RSSI, latency, and manual
//! presence hints can help choose the most likely target for automatic device
//! switching, but secure pairing and authenticated encrypted sessions still own
//! trust. This module is pure policy/state and does not talk to Bluetooth,
//! Wi-Fi, mDNS, or platform presence APIs directly.

use std::collections::HashMap;

use nexkvm_core::identity::DeviceId;
use serde::{Deserialize, Serialize};

/// Source of a proximity signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ProximitySignalKind {
    /// LAN/mDNS heartbeat round-trip or freshness signal.
    LanHeartbeat,
    /// Bluetooth RSSI or similar short-range signal.
    BluetoothRssi,
    /// Wi-Fi RSSI or access-point co-location signal.
    WifiRssi,
    /// User or platform presence hint (active session, unlocked screen, etc.).
    PresenceHint,
}

/// One normalized observation for a device.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ProximityObservation {
    /// Observed device.
    pub device: DeviceId,
    /// Signal source.
    pub kind: ProximitySignalKind,
    /// Normalized confidence in `[0, 1]`; higher means closer/more present.
    pub confidence: f64,
    /// Wall-clock milliseconds chosen by the caller.
    pub at_millis: u64,
}

impl ProximityObservation {
    /// Construct an observation, clamping confidence to `[0, 1]`.
    #[must_use]
    pub fn new(
        device: DeviceId,
        kind: ProximitySignalKind,
        confidence: f64,
        at_millis: u64,
    ) -> Self {
        Self {
            device,
            kind,
            confidence: confidence.clamp(0.0, 1.0),
            at_millis,
        }
    }
}

/// Coarse device presence bucket used by automatic switching policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PresenceState {
    /// No fresh signal.
    Away,
    /// Nearby enough to show in suggestions.
    Nearby,
    /// Likely in the user's current workspace.
    AtDesk,
    /// Strong signal; eligible for automatic switching when policy allows it.
    Active,
}

/// Proximity policy thresholds.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PresencePolicy {
    /// Observations older than this are ignored.
    pub stale_after_millis: u64,
    /// Minimum score for [`PresenceState::Nearby`].
    pub nearby_score: f64,
    /// Minimum score for [`PresenceState::AtDesk`].
    pub at_desk_score: f64,
    /// Minimum score for [`PresenceState::Active`].
    pub active_score: f64,
}

impl PresencePolicy {
    /// Conservative LAN-default policy.
    #[must_use]
    pub const fn lan_default() -> Self {
        Self {
            stale_after_millis: 20_000,
            nearby_score: 0.25,
            at_desk_score: 0.55,
            active_score: 0.8,
        }
    }
}

impl Default for PresencePolicy {
    fn default() -> Self {
        Self::lan_default()
    }
}

/// Current proximity/presence snapshot for a device.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProximitySnapshot {
    /// Device id.
    pub device: DeviceId,
    /// Weighted aggregate confidence.
    pub score: f64,
    /// Derived presence state.
    pub state: PresenceState,
    /// Most recent observation time.
    pub last_seen_millis: u64,
}

/// In-memory proximity tracker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PresenceTracker {
    policy: PresencePolicy,
    observations: HashMap<DeviceId, Vec<ProximityObservation>>,
}

impl PresenceTracker {
    /// Create an empty tracker.
    #[must_use]
    pub fn new(policy: PresencePolicy) -> Self {
        Self {
            policy,
            observations: HashMap::new(),
        }
    }

    /// Record a proximity observation.
    pub fn observe(&mut self, observation: ProximityObservation) {
        let device_observations = self.observations.entry(observation.device).or_default();
        if let Some(existing) = device_observations
            .iter_mut()
            .find(|existing| existing.kind == observation.kind)
        {
            *existing = observation;
        } else {
            device_observations.push(observation);
        }
    }

    /// Drop stale observations and empty device buckets.
    pub fn prune(&mut self, now_millis: u64) {
        let stale_after = self.policy.stale_after_millis;
        self.observations.retain(|_, observations| {
            observations.retain(|observation| {
                now_millis.saturating_sub(observation.at_millis) <= stale_after
            });
            !observations.is_empty()
        });
    }

    /// Snapshot one device's current presence.
    #[must_use]
    pub fn snapshot(&self, device: DeviceId, now_millis: u64) -> Option<ProximitySnapshot> {
        let observations = self.observations.get(&device)?;
        snapshot_for(self.policy, device, observations, now_millis)
    }

    /// All fresh snapshots sorted by strongest presence first.
    #[must_use]
    pub fn ranked(&self, now_millis: u64) -> Vec<ProximitySnapshot> {
        let mut snapshots: Vec<_> = self
            .observations
            .iter()
            .filter_map(|(device, observations)| {
                snapshot_for(self.policy, *device, observations, now_millis)
            })
            .collect();
        snapshots.sort_by(|left, right| {
            right
                .score
                .partial_cmp(&left.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| right.last_seen_millis.cmp(&left.last_seen_millis))
        });
        snapshots
    }

    /// Best active device for presence-aware switching.
    #[must_use]
    pub fn best_active(&self, now_millis: u64) -> Option<DeviceId> {
        self.ranked(now_millis)
            .into_iter()
            .find(|snapshot| snapshot.state == PresenceState::Active)
            .map(|snapshot| snapshot.device)
    }
}

impl Default for PresenceTracker {
    fn default() -> Self {
        Self::new(PresencePolicy::default())
    }
}

fn snapshot_for(
    policy: PresencePolicy,
    device: DeviceId,
    observations: &[ProximityObservation],
    now_millis: u64,
) -> Option<ProximitySnapshot> {
    let mut weighted_score = 0.0;
    let mut total_weight = 0.0;
    let mut last_seen_millis = 0;

    for observation in observations {
        let age = now_millis.saturating_sub(observation.at_millis);
        if age > policy.stale_after_millis {
            continue;
        }
        let freshness = 1.0 - (age as f64 / policy.stale_after_millis.max(1) as f64);
        let weight = signal_weight(observation.kind) * freshness.max(0.05);
        weighted_score += observation.confidence * weight;
        total_weight += weight;
        last_seen_millis = last_seen_millis.max(observation.at_millis);
    }

    if total_weight <= f64::EPSILON {
        return None;
    }

    let score = (weighted_score / total_weight).clamp(0.0, 1.0);
    Some(ProximitySnapshot {
        device,
        score,
        state: state_for(policy, score),
        last_seen_millis,
    })
}

fn signal_weight(kind: ProximitySignalKind) -> f64 {
    match kind {
        ProximitySignalKind::LanHeartbeat => 0.8,
        ProximitySignalKind::BluetoothRssi => 1.2,
        ProximitySignalKind::WifiRssi => 0.9,
        ProximitySignalKind::PresenceHint => 1.5,
    }
}

fn state_for(policy: PresencePolicy, score: f64) -> PresenceState {
    if score >= policy.active_score {
        PresenceState::Active
    } else if score >= policy.at_desk_score {
        PresenceState::AtDesk
    } else if score >= policy.nearby_score {
        PresenceState::Nearby
    } else {
        PresenceState::Away
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranks_strong_presence_first() {
        let first = DeviceId::generate();
        let second = DeviceId::generate();
        let mut tracker = PresenceTracker::default();
        tracker.observe(ProximityObservation::new(
            first,
            ProximitySignalKind::LanHeartbeat,
            0.4,
            100,
        ));
        tracker.observe(ProximityObservation::new(
            second,
            ProximitySignalKind::PresenceHint,
            0.95,
            100,
        ));

        let ranked = tracker.ranked(100);
        assert_eq!(ranked[0].device, second);
        assert_eq!(tracker.best_active(100), Some(second));
    }

    #[test]
    fn stale_observations_drop_out() {
        let device = DeviceId::generate();
        let mut tracker = PresenceTracker::new(PresencePolicy {
            stale_after_millis: 10,
            ..PresencePolicy::lan_default()
        });
        tracker.observe(ProximityObservation::new(
            device,
            ProximitySignalKind::BluetoothRssi,
            1.0,
            0,
        ));
        assert!(tracker.snapshot(device, 11).is_none());
        tracker.prune(11);
        assert!(tracker.ranked(11).is_empty());
    }
}
