//! Persistent local cryptographic identity seed.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use nexkvm_crypto::DeviceKeypair;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use zeroize::{Zeroize, ZeroizeOnDrop};

const MAX_IDENTITY_FILE_BYTES: u64 = 4 * 1024;

/// Errors loading or persisting the local device identity.
#[derive(Debug, Error)]
pub enum IdentityStoreError {
    /// Failed to read/write the identity file.
    #[error("identity store io error: {0}")]
    Io(#[from] std::io::Error),

    /// Failed to serialize/deserialize the identity file.
    #[error("identity store json error: {0}")]
    Json(#[from] serde_json::Error),

    /// Failed to generate a private identity seed.
    #[error("identity random generation error: {0}")]
    Random(String),

    /// The cached identity mutex was poisoned.
    #[error("identity store lock poisoned")]
    LockPoisoned,

    /// The configured identity path is unsafe or not a regular file.
    #[error("invalid identity store path: {0}")]
    InvalidPath(String),
}

/// File-backed private identity seed store.
///
/// This is the cross-platform fallback until platform keychain backends are
/// wired in. It stores only the Ed25519 seed bytes and derives the public key at
/// runtime.
pub struct FileDeviceIdentityStore {
    path: PathBuf,
    seed: Mutex<Option<SecretSeed>>,
}

impl std::fmt::Debug for FileDeviceIdentityStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FileDeviceIdentityStore")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl FileDeviceIdentityStore {
    /// Load a store from `path`; the identity is created lazily if absent.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            seed: Mutex::new(None),
        }
    }

    /// Load or create the local device keypair.
    ///
    /// # Errors
    /// Returns [`IdentityStoreError`] on read/write/JSON failure.
    pub fn load_or_create(&self, _device_name: &str) -> Result<DeviceKeypair, IdentityStoreError> {
        let mut cached = self
            .seed
            .lock()
            .map_err(|_| IdentityStoreError::LockPoisoned)?;
        if let Some(seed) = cached.as_ref() {
            return Ok(DeviceKeypair::from_seed(seed.0));
        }

        let seed = match read_regular_file(&self.path) {
            Ok(bytes) => {
                let record: IdentityRecord = serde_json::from_slice(&bytes)?;
                record.seed
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let seed = generate_seed()?;
                self.write(seed)?;
                seed
            }
            Err(error) => return Err(IdentityStoreError::Io(error)),
        };
        *cached = Some(SecretSeed(seed));
        Ok(DeviceKeypair::from_seed(seed))
    }

    fn write(&self, seed: [u8; 32]) -> Result<(), IdentityStoreError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        reject_unsafe_existing_path(&self.path)?;
        let json = serde_json::to_vec(&IdentityRecord { seed })?;
        atomic_write_owner_only(&self.path, &json)
    }
}

#[derive(Zeroize, ZeroizeOnDrop)]
struct SecretSeed([u8; 32]);

#[derive(Debug, Serialize, Deserialize)]
struct IdentityRecord {
    seed: [u8; 32],
}

fn generate_seed() -> Result<[u8; 32], IdentityStoreError> {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|error| IdentityStoreError::Random(error.to_string()))?;
    Ok(seed)
}

fn read_regular_file(path: &Path) -> Result<Vec<u8>, std::io::Error> {
    crate::bounded_file::read_owner_only_bounded_regular_file(path, MAX_IDENTITY_FILE_BYTES)
}

fn reject_unsafe_existing_path(path: &Path) -> Result<(), IdentityStoreError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(IdentityStoreError::InvalidPath(path.display().to_string()))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn atomic_write_owner_only(path: &Path, contents: &[u8]) -> Result<(), IdentityStoreError> {
    let mut random = [0u8; 8];
    getrandom::fill(&mut random).map_err(|error| IdentityStoreError::Random(error.to_string()))?;
    let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    let temp_path = path.with_extension(format!("tmp-{suffix}"));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp_path)?;
        file.write_all(contents)?;
        file.sync_all()?;
        std::fs::rename(&temp_path, path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        if let Some(parent) = path.parent() {
            File::open(parent)?.sync_all()?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_or_create_persists_public_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.json");

        let first = FileDeviceIdentityStore::new(&path)
            .load_or_create("studio-mac")
            .unwrap()
            .public_key();
        let second = FileDeviceIdentityStore::new(&path)
            .load_or_create("studio-mac")
            .unwrap()
            .public_key();

        assert_eq!(first, second);
    }

    #[test]
    fn debug_output_never_contains_cached_seed() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileDeviceIdentityStore::new(dir.path().join("identity.json"));
        store.load_or_create("studio-mac").unwrap();

        let debug = format!("{store:?}");
        assert!(debug.contains("identity.json"));
        assert!(!debug.contains("seed"));
    }

    #[cfg(unix)]
    #[test]
    fn identity_store_rejects_symlink_path_without_touching_target() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, b"unchanged").unwrap();
        let path = dir.path().join("identity.json");
        symlink(&target, &path).unwrap();

        assert!(
            FileDeviceIdentityStore::new(path)
                .load_or_create("studio-mac")
                .is_err()
        );
        assert_eq!(std::fs::read(target).unwrap(), b"unchanged");
    }

    #[test]
    fn identity_store_rejects_oversized_files_before_json_decode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.json");
        std::fs::write(&path, vec![b'x'; MAX_IDENTITY_FILE_BYTES as usize + 1]).unwrap();

        let error = FileDeviceIdentityStore::new(path)
            .load_or_create("studio-mac")
            .expect_err("oversized identity must be rejected");

        assert!(
            matches!(error, IdentityStoreError::Io(error) if error.kind() == std::io::ErrorKind::InvalidData)
        );
    }
}
