//! Persistent trusted-device registry.
//!
//! Backs [`nexkvm_crypto::TrustStore`] with a JSON file so pairings survive
//! restarts. Trust entries hold only *public* key material and metadata — never
//! secrets — so a plain owner-only file in the app's config directory is
//! appropriate. Private identity material is stored separately by the current
//! owner-only [`crate::FileDeviceIdentityStore`] fallback; platform keychains
//! can replace that backend later.
//!
//! # Persistence model
//! The in-memory map is the source of truth; every mutation best-effort writes
//! the whole map to a same-directory temporary file and atomically replaces the
//! previous registry (the set of paired devices is small). New files are
//! owner-only on Unix. Because the [`nexkvm_crypto::TrustStore`] trait methods
//! are infallible, write errors from [`TrustStore::insert`]/
//! [`remove`](TrustStore::remove) cannot be surfaced there; call
//! [`FileTrustStore::flush`] when you need to confirm a write reached disk.
//!
//! The parent path is resolved to a stable location when the store is loaded.
//! Static symlink file targets and symlinks introduced in not-yet-created path
//! components are rejected. Portable path APIs cannot close every local
//! filesystem TOCTOU race, so the resolved config directory must not be
//! concurrently writable by an untrusted local principal.

use std::collections::HashMap;
use std::fmt;
use std::fs::{self, File};
use std::io::{ErrorKind, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::Instant;

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

use nexkvm_crypto::{
    CryptoError, DeviceIdentity, PairingSession, PublicKey, TrustEntry, TrustStore,
};
use thiserror::Error;

const MAX_TRUST_STORE_BYTES: u64 = 1024 * 1024;

/// Errors loading or persisting the trust registry.
#[derive(Debug, Error)]
pub enum TrustStoreError {
    /// Failed to read/write the trust file.
    #[error("trust store io error: {0}")]
    Io(#[from] std::io::Error),

    /// Failed to (de)serialize trust entries.
    #[error("trust store json error: {0}")]
    Json(#[from] serde_json::Error),

    /// Pairing confirmation failed before the peer could be persisted.
    #[error(transparent)]
    Crypto(#[from] CryptoError),

    /// The trust path is a symlink, non-regular target, or unsafe directory
    /// chain.
    #[error("unsafe trust store path")]
    UnsafePath,
}

/// A JSON-file-backed [`TrustStore`].
pub struct FileTrustStore {
    path: PathBuf,
    entries: Mutex<HashMap<PublicKey, TrustEntry>>,
}

impl fmt::Debug for FileTrustStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Paths, device names, and pinned keys are operationally sensitive even
        // though the trust store never contains private key material.
        f.debug_struct("FileTrustStore").finish_non_exhaustive()
    }
}

impl FileTrustStore {
    /// Load a trust store from `path`, starting empty if the file is absent.
    ///
    /// # Errors
    /// Returns [`TrustStoreError`] on I/O failure (other than not-found) or if
    /// the existing file cannot be parsed.
    pub fn load(path: impl Into<PathBuf>) -> Result<Self, TrustStoreError> {
        let path = stabilize_store_path(path.into())?;
        let entries = match read_trust_file(&path) {
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
        self.lock_entries().values().cloned().collect()
    }

    /// Write the current set of entries to disk, creating parent dirs.
    ///
    /// # Errors
    /// Returns [`TrustStoreError`] on serialization or I/O failure.
    pub fn flush(&self) -> Result<(), TrustStoreError> {
        let map = self.lock_entries();
        self.write_locked(&map)
    }

    /// Verify a user-confirmed pairing code, pin the peer, and confirm the
    /// updated trust store reached disk.
    ///
    /// This is the runtime write path after the network/crypto pairing flow has
    /// displayed a confirmation code and the user approved it. The peer is only
    /// inserted when `entered_code` matches the expected code for `session`.
    ///
    /// # Errors
    /// Returns [`TrustStoreError::Crypto`] if the code is wrong or the pairing
    /// token is expired/used. Returns [`TrustStoreError::Io`] or
    /// [`TrustStoreError::Json`] if the file-backed store cannot be flushed.
    pub fn confirm_pairing(
        &self,
        session: &mut PairingSession,
        entered_code: &str,
        peer: &DeviceIdentity,
        paired_at: u64,
        now: Instant,
    ) -> Result<TrustEntry, TrustStoreError> {
        let entry = session.verify_and_accept(entered_code, peer, paired_at, now, self)?;
        self.flush()?;
        Ok(entry)
    }

    fn write(&self, entries: &[TrustEntry]) -> Result<(), TrustStoreError> {
        let json = serde_json::to_vec_pretty(entries)?;
        if json.len() as u64 > MAX_TRUST_STORE_BYTES {
            return Err(TrustStoreError::Io(std::io::Error::new(
                ErrorKind::InvalidData,
                "trust store exceeds configured size limit",
            )));
        }
        let parent = self.path.parent().ok_or(TrustStoreError::UnsafePath)?;
        ensure_secure_directory_chain(parent)?;
        validate_trust_target(&self.path)?;

        let mut temporary = tempfile::Builder::new()
            .prefix(".nexkvm-trust-")
            .suffix(".tmp")
            .tempfile_in(parent)?;
        harden_temporary_permissions(temporary.as_file())?;
        temporary.write_all(&json)?;
        temporary.flush()?;
        temporary.as_file().sync_all()?;

        // Revalidate immediately before publication. Atomic replacement does
        // not follow a regular target and never exposes a partially-written
        // JSON document.
        validate_trust_target(&self.path)?;
        let persisted = temporary
            .persist(&self.path)
            .map_err(|error| TrustStoreError::Io(error.error))?;
        persisted.sync_all()?;
        sync_parent_directory(parent)?;
        Ok(())
    }

    fn persist_locked(&self, map: &HashMap<PublicKey, TrustEntry>) {
        // Best-effort: trait methods are infallible. Use `flush` to verify.
        let _ = self.write_locked(map);
    }

    fn write_locked(&self, map: &HashMap<PublicKey, TrustEntry>) -> Result<(), TrustStoreError> {
        let snapshot = ordered_snapshot(map);
        self.write(&snapshot)
    }

    fn lock_entries(&self) -> MutexGuard<'_, HashMap<PublicKey, TrustEntry>> {
        match self.entries.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                // TrustStore's interface is infallible. HashMap remains memory
                // safe across unwinding, so retain the last in-memory state and
                // clear poison instead of turning every future auth lookup into
                // a process panic.
                self.entries.clear_poison();
                poisoned.into_inner()
            }
        }
    }
}

