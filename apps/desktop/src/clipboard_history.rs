//! Non-blocking bridge to the encrypted clipboard history archive.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use nexkvm_clipboard::ClipboardSnapshot;
use nexkvm_clipboard::{Clipboard, ContentFingerprint};
use nexkvm_core::DeviceId;
use nexkvm_storage::{
    ClipboardConfig, ClipboardHistoryArchive, ClipboardHistoryArchiveConfig,
    ClipboardHistoryStoreError,
};

const MAX_ARCHIVE_BYTES: usize = 128 * 1024 * 1024;

/// Cloneable recorder shared by local polling and peer receive tasks.
#[derive(Debug, Clone)]
pub struct ClipboardHistoryRecorder {
    path: PathBuf,
    config: ClipboardHistoryArchiveConfig,
    process_lock: Arc<Mutex<()>>,
}

impl ClipboardHistoryRecorder {
    /// Open the encrypted archive when history is enabled.
    ///
    /// # Errors
    /// Returns an archive error for invalid limits or inaccessible/corrupt data.
    pub fn open(
        config_path: &Path,
        config: &ClipboardConfig,
    ) -> Result<Option<Self>, ClipboardHistoryStoreError> {
        if !config.history_enabled {
            return Ok(None);
        }
        let path = archive_path(config_path);
        let archive_config = archive_config(config);
        // Validate/decrypt now so daemon startup still reports a corrupt archive
        // immediately. The transaction lock is released before returning.
        ClipboardHistoryArchive::open_exclusive(&path, archive_config)?;
        Ok(Some(Self {
            path,
            config: archive_config,
            process_lock: Arc::new(Mutex::new(())),
        }))
    }

    /// Record and atomically persist an observed selection without blocking the
    /// async runtime on filesystem encryption or `fsync`.
    ///
    /// # Errors
    /// Returns an archive error or a task-join error converted to I/O.
    pub async fn record(
        &self,
        snapshot: ClipboardSnapshot,
        origin: DeviceId,
        at_millis: u64,
        preserve_existing_origin: bool,
    ) -> Result<bool, ClipboardHistoryStoreError> {
        let path = self.path.clone();
        let config = self.config;
        let process_lock = Arc::clone(&self.process_lock);
        tokio::task::spawn_blocking(move || {
            let _process_guard = process_lock.lock().map_err(|_| {
                ClipboardHistoryStoreError::Io(std::io::Error::other(
                    "clipboard history lock poisoned",
                ))
            })?;
            // Acquire the inter-process lock before loading, then retain it
            // through persistence. This makes each record a transaction over
            // the latest disk state rather than a daemon-lifetime snapshot.
            let mut archive = ClipboardHistoryArchive::open_exclusive(path, config)?;
            let origin = if preserve_existing_origin {
                archive
                    .entries()
                    .find(|entry| entry.fingerprint() == snapshot.fingerprint())
                    .map_or(origin, |entry| entry.origin)
            } else {
                origin
            };
            let retained = archive.record(snapshot, origin, at_millis);
            if retained {
                archive.persist()?;
            }
            Ok(retained)
        })
        .await
        .map_err(|error| {
            ClipboardHistoryStoreError::Io(std::io::Error::other(format!(
                "clipboard history worker failed: {error}"
            )))
        })?
    }
}

/// Clear every unpinned entry as one locked read-modify-write transaction.
///
/// # Errors
/// Returns an archive error if locking, reloading, or persistence fails.
pub fn clear_unpinned(
    config_path: &Path,
    config: &ClipboardConfig,
) -> Result<(), ClipboardHistoryStoreError> {
    let mut archive =
        ClipboardHistoryArchive::open_exclusive(archive_path(config_path), archive_config(config))?;
    archive.clear_unpinned();
    archive.persist()
}

/// Convert user settings to hard-capped archive limits.
#[must_use]
pub fn archive_config(config: &ClipboardConfig) -> ClipboardHistoryArchiveConfig {
    let max_archive_bytes = config
        .history_capacity
        .saturating_mul(config.history_max_entry_bytes.saturating_add(64))
        .saturating_add(4)
        .min(MAX_ARCHIVE_BYTES);
    ClipboardHistoryArchiveConfig {
        capacity: config.history_capacity,
        max_entry_bytes: config.history_max_entry_bytes,
        max_archive_bytes,
    }
}

