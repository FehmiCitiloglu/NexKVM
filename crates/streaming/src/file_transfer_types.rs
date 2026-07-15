//! File transfer metadata and manifest types.
//!
//! This module models drag/drop and picker-initiated file/folder transfers in a
//! platform-neutral shape. It deliberately does not perform filesystem I/O.

use std::collections::HashSet;

use bytes::Bytes;
use nexkvm_core::identity::DeviceId;
use uuid::Uuid;

use crate::{
    FILE_TRANSFER_SHA256_BYTES, MAX_TRANSFER_MANIFEST_ENTRIES, MAX_TRANSFER_PATH_BYTES,
    MAX_TRANSFER_TOTAL_BYTES, TransferError,
};

/// Stable identifier for one file/folder transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TransferId(pub Uuid);

impl TransferId {
    /// Generate a new transfer id.
    #[must_use]
    pub fn generate() -> Self {
        Self(Uuid::new_v4())
    }
}

/// How a transfer was initiated on the sender.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferSource {
    /// User dragged files/folders into the target device surface.
    DragDrop,
    /// User chose files from an explicit file picker.
    Picker,
    /// Programmatic/background sync trigger.
    Sync,
}

/// One entry in a transfer payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferEntry {
    /// Relative path under the transfer root.
    pub relative_path: String,
    /// Directory marker; files carry bytes in stream chunks.
    pub is_dir: bool,
    /// Byte length for files, zero for directories.
    pub size_bytes: u64,
    /// Expected SHA-256 of the complete file. Directories never carry one.
    pub sha256: Option<[u8; FILE_TRANSFER_SHA256_BYTES]>,
}

impl TransferEntry {
    /// Construct a validated file entry.
    ///
    /// # Errors
    /// Returns [`TransferError::InvalidPath`] for unsafe paths.
    pub fn file(
        relative_path: impl Into<String>,
        size_bytes: u64,
        sha256: [u8; FILE_TRANSFER_SHA256_BYTES],
    ) -> Result<Self, TransferError> {
        let relative_path = relative_path.into();
        validate_relative_path(&relative_path)?;
        Ok(Self {
            relative_path,
            is_dir: false,
            size_bytes,
            sha256: Some(sha256),
        })
    }

    /// Construct a validated directory entry.
    ///
    /// # Errors
    /// Returns [`TransferError::InvalidPath`] for unsafe paths.
    pub fn dir(relative_path: impl Into<String>) -> Result<Self, TransferError> {
        let relative_path = relative_path.into();
        validate_relative_path(&relative_path)?;
        Ok(Self {
            relative_path,
            is_dir: true,
            size_bytes: 0,
            sha256: None,
        })
    }
}

/// A complete file/folder payload description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferManifest {
    /// Transfer id.
    pub id: TransferId,
    /// Sender device.
    pub from: DeviceId,
    /// Optional specific target; `None` for broadcast/selection flow.
    pub to: Option<DeviceId>,
    /// Initiation source.
    pub source: TransferSource,
    /// Entries in deterministic traversal order.
    pub entries: Vec<TransferEntry>,
    /// Total bytes across file entries.
    pub total_bytes: u64,
}

impl TransferManifest {
    /// Build and validate a manifest.
    ///
    /// # Errors
    /// Returns [`TransferError::EmptyManifest`] when there are no entries.
    pub fn new(
        id: TransferId,
        from: DeviceId,
        to: Option<DeviceId>,
        source: TransferSource,
        entries: Vec<TransferEntry>,
    ) -> Result<Self, TransferError> {
        let total_bytes = validate_manifest_entries(&entries)?;
        Ok(Self {
            id,
            from,
            to,
            source,
            entries,
            total_bytes,
        })
    }

    /// Number of file entries (directories excluded).
    #[must_use]
    pub fn file_count(&self) -> usize {
        self.entries.iter().filter(|e| !e.is_dir).count()
    }
}

/// Outbound bytes for one file entry in the manifest.
#[derive(Debug, Clone)]
pub struct TransferFileData {
    /// Index into [`TransferManifest::entries`].
    pub entry_index: u32,
    /// Entire file payload for chunking.
    pub bytes: Bytes,
}

