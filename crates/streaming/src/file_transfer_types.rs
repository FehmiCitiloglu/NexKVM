//! File transfer metadata and manifest types.
//!
//! This module models drag/drop and picker-initiated file/folder transfers in a
//! platform-neutral shape. It deliberately does not perform filesystem I/O.

use std::path::Path;

use bytes::Bytes;
use coklu_core::identity::DeviceId;
use uuid::Uuid;

use crate::TransferError;

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
}

impl TransferEntry {
    /// Construct a validated file entry.
    ///
    /// # Errors
    /// Returns [`TransferError::InvalidPath`] for unsafe paths.
    pub fn file(relative_path: impl Into<String>, size_bytes: u64) -> Result<Self, TransferError> {
        let relative_path = relative_path.into();
        validate_relative_path(&relative_path)?;
        Ok(Self {
            relative_path,
            is_dir: false,
            size_bytes,
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
        })
    }
}

/// A complete file/folder payload description.
#[derive(Debug, Clone)]
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
        if entries.is_empty() {
            return Err(TransferError::EmptyManifest);
        }
        let total_bytes = entries
            .iter()
            .filter(|e| !e.is_dir)
            .map(|e| e.size_bytes)
            .sum();
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
fn validate_relative_path(path: &str) -> Result<(), TransferError> {
    let p = Path::new(path);
    if path.is_empty() || p.is_absolute() {
        return Err(TransferError::InvalidPath(path.into()));
    }
    for c in p.components() {
        if matches!(
            c,
            std::path::Component::ParentDir | std::path::Component::RootDir
        ) {
            return Err(TransferError::InvalidPath(path.into()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_absolute_and_parent_paths() {
        assert!(TransferEntry::file("/etc/passwd", 1).is_err());
        assert!(TransferEntry::file("../secret.txt", 1).is_err());
        assert!(TransferEntry::dir("../../tmp").is_err());
    }

    #[test]
    fn builds_manifest_and_counts_files() {
        let from = DeviceId::generate();
        let entries = vec![
            TransferEntry::dir("photos").unwrap(),
            TransferEntry::file("photos/a.png", 10).unwrap(),
            TransferEntry::file("photos/b.png", 20).unwrap(),
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