fn ordered_snapshot(map: &HashMap<PublicKey, TrustEntry>) -> Vec<TrustEntry> {
    let mut snapshot: Vec<_> = map.values().cloned().collect();
    snapshot.sort_unstable_by(|left, right| {
        left.public_key.as_bytes().cmp(right.public_key.as_bytes())
    });
    snapshot
}

impl TrustStore for FileTrustStore {
    fn get(&self, key: &PublicKey) -> Option<TrustEntry> {
        self.lock_entries().get(key).cloned()
    }

    fn insert(&self, entry: TrustEntry) {
        let mut map = self.lock_entries();
        map.insert(entry.public_key.clone(), entry);
        self.persist_locked(&map);
    }

    fn remove(&self, key: &PublicKey) {
        let mut map = self.lock_entries();
        map.remove(key);
        self.persist_locked(&map);
    }
}

fn stabilize_store_path(path: PathBuf) -> Result<PathBuf, TrustStoreError> {
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(TrustStoreError::UnsafePath);
    }
    let absolute = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };
    let file_name = absolute
        .file_name()
        .ok_or(TrustStoreError::UnsafePath)?
        .to_os_string();
    let parent = absolute.parent().ok_or(TrustStoreError::UnsafePath)?;

    let mut existing = parent.to_path_buf();
    let mut missing = Vec::new();
    loop {
        match fs::symlink_metadata(&existing) {
            Ok(_) => break,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                let component = existing
                    .file_name()
                    .ok_or(TrustStoreError::UnsafePath)?
                    .to_os_string();
                missing.push(component);
                if !existing.pop() {
                    return Err(TrustStoreError::UnsafePath);
                }
            }
            Err(error) => return Err(error.into()),
        }
    }

    let mut stable_parent = fs::canonicalize(existing)?;
    if !fs::metadata(&stable_parent)?.is_dir() {
        return Err(TrustStoreError::UnsafePath);
    }
    for component in missing.iter().rev() {
        stable_parent.push(component);
    }
    Ok(stable_parent.join(file_name))
}

fn read_trust_file(path: &Path) -> Result<Vec<u8>, std::io::Error> {
    crate::bounded_file::read_owner_only_bounded_regular_file(path, MAX_TRUST_STORE_BYTES)
}

