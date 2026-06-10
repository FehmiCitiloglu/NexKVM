//! Persistent trusted-device registry.
//!
//! Backs [`nexkvm_crypto::TrustStore`] with a JSON file so pairings survive
//! restarts. Trust entries hold only *public* key material and metadata — never
//! secrets — so a plain file (in the app's config dir) is appropriate; private
//! keys live in the OS keychain, wired up separately.
//!
//! # Persistence model
//! The in-memory map is the source of truth; every mutation best-effort writes
//! the whole map back to disk (the set of paired devices is small). Because the
//! [`nexkvm_crypto::TrustStore`] trait methods are infallible, write errors from
//! [`TrustStore::insert`]/[`remove`](TrustStore::remove) cannot be surfaced
//! there; call [`FileTrustStore::flush`] when you need to confirm a write
//! reached disk.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use nexkvm_crypto::{PublicKey, TrustEntry, TrustStore};
use thiserror::Error;

/// Errors loading or persisting the trust registry.
#[derive(Debug, Error)]
pub enum TrustStoreError {
    /// Failed to read/write the trust file.
    #[error("trust store io error: {0}")]
    Io(#[from] std::io::Error),

    /// Failed to (de)serialize trust entries.
    #[error("trust store json error: {0}")]
    Json(#[from] serde_json::Error),
}

/// A JSON-file-backed [`TrustStore`].
#[derive(Debug)]
pub struct FileTrustStore {
    path: PathBuf,
    entries: Mutex<HashMap<PublicKey, TrustEntry>>,
}

impl FileTrustStore {
    /// Load a trust store from `path`, starting empty if the file is absent.
    ///
    /// # Errors
    /// Returns [`TrustStoreError`] on I/O failure (other than not-found) or if
    /// the existing file cannot be parsed.
    pub fn load(path: impl Into<PathBuf>) -> Result<Self, TrustStoreError> {
        let path = path.into();
        let entries = match std::fs::read(&path) {
            Ok(bytes) => {
                let list: Vec<TrustEntry> = serde_json::from_slice(&bytes)?;
                list.into_iter()
                    .map(|e| (e.public_key.clone(), e))
                    .collect()
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => return Err(TrustStoreError::Io(e)),
        };
        Ok(Self {
            path,
            entries: Mutex::new(entries),
        })
    }

    /// Snapshot of all trusted entries.
    #[must_use]
    pub fn entries(&self) -> Vec<TrustEntry> {
        self.entries
            .lock()
            .expect("trust mutex poisoned")
            .values()
            .cloned()
            .collect()
    }

    /// Write the current set of entries to disk, creating parent dirs.
    ///
    /// # Errors
    /// Returns [`TrustStoreError`] on serialization or I/O failure.
    pub fn flush(&self) -> Result<(), TrustStoreError> {
        let snapshot = self.entries();
        self.write(&snapshot)
    }

    fn write(&self, entries: &[TrustEntry]) -> Result<(), TrustStoreError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(entries)?;
        std::fs::write(&self.path, json)?;
        Ok(())
    }

    fn persist_locked(&self, map: &HashMap<PublicKey, TrustEntry>) {
        let snapshot: Vec<TrustEntry> = map.values().cloned().collect();
        // Best-effort: trait methods are infallible. Use `flush` to verify.
        let _ = self.write(&snapshot);
    }
}

impl TrustStore for FileTrustStore {
    fn get(&self, key: &PublicKey) -> Option<TrustEntry> {
        self.entries
            .lock()
            .expect("trust mutex poisoned")
            .get(key)
            .cloned()
    }

    fn insert(&self, entry: TrustEntry) {
        let mut map = self.entries.lock().expect("trust mutex poisoned");
        map.insert(entry.public_key.clone(), entry);
        self.persist_locked(&map);
    }

    fn remove(&self, key: &PublicKey) {
        let mut map = self.entries.lock().expect("trust mutex poisoned");
        map.remove(key);
        self.persist_locked(&map);
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
    fn insert_persists_across_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trust.json");

        let store = FileTrustStore::load(&path).unwrap();
        let e = entry("laptop", &[1, 2, 3]);
        let key = e.public_key.clone();
        store.insert(e.clone());

        // Reload from disk in a fresh instance.
        let reloaded = FileTrustStore::load(&path).unwrap();
        assert!(reloaded.is_trusted(&key));
        assert_eq!(reloaded.get(&key), Some(e));
    }

    #[test]
    fn remove_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trust.json");

        let store = FileTrustStore::load(&path).unwrap();
        let e = entry("phone", &[9, 9]);
        let key = e.public_key.clone();
        store.insert(e);
        store.remove(&key);

        let reloaded = FileTrustStore::load(&path).unwrap();
        assert!(!reloaded.is_trusted(&key));
    }

    #[test]
    fn missing_file_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.json");
        let store = FileTrustStore::load(&path).unwrap();
        assert!(store.entries().is_empty());
    }
}