/// Poll a platform pasteboard and retain each changed selection locally.
/// The returned task ends only when aborted or the runtime shuts down.
pub fn spawn_local_history_poll<C>(
    clipboard: Arc<C>,
    recorder: ClipboardHistoryRecorder,
    local_device_id: DeviceId,
) -> tokio::task::JoinHandle<()>
where
    C: Clipboard + 'static,
{
    tokio::spawn(async move {
        let mut last_fingerprint: Option<ContentFingerprint> = None;
        let mut backend_error_reported = false;
        loop {
            match clipboard.read().await {
                Ok(Some(snapshot)) => {
                    backend_error_reported = false;
                    let fingerprint = snapshot.fingerprint();
                    if last_fingerprint != Some(fingerprint) {
                        last_fingerprint = Some(fingerprint);
                        if let Err(error) = recorder
                            .record(snapshot, local_device_id, now_millis(), true)
                            .await
                        {
                            tracing::warn!(%error, "failed to persist clipboard history");
                        }
                    }
                }
                Ok(None) => {
                    backend_error_reported = false;
                    last_fingerprint = None;
                }
                Err(error) => {
                    if !backend_error_reported {
                        tracing::warn!(%error, "clipboard history polling failed");
                        backend_error_reported = true;
                    }
                }
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(400)).await;
        }
    })
}

/// Resolve the encrypted history file beside the main configuration.
#[must_use]
pub fn archive_path(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .map(|parent| parent.join("clipboard-history.enc"))
        .unwrap_or_else(|| PathBuf::from("clipboard-history.enc"))
}

#[must_use]
pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn records_off_runtime_and_preserves_remote_origin_on_local_observation() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        let config = ClipboardConfig {
            history_enabled: true,
            ..ClipboardConfig::default()
        };
        let recorder = ClipboardHistoryRecorder::open(&config_path, &config)
            .unwrap()
            .unwrap();
        let local = DeviceId::generate();
        let remote = DeviceId::generate();
        let snapshot = ClipboardSnapshot::from_text("shared entry");

        recorder
            .record(snapshot.clone(), remote, 1, false)
            .await
            .unwrap();
        recorder.record(snapshot, local, 2, true).await.unwrap();

        let archive = ClipboardHistoryArchive::open(
            archive_path(&config_path),
            ClipboardHistoryArchiveConfig::default(),
        )
        .unwrap();
        let entry = archive.entries().next().unwrap();
        assert_eq!(entry.origin, remote);
        assert_eq!(entry.at_millis, 2);
        let disk = std::fs::read(archive.path()).unwrap();
        assert!(!disk.windows(12).any(|window| window == b"shared entry"));
    }

    #[tokio::test]
    async fn recording_after_external_clear_does_not_resurrect_stale_entries() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        let config = ClipboardConfig {
            history_enabled: true,
            ..ClipboardConfig::default()
        };
        let recorder = ClipboardHistoryRecorder::open(&config_path, &config)
            .unwrap()
            .unwrap();
        let local = DeviceId::generate();
        let stale = ClipboardSnapshot::from_text("must stay cleared");
        let fresh = ClipboardSnapshot::from_text("fresh after clear");

        recorder.record(stale, local, 1, false).await.unwrap();

        let path = archive_path(&config_path);
        let archive_limits = archive_config(&config);
        clear_unpinned(&config_path, &config).unwrap();

        recorder
            .record(fresh.clone(), local, 2, false)
            .await
            .unwrap();

        let reloaded = ClipboardHistoryArchive::open(path, archive_limits).unwrap();
        let entries = reloaded.entries().collect::<Vec<_>>();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].snapshot, fresh);
    }

    #[test]
    fn disabled_history_does_not_create_a_key() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        let config = ClipboardConfig {
            history_enabled: false,
            ..ClipboardConfig::default()
        };

        assert!(
            ClipboardHistoryRecorder::open(&config_path, &config)
                .unwrap()
                .is_none()
        );
        assert!(!archive_path(&config_path).with_extension("key").exists());
    }
}
