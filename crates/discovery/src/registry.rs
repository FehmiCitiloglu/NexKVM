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

#[derive(Debug)]
struct Entry {
    device: DiscoveredDevice,
    last_seen: Instant,
}

/// Thread-safe, TTL-pruned set of discovered peers.
#[derive(Debug)]
pub struct DiscoveryRegistry {
    ttl: Duration,
    inner: Mutex<HashMap<DeviceId, Entry>>,
}

impl DiscoveryRegistry {
    /// Create a registry whose entries expire `ttl` after they were last seen.
    #[must_use]
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
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
        let mut map = self.inner.lock().expect("registry mutex poisoned");
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
        let mut map = self.inner.lock().expect("registry mutex poisoned");
        map.retain(|_, e| now.duration_since(e.last_seen) < self.ttl);
    }

    /// Snapshot of peers still live at `now` (does not mutate the registry).
    #[must_use]
    pub fn live(&self, now: Instant) -> Vec<DiscoveredDevice> {
        let map = self.inner.lock().expect("registry mutex poisoned");
        map.values()
            .filter(|e| now.duration_since(e.last_seen) < self.ttl)
            .map(|e| e.device.clone())
            .collect()
    }

    /// Number of tracked entries (including any not yet pruned).
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.lock().expect("registry mutex poisoned").len()
    }

    /// Whether the registry currently tracks no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
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
        // Still stored until pruned.
        assert_eq!(reg.len(), 1);
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
}