fn validate_trust_target(path: &Path) -> Result<(), TrustStoreError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(TrustStoreError::UnsafePath)
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn ensure_secure_directory_chain(path: &Path) -> Result<(), TrustStoreError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => current.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => return Err(TrustStoreError::UnsafePath),
            Component::Normal(name) => {
                current.push(name);
                match fs::symlink_metadata(&current) {
                    Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                        return Err(TrustStoreError::UnsafePath);
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == ErrorKind::NotFound => {
                        create_secure_directory(&current)?;
                        let metadata = fs::symlink_metadata(&current)?;
                        if metadata.file_type().is_symlink() || !metadata.is_dir() {
                            return Err(TrustStoreError::UnsafePath);
                        }
                    }
                    Err(error) => return Err(error.into()),
                }
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn create_secure_directory(path: &Path) -> Result<(), std::io::Error> {
    fs::DirBuilder::new().mode(0o700).create(path)
}

#[cfg(not(unix))]
fn create_secure_directory(path: &Path) -> Result<(), std::io::Error> {
    fs::create_dir(path)
}

#[cfg(unix)]
fn harden_temporary_permissions(file: &File) -> Result<(), std::io::Error> {
    file.set_permissions(fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn harden_temporary_permissions(_file: &File) -> Result<(), std::io::Error> {
    Ok(())
}

#[cfg(unix)]
fn sync_parent_directory(parent: &Path) -> Result<(), std::io::Error> {
    File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_directory(_parent: &Path) -> Result<(), std::io::Error> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexkvm_crypto::{DEFAULT_PAIRING_TTL, PairingState};
    use std::panic::{AssertUnwindSafe, catch_unwind};

    #[cfg(unix)]
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

    fn entry(name: &str, key: &[u8]) -> TrustEntry {
        TrustEntry {
            display_name: name.into(),
            public_key: PublicKey(key.to_vec()),
            paired_at: 1_700_000_000,
        }
    }

    fn identity(name: &str, key: &[u8]) -> DeviceIdentity {
        DeviceIdentity {
            display_name: name.into(),
            public_key: PublicKey(key.to_vec()),
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

    #[test]
    fn confirmed_pairing_pins_peer_and_flushes_to_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trust.json");
        let store = FileTrustStore::load(&path).unwrap();
        let now = Instant::now();
        let local = identity("desk-macos", &[1, 2, 3, 4]);
        let peer = identity("laptop-linux", &[9, 8, 7, 6]);
        let mut session =
            PairingSession::initiate(local, "127.0.0.1:4101", [7u8; 32], now, DEFAULT_PAIRING_TTL);
        let code = session.confirmation_code(&peer.public_key, now).unwrap();

        let entry = store
            .confirm_pairing(&mut session, code.as_str(), &peer, 1_700_000_000, now)
            .unwrap();

        assert_eq!(entry.display_name, "laptop-linux");
        assert_eq!(session.state(), PairingState::Paired);
        let reloaded = FileTrustStore::load(&path).unwrap();
        assert!(reloaded.is_trusted(&peer.public_key));
    }

    #[test]
    fn wrong_confirmation_code_does_not_write_peer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trust.json");
        let store = FileTrustStore::load(&path).unwrap();
        let now = Instant::now();
        let local = identity("desk-macos", &[1, 2, 3, 4]);
        let peer = identity("laptop-linux", &[9, 8, 7, 6]);
        let mut session =
            PairingSession::initiate(local, "127.0.0.1:4101", [7u8; 32], now, DEFAULT_PAIRING_TTL);

        let err = store
            .confirm_pairing(&mut session, "000000", &peer, 1_700_000_000, now)
            .unwrap_err();

        assert!(matches!(
            err,
            TrustStoreError::Crypto(CryptoError::PairingMismatch)
        ));
        assert_eq!(session.state(), PairingState::Failed);
        assert!(store.entries().is_empty());
        assert!(!path.exists());
    }

    #[test]
    fn poisoned_mutex_is_recovered_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trust.json");
        let store = FileTrustStore::load(&path).unwrap();

        let poison_result = catch_unwind(AssertUnwindSafe(|| {
            let _guard = store.entries.lock().unwrap();
            panic!("poison trust-store mutex");
        }));
        assert!(poison_result.is_err());

        let trusted = entry("recovered", &[4, 2]);
        let key = trusted.public_key.clone();
        store.insert(trusted.clone());
        assert_eq!(store.get(&key), Some(trusted));
        store.remove(&key);
        assert!(store.entries().is_empty());
    }

    #[test]
    fn debug_output_redacts_path_and_trust_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("private-trust.json");
        let store = FileTrustStore::load(&path).unwrap();
        store.insert(entry("private-device-name", &[42, 43, 44]));

        let rendered = format!("{store:?}");
        assert!(!rendered.contains(path.to_string_lossy().as_ref()));
        assert!(!rendered.contains("private-device-name"));
        assert!(!rendered.contains("42, 43, 44"));
    }

    #[test]
    fn persisted_snapshot_is_sorted_by_public_key() {
        let entries = [
            entry("third", &[3]),
            entry("first", &[1]),
            entry("second", &[2]),
        ];
        let map: HashMap<_, _> = entries
            .into_iter()
            .map(|entry| (entry.public_key.clone(), entry))
            .collect();

        let snapshot = ordered_snapshot(&map);
        let keys: Vec<_> = snapshot
            .iter()
            .map(|entry| entry.public_key.as_bytes())
            .collect();
        assert_eq!(keys, vec![&[1][..], &[2][..], &[3][..]]);
    }

    #[cfg(unix)]
    #[test]
    fn persistence_atomically_replaces_with_owner_only_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trust.json");
        let store = FileTrustStore::load(&path).unwrap();

        store.insert(entry("first", &[1]));
        let first_metadata = std::fs::metadata(&path).unwrap();
        store.insert(entry("second", &[2]));
        let second_metadata = std::fs::metadata(&path).unwrap();

        assert_ne!(first_metadata.ino(), second_metadata.ino());
        assert_eq!(second_metadata.permissions().mode() & 0o777, 0o600);
        let persisted: Vec<TrustEntry> =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(persisted.len(), 2);
        assert!(std::fs::read_dir(dir.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        }));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_trust_file_is_rejected_without_reading_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("unrelated.json");
        let path = dir.path().join("trust.json");
        std::fs::write(&target, b"[]").unwrap();
        symlink(&target, &path).unwrap();

        assert!(FileTrustStore::load(&path).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"[]");
    }

    #[cfg(unix)]
    #[test]
    fn resolved_parent_cannot_be_retargeted_after_load() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("original");
        let redirected = dir.path().join("redirected");
        let alias = dir.path().join("alias");
        std::fs::create_dir(&original).unwrap();
        std::fs::create_dir(&redirected).unwrap();
        symlink(&original, &alias).unwrap();

        let store = FileTrustStore::load(alias.join("trust.json")).unwrap();
        std::fs::remove_file(&alias).unwrap();
        symlink(&redirected, &alias).unwrap();
        store.insert(entry("laptop", &[1, 2, 3]));

        assert!(original.join("trust.json").is_file());
        assert!(!redirected.join("trust.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn late_symlink_in_missing_parent_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let redirected = dir.path().join("redirected");
        let missing_parent = dir.path().join("future");
        std::fs::create_dir(&redirected).unwrap();
        let store = FileTrustStore::load(missing_parent.join("trust.json")).unwrap();
        symlink(&redirected, &missing_parent).unwrap();

        store.insert(entry("laptop", &[1, 2, 3]));
        assert!(store.flush().is_err());
        assert!(!redirected.join("trust.json").exists());
    }

    #[test]
    fn oversized_trust_file_is_rejected_before_json_decode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trust.json");
        File::create(&path)
            .unwrap()
            .set_len(MAX_TRUST_STORE_BYTES + 1)
            .unwrap();

        let error = FileTrustStore::load(path).expect_err("oversized trust file must fail");

        assert!(matches!(
            error,
            TrustStoreError::Io(error) if error.kind() == ErrorKind::InvalidData
        ));
    }

    #[test]
    fn oversized_trust_snapshot_does_not_replace_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trust.json");
        let store = FileTrustStore::load(&path).unwrap();
        store.insert(entry("existing", &[1]));
        let before = fs::read(&path).unwrap();
        let oversized_name = "x".repeat(MAX_TRUST_STORE_BYTES as usize + 1);

        let error = store
            .write(&[entry(&oversized_name, &[2])])
            .expect_err("oversized trust snapshot must fail");

        assert!(matches!(
            error,
            TrustStoreError::Io(error) if error.kind() == ErrorKind::InvalidData
        ));
        assert_eq!(fs::read(path).unwrap(), before);
    }
}