/// Validates a path remains relative and traversal-safe.
pub(crate) fn validate_relative_path(path: &str) -> Result<(), TransferError> {
    if path.len() > MAX_TRANSFER_PATH_BYTES {
        return Err(TransferError::TooLarge {
            size: path.len(),
            limit: MAX_TRANSFER_PATH_BYTES,
        });
    }
    if path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || path.contains('\0')
        || path.contains(':')
        || path.ends_with('/')
        || path.chars().any(char::is_control)
    {
        return Err(TransferError::InvalidPath(path.into()));
    }
    for component in path.split('/') {
        if component.is_empty()
            || component == "."
            || component == ".."
            || component.ends_with([' ', '.'])
            || is_windows_reserved_name(component)
        {
            return Err(TransferError::InvalidPath(path.into()));
        }
    }
    Ok(())
}

pub(crate) fn validate_manifest_entries(entries: &[TransferEntry]) -> Result<u64, TransferError> {
    if entries.is_empty() {
        return Err(TransferError::EmptyManifest);
    }
    if entries.len() > MAX_TRANSFER_MANIFEST_ENTRIES {
        return Err(TransferError::TooLarge {
            size: entries.len(),
            limit: MAX_TRANSFER_MANIFEST_ENTRIES,
        });
    }

    let mut portable_paths = HashSet::with_capacity(entries.len());
    let mut total_bytes = 0u64;
    for entry in entries {
        validate_relative_path(&entry.relative_path)?;
        if !portable_paths.insert(entry.relative_path.to_lowercase()) {
            return Err(TransferError::InvalidPath(format!(
                "duplicate path {}",
                entry.relative_path
            )));
        }
        if entry.is_dir {
            if entry.size_bytes != 0 {
                return Err(TransferError::Codec(
                    "directory entry has non-zero size".into(),
                ));
            }
            if entry.sha256.is_some() {
                return Err(TransferError::Codec(
                    "directory entry has a content digest".into(),
                ));
            }
            continue;
        }
        if entry.sha256.is_none() {
            return Err(TransferError::Codec(
                "file entry is missing its SHA-256 digest".into(),
            ));
        }
        total_bytes = total_bytes
            .checked_add(entry.size_bytes)
            .ok_or_else(|| TransferError::Codec("manifest byte total overflow".into()))?;
        if total_bytes > MAX_TRANSFER_TOTAL_BYTES {
            return Err(TransferError::TooLarge {
                size: usize::try_from(total_bytes).unwrap_or(usize::MAX),
                limit: usize::try_from(MAX_TRANSFER_TOTAL_BYTES).unwrap_or(usize::MAX),
            });
        }
    }
    Ok(total_bytes)
}

fn is_windows_reserved_name(component: &str) -> bool {
    let stem = component
        .split_once('.')
        .map_or(component, |(stem, _)| stem)
        .to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem.strip_prefix("COM").is_some_and(|suffix| {
            matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
        })
        || stem.strip_prefix("LPT").is_some_and(|suffix| {
            matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_absolute_and_parent_paths() {
        assert!(TransferEntry::file("/etc/passwd", 1, [0; 32]).is_err());
        assert!(TransferEntry::file("../secret.txt", 1, [0; 32]).is_err());
        assert!(TransferEntry::dir("../../tmp").is_err());
    }

    #[test]
    fn builds_manifest_and_counts_files() {
        let from = DeviceId::generate();
        let entries = vec![
            TransferEntry::dir("photos").unwrap(),
            TransferEntry::file("photos/a.png", 10, [1; 32]).unwrap(),
            TransferEntry::file("photos/b.png", 20, [2; 32]).unwrap(),
        ];
        let manifest = TransferManifest::new(
            TransferId::generate(),
            from,
            None,
            TransferSource::DragDrop,
            entries,
        )
        .unwrap();
        assert_eq!(manifest.file_count(), 2);
        assert_eq!(manifest.total_bytes, 30);
    }
}
