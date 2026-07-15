//! Trust store: the set of devices this device has paired with.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

use crate::identity::PublicKey;

/// A pinned, trusted peer device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustEntry {
    /// Display name captured at pairing time.
    pub display_name: String,
    /// The pinned public key. A session is only accepted if the peer proves
    /// possession of the matching private key.
    pub public_key: PublicKey,
    /// Unix timestamp (seconds) when the device was paired.
    pub paired_at: u64,
}

/// Persistence boundary for trusted devices.
///
/// Backed by the `storage` crate in production. Kept as a trait so pairing
/// logic and tests can swap an in-memory implementation.
pub trait TrustStore: Send + Sync {
    /// Look up a trusted device by its public key.
    fn get(&self, key: &PublicKey) -> Option<TrustEntry>;

    /// Pin a newly paired device.
    fn insert(&self, entry: TrustEntry);

    /// Remove (revoke) a trusted device.
    fn remove(&self, key: &PublicKey);

    /// Whether the key is currently trusted.
    fn is_trusted(&self, key: &PublicKey) -> bool {
        self.get(key).is_some()
    }
}

/// A simple, thread-safe in-memory [`TrustStore`].
///
/// Suitable as a runtime default before any on-disk persistence is configured,
/// and as a test double. Production deployments back trust with the `storage`
/// crate so pairings survive restarts.
#[derive(Debug, Default)]
pub struct InMemoryTrustStore {
    entries: Mutex<HashMap<PublicKey, TrustEntry>>,
}

impl InMemoryTrustStore {
    /// Create an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed a store from existing entries (e.g. loaded from disk).
    #[must_use]
    pub fn from_entries(entries: impl IntoIterator<Item = TrustEntry>) -> Self {
        let map = entries
            .into_iter()
            .map(|e| (e.public_key.clone(), e))
            .collect();
        Self {
            entries: Mutex::new(map),
        }
    }

    /// Snapshot of all trusted entries.
    #[must_use]
    pub fn entries(&self) -> Vec<TrustEntry> {
        self.lock_entries().values().cloned().collect()
    }

    fn lock_entries(&self) -> MutexGuard<'_, HashMap<PublicKey, TrustEntry>> {
        match self.entries.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                // HashMap preserves its memory-safety invariants across unwind.
                // TrustStore's infallible API must remain usable for revocation.
                self.entries.clear_poison();
                poisoned.into_inner()
            }
        }
    }
}

impl TrustStore for InMemoryTrustStore {
    fn get(&self, key: &PublicKey) -> Option<TrustEntry> {
        self.lock_entries().get(key).cloned()
    }

    fn insert(&self, entry: TrustEntry) {
        self.lock_entries().insert(entry.public_key.clone(), entry);
    }

    fn remove(&self, key: &PublicKey) {
        self.lock_entries().remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, key: &[u8]) -> TrustEntry {
        TrustEntry {
            display_name: name.into(),
            public_key: PublicKey(key.to_vec()),
            paired_at: 1_700_000_000,
        }
    }

    #[test]
    fn insert_then_trusted() {
        let store = InMemoryTrustStore::new();
        let e = entry("laptop", &[1, 2, 3]);
        let key = e.public_key.clone();
        assert!(!store.is_trusted(&key));
        store.insert(e.clone());
        assert!(store.is_trusted(&key));
        assert_eq!(store.get(&key), Some(e));
    }

    #[test]
    fn remove_revokes_trust() {
        let store = InMemoryTrustStore::new();
        let e = entry("phone", &[9, 9]);
        let key = e.public_key.clone();
        store.insert(e);
        store.remove(&key);
        assert!(!store.is_trusted(&key));
    }

    #[test]
    fn from_entries_seeds_store() {
        let store = InMemoryTrustStore::from_entries([entry("a", &[1]), entry("b", &[2])]);
        assert_eq!(store.entries().len(), 2);
    }

    #[test]
    fn poisoned_mutex_does_not_panic_or_disable_trust_store_operations() {
        let store = InMemoryTrustStore::from_entries([entry("existing", &[1])]);
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = store.entries.lock().expect("fixture acquires healthy lock");
            panic!("poison trust fixture");
        }));
        assert!(poisoned.is_err());

        let added = entry("added", &[2]);
        let added_key = added.public_key.clone();
        store.insert(added.clone());

        assert_eq!(store.get(&added_key), Some(added));
        assert_eq!(store.entries().len(), 2);
        store.remove(&added_key);
        assert!(!store.is_trusted(&added_key));
    }
}
