//! Persistent local cryptographic identity seed.

use std::path::PathBuf;
use std::sync::Mutex;

use nexkvm_crypto::DeviceKeypair;
use serde::{Deserialize, Serialize};
use thiserror::Error;

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
}

/// File-backed private identity seed store.
///
/// This is the cross-platform fallback until platform keychain backends are
/// wired in. It stores only the Ed25519 seed bytes and derives the public key at
/// runtime.
#[derive(Debug)]
pub struct FileDeviceIdentityStore {
    path: PathBuf,
    seed: Mutex<Option<[u8; 32]>>,
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
        if let Some(seed) = *self.seed.lock().expect("identity mutex poisoned") {
            return Ok(DeviceKeypair::from_seed(seed));
        }

        let seed = match std::fs::read(&self.path) {
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
        *self.seed.lock().expect("identity mutex poisoned") = Some(seed);
        Ok(DeviceKeypair::from_seed(seed))
    }

    fn write(&self, seed: [u8; 32]) -> Result<(), IdentityStoreError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(&IdentityRecord { seed })?;
        std::fs::write(&self.path, json)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct IdentityRecord {
    seed: [u8; 32],
}

fn generate_seed() -> Result<[u8; 32], IdentityStoreError> {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|error| IdentityStoreError::Random(error.to_string()))?;
    Ok(seed)
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
}
