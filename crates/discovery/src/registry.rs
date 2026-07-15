//! In-memory registry of currently-visible peers, with time-to-live expiry.
//!
//! Discovery backends (`udp`, `mdns`) funnel observations here; consumers read a
//! snapshot of *live* peers. Entries expire after a TTL so a device that goes
//! offline (and stops announcing) disappears without an explicit "goodbye".
//!
//! The registry is sans-IO and clock-injected: callers pass the current
//! [`Instant`], keeping it deterministic and unit-testable.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use nexkvm_core::identity::DeviceId;

use crate::DiscoveredDevice;

/// Default time a peer remains "live" after its last announcement.
pub const DEFAULT_TTL: Duration = Duration::from_secs(15);
/// Maximum distinct peer identities retained from untrusted LAN discovery.
pub const MAX_DISCOVERED_PEERS: usize = 1_024;

#[derive(Debug)]
struct Entry {
    device: DiscoveredDevice,
    last_seen: Instant,
}

/// Thread-safe, TTL-pruned set of discovered peers.
#[derive(Debug)]
pub struct DiscoveryRegistry {
    ttl: Duration,
    capacity: usize,
    inner: Mutex<HashMap<DeviceId, Entry>>,
}

impl DiscoveryRegistry {
    /// Create a registry whose entries expire `ttl` after they were last seen.
    #[must_use]
    pub fn new(ttl: Duration) -> Self {
        Self::with_capacity(ttl, MAX_DISCOVERED_PEERS)
    }

    fn with_capacity(ttl: Duration, capacity: usize) -> Self {
        Self {
            ttl,
            capacity: capacity.max(1),
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Record (or refresh) an observation of `device` seen at `now`.
    ///
    /// A later observation for the same [`DeviceId`] overwrites the stored
    /// address/metadata, so a roaming device's new address replaces the old.
    pub fn observe(&self, device: DiscoveredDevice, now: Instant) {
        let id = device.info.id;
        // No `.await` is held while locked.
        let mut map = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        prune_expired(&mut map, now, self.ttl);
        if !map.contains_key(&id)
            && map.len() >= self.capacity
            && let Some(oldest) = map
                .iter()
                .min_by_key(|(_, entry)| entry.last_seen)
                .map(|(id, _)| *id)
        {
            map.remove(&oldest);
        }
        map.insert(
            id,
            Entry {
                device,
                last_seen: now,
            },
        );
    }

    /// Drop entries whose TTL has elapsed relative to `now`.
    pub fn prune(&self, now: Instant) {
        let mut map = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        prune_expired(&mut map, now, self.ttl);
    }

    /// Snapshot of peers still live at `now`, pruning expired storage first.
    #[must_use]
    pub fn live(&self, now: Instant) -> Vec<DiscoveredDevice> {
        let mut map = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        prune_expired(&mut map, now, self.ttl);
        map.values().map(|e| e.device.clone()).collect()
    }

    /// Number of tracked entries (including any not yet pruned).
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// Whether the registry currently tracks no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn prune_expired(map: &mut HashMap<DeviceId, Entry>, now: Instant, ttl: Duration) {
    map.retain(|_, entry| now.saturating_duration_since(entry.last_seen) < ttl);
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexkvm_core::identity::{DeviceInfo, OsKind};
    use std::net::SocketAddr;

    fn device(name: &str) -> DiscoveredDevice {
        DiscoveredDevice {
            info: DeviceInfo::new(name, OsKind::Linux),
            addr: "127.0.0.1:47654".parse::<SocketAddr>().unwrap(),
            fingerprint: None,
        }
    }

    #[test]
    fn observe_then_live_returns_device() {
        let reg = DiscoveryRegistry::new(Duration::from_secs(10));
        let now = Instant::now();
        reg.observe(device("a"), now);
        assert_eq!(reg.live(now).len(), 1);
    }

    #[test]
    fn expired_entries_are_not_live_and_prune_removes_them() {
        let ttl = Duration::from_secs(10);
        let reg = DiscoveryRegistry::new(ttl);
        let t0 = Instant::now();
        reg.observe(device("a"), t0);

        let later = t0 + Duration::from_secs(11);
        assert!(reg.live(later).is_empty());
        // Live snapshots prune expired storage; explicit prune is idempotent.
        assert!(reg.is_empty());
        reg.prune(later);
        assert!(reg.is_empty());
    }

    #[test]
    fn re_observation_refreshes_last_seen() {
        let ttl = Duration::from_secs(10);
        let reg = DiscoveryRegistry::new(ttl);
        let t0 = Instant::now();
        let dev = device("a");
        reg.observe(dev.clone(), t0);
        // Refresh just before expiry.
        reg.observe(dev, t0 + Duration::from_secs(9));
        assert_eq!(reg.live(t0 + Duration::from_secs(15)).len(), 1);
        assert_eq!(reg.len(), 1, "same id should not duplicate");
    }

    #[test]
    fn live_snapshot_prunes_expired_storage() {
        let reg = DiscoveryRegistry::new(Duration::from_secs(1));
        let t0 = Instant::now();
        reg.observe(device("expired"), t0);

        assert!(reg.live(t0 + Duration::from_secs(2)).is_empty());
        assert!(
            reg.is_empty(),
            "expired entries must not accumulate in memory"
        );
    }

    #[test]
    fn registry_is_hard_capped_and_keeps_recent_observations() {
        let reg = DiscoveryRegistry::with_capacity(Duration::from_secs(60), 2);
        let t0 = Instant::now();
        let oldest = device("oldest");
        let oldest_id = oldest.info.id;
        reg.observe(oldest, t0);
        reg.observe(device("middle"), t0 + Duration::from_millis(1));
        let newest = device("newest");
        let newest_id = newest.info.id;
        reg.observe(newest, t0 + Duration::from_millis(2));

        let live = reg.live(t0 + Duration::from_millis(2));
        assert_eq!(reg.len(), 2);
        assert!(live.iter().any(|entry| entry.info.id == newest_id));
        assert!(!live.iter().any(|entry| entry.info.id == oldest_id));
    }
}
