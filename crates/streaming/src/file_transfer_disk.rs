//! Bounded disk-streaming primitives for file and empty-directory transfer.
//!
//! The source side reads through [`Read`] + [`Seek`] and holds at most one
//! configured chunk in memory. The receive side writes contiguous decoded
//! chunks to a sibling `.part` file, flushes/syncs it, and publishes with an
//! atomic no-clobber hard-link operation.
//! All operations are synchronous; async runtimes must call them from a
//! dedicated blocking worker (for example `tokio::task::spawn_blocking`).
//!
//! # Filesystem security contract
//! Paths are canonical protocol-relative paths (forward slashes only). Static
//! symlink ancestors, traversal, final symlinks, and existing destinations are
//! rejected. Portable `std::fs` path APIs cannot eliminate a malicious local
//! process swapping an already-checked parent directory (TOCTOU); therefore the
//! supplied root must be a trusted directory not concurrently mutable by an
//! untrusted local principal. A platform integration accepting hostile roots
//! must additionally sandbox by directory handle with OS-specific no-follow
//! operations.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

use bytes::Bytes;

use crate::file_transfer_compression::TransferCompression;
use crate::file_transfer_session::{DecodedChunk, TransferChunk, validate_chunk_size};
use crate::file_transfer_types::{TransferId, validate_relative_path};
use crate::{MAX_TRANSFER_MANIFEST_ENTRIES, MAX_TRANSFER_TOTAL_BYTES, TransferError};

/// Produces raw, bounded [`TransferChunk`]s from a seekable source.
///
/// Chunks are intentionally not encrypted again: send their encoded
/// [`FileTransferMessage`](crate::FileTransferMessage) through NexKVM's outer
/// authenticated connection.
pub struct TransferFileReader<R> {
    reader: R,
    transfer_id: TransferId,
    file_index: u32,
    expected_len: u64,
    offset: u64,
    chunk_size: usize,
    finished: bool,
}

impl<R> fmt::Debug for TransferFileReader<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TransferFileReader")
            .field("transfer_id", &self.transfer_id)
            .field("file_index", &self.file_index)
            .field("expected_len", &self.expected_len)
            .field("offset", &self.offset)
            .field("chunk_size", &self.chunk_size)
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}

impl<R: Read + Seek> TransferFileReader<R> {
    /// Start reading a manifest file entry from offset zero.
    ///
    /// The source's current length must equal `expected_len`; the constructor
    /// seeks to offset zero before returning.
    ///
    /// # Errors
    /// Returns [`TransferError`] for invalid limits, a source length mismatch,
    /// or seek I/O failure.
    pub fn new(
        reader: R,
        transfer_id: TransferId,
        file_index: u32,
        expected_len: u64,
        chunk_size: usize,
    ) -> Result<Self, TransferError> {
        Self::at_offset(
            reader,
            transfer_id,
            file_index,
            expected_len,
            0,
            chunk_size,
            false,
        )
    }

    /// Resume reading at a persisted contiguous byte offset.
    ///
    /// An offset equal to `expected_len` is already complete, including a
    /// completed zero-byte file, and produces no duplicate final marker.
    ///
    /// # Errors
    /// Returns [`TransferError`] for invalid limits/offsets, a source length
    /// mismatch, or seek I/O failure.
    pub fn resume(
        reader: R,
        transfer_id: TransferId,
        file_index: u32,
        expected_len: u64,
        offset: u64,
        chunk_size: usize,
    ) -> Result<Self, TransferError> {
        Self::at_offset(
            reader,
            transfer_id,
            file_index,
            expected_len,
            offset,
            chunk_size,
            true,
        )
    }

    fn at_offset(
        mut reader: R,
        transfer_id: TransferId,
        file_index: u32,
        expected_len: u64,
        offset: u64,
        chunk_size: usize,
        resume: bool,
    ) -> Result<Self, TransferError> {
        validate_chunk_size(chunk_size)?;
        validate_file_index(file_index)?;
        enforce_file_size(expected_len)?;
        if offset > expected_len {
            return Err(TransferError::UnexpectedOffset {
                expected: expected_len,
                actual: offset,
            });
        }
        let actual_len = reader.seek(SeekFrom::End(0))?;
        if actual_len != expected_len {
            return Err(TransferError::Codec(format!(
                "source length {actual_len} does not match manifest length {expected_len}"
            )));
        }
        reader.seek(SeekFrom::Start(offset))?;
        Ok(Self {
            reader,
            transfer_id,
            file_index,
            expected_len,
            offset,
            chunk_size,
            finished: resume && offset == expected_len,
        })
    }

    /// Read and produce the next raw chunk.
    ///
    /// # Errors
    /// Returns [`TransferError::Io`] if the source changes/truncates or reading
    /// fails. State advances only after a complete chunk was read.
    pub fn next_chunk(&mut self) -> Result<Option<TransferChunk>, TransferError> {
        if self.finished {
            return Ok(None);
        }
        if self.expected_len == 0 {
            self.finished = true;
            return Ok(Some(TransferChunk {
                transfer_id: self.transfer_id,
                file_index: self.file_index,
                offset: 0,
                plain_len: 0,
                compression: TransferCompression::None,
                final_chunk_for_file: true,
                payload: Bytes::new(),
            }));
        }

        let remaining = self.expected_len - self.offset;
        let read_len = usize::try_from(remaining.min(self.chunk_size as u64))
            .map_err(|_| TransferError::Codec("chunk length does not fit usize".into()))?;
        let mut payload = vec![0u8; read_len];
        let start = self.offset;
        if let Err(read_error) = self.reader.read_exact(&mut payload) {
            if let Err(seek_error) = self.reader.seek(SeekFrom::Start(start)) {
                return Err(TransferError::Io(std::io::Error::new(
                    seek_error.kind(),
                    format!(
                        "chunk read failed ({read_error}); position rollback failed ({seek_error})"
                    ),
                )));
            }
            return Err(read_error.into());
        }
        let end = start
            .checked_add(read_len as u64)
            .ok_or_else(|| TransferError::Codec("source offset overflow".into()))?;
        let final_chunk_for_file = end == self.expected_len;
        self.offset = end;
        self.finished = final_chunk_for_file;
        Ok(Some(TransferChunk {
            transfer_id: self.transfer_id,
            file_index: self.file_index,
            offset: start,
            plain_len: u32::try_from(read_len).map_err(|_| TransferError::TooLarge {
                size: read_len,
                limit: u32::MAX as usize,
            })?,
            compression: TransferCompression::None,
            final_chunk_for_file,
            payload: Bytes::from(payload),
        }))
    }

    /// Next byte offset that would be read.
    #[must_use]
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Return the underlying reader.
    #[must_use]
    pub fn into_inner(self) -> R {
        self.reader
    }
}

/// Contiguous `.part` file writer with resume and no-overwrite finalization.
///
/// Dropping a writer intentionally preserves its `.part` file for resume.
pub struct TransferPartWriter {
    file: File,
    final_path: PathBuf,
    part_path: PathBuf,
    file_index: u32,
    expected_len: u64,
    offset: u64,
    complete: bool,
}

impl fmt::Debug for TransferPartWriter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TransferPartWriter")
            .field("final_path", &self.final_path)
            .field("part_path", &self.part_path)
            .field("file_index", &self.file_index)
            .field("expected_len", &self.expected_len)
            .field("offset", &self.offset)
            .field("complete", &self.complete)
            .finish_non_exhaustive()
    }
}

impl TransferPartWriter {
    /// Create a new sibling `.part` file without replacing any existing path.
    ///
    /// Parent directories must already exist; use
    /// [`create_transfer_directory`] for manifest directory entries.
    ///
    /// # Errors
    /// Returns [`TransferError`] for unsafe/traversing paths, symlink ancestors,
    /// an existing final/part path, invalid limits, or filesystem I/O failure.
    pub fn create(
        root: impl AsRef<Path>,
        relative_path: &str,
        file_index: u32,
        expected_len: u64,
    ) -> Result<Self, TransferError> {
        validate_file_index(file_index)?;
        enforce_file_size(expected_len)?;
        let final_path = prepare_file_destination(root.as_ref(), relative_path)?;
        let part_path = part_path_for(&final_path)?;
        reject_existing_path(&part_path)?;
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&part_path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                return Err(TransferError::DestinationExists(
                    part_path.display().to_string(),
                ));
            }
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            file,
            final_path,
            part_path,
            file_index,
            expected_len,
            offset: 0,
            complete: false,
        })
    }

    /// Resume an existing regular `.part` file at exactly `offset` bytes.
    ///
    /// # Errors
    /// Returns [`TransferError`] when the part is absent, is a symlink/non-file,
    /// its on-disk length differs from `offset`, or another path/limit/I/O check
    /// fails.
    pub fn resume(
        root: impl AsRef<Path>,
        relative_path: &str,
        file_index: u32,
        expected_len: u64,
        offset: u64,
    ) -> Result<Self, TransferError> {
        validate_file_index(file_index)?;
        enforce_file_size(expected_len)?;
        if offset > expected_len {
            return Err(TransferError::UnexpectedOffset {
                expected: expected_len,
                actual: offset,
            });
        }
        let final_path = prepare_file_destination(root.as_ref(), relative_path)?;
        let part_path = part_path_for(&final_path)?;
        let metadata = fs::symlink_metadata(&part_path)?;
        if metadata.file_type().is_symlink() {
            return Err(TransferError::UnsafeDestination(
                part_path.display().to_string(),
            ));
        }
        if !metadata.is_file() {
            return Err(TransferError::UnsafeDestination(
                part_path.display().to_string(),
            ));
        }
        if metadata.len() != offset {
            return Err(TransferError::UnexpectedOffset {
                expected: offset,
                actual: metadata.len(),
            });
        }
        let mut file = OpenOptions::new().read(true).write(true).open(&part_path)?;
        file.seek(SeekFrom::Start(offset))?;
        Ok(Self {
            file,
            final_path,
            part_path,
            file_index,
            expected_len,
            offset,
            complete: offset == expected_len,
        })
    }

    /// Append one decoded chunk at the required contiguous offset.
    ///
    /// The final marker is accepted only when the resulting length exactly
    /// equals the manifest size. A failed validation never advances state.
    ///
    /// # Errors
    /// Returns [`TransferError`] for file-index/offset/final-marker mismatch,
    /// length overrun, or write I/O failure.
    pub fn write_chunk(&mut self, chunk: &DecodedChunk) -> Result<(), TransferError> {
        if self.complete {
            return Err(TransferError::Codec("chunk for completed part file".into()));
        }
        if chunk.file_index != self.file_index {
            return Err(TransferError::Codec(format!(
                "chunk file index {} does not match writer index {}",
                chunk.file_index, self.file_index
            )));
        }
        if chunk.offset != self.offset {
            return Err(TransferError::UnexpectedOffset {
                expected: self.offset,
                actual: chunk.offset,
            });
        }
        if chunk.bytes.is_empty() && self.expected_len != 0 {
            return Err(TransferError::Codec(
                "empty chunk for non-empty file".into(),
            ));
        }
        let chunk_len = u64::try_from(chunk.bytes.len())
            .map_err(|_| TransferError::Codec("chunk length does not fit u64".into()))?;
        let end = self
            .offset
            .checked_add(chunk_len)
            .ok_or_else(|| TransferError::Codec("part file offset overflow".into()))?;
        if end > self.expected_len {
            return Err(TransferError::TooLarge {
                size: usize::try_from(end).unwrap_or(usize::MAX),
                limit: usize::try_from(self.expected_len).unwrap_or(usize::MAX),
            });
        }
        let should_be_final = end == self.expected_len;
        if chunk.final_chunk_for_file != should_be_final {
            return Err(TransferError::Codec(
                "chunk final marker does not match manifest length".into(),
            ));
        }

        let start = self.offset;
        if let Err(write_error) = self.file.write_all(&chunk.bytes) {
            let rollback = self
                .file
                .set_len(start)
                .and_then(|()| self.file.seek(SeekFrom::Start(start)).map(|_| ()));
            if let Err(rollback_error) = rollback {
                return Err(TransferError::Io(std::io::Error::new(
                    rollback_error.kind(),
                    format!(
                        "chunk write failed ({write_error}); rollback failed ({rollback_error})"
                    ),
                )));
            }
            return Err(write_error.into());
        }
        self.offset = end;
        self.complete = chunk.final_chunk_for_file;
        Ok(())
    }

    /// Write a raw chunk decoded from an outer-authenticated wire message.
    ///
    /// This is the zero-copy bridge for [`TransferFileReader`] output and only
    /// accepts `Compression::None` with `payload.len() == plain_len`. Encrypted
    /// or compressed legacy chunks must first pass through
    /// [`TransferReceiver`](crate::TransferReceiver) and [`write_chunk`](Self::write_chunk).
    ///
    /// # Errors
    /// Returns [`TransferError`] when the chunk is not raw/plain or violates the
    /// same index, offset, length, final-marker, or I/O checks as `write_chunk`.
    pub fn write_raw_chunk(&mut self, chunk: &TransferChunk) -> Result<(), TransferError> {
        if chunk.compression != TransferCompression::None {
            return Err(TransferError::Codec(
                "raw disk writer requires an uncompressed chunk".into(),
            ));
        }
        if chunk.payload.len() != chunk.plain_len as usize {
            return Err(TransferError::Codec(
                "raw chunk payload length does not match plain length".into(),
            ));
        }
        self.write_chunk(&DecodedChunk {
            file_index: chunk.file_index,
            offset: chunk.offset,
            bytes: chunk.payload.clone(),
            final_chunk_for_file: chunk.final_chunk_for_file,
        })
    }

    /// Flush buffered bytes to the operating system.
    ///
    /// # Errors
    /// Returns [`TransferError::Io`] on failure.
    pub fn flush(&mut self) -> Result<(), TransferError> {
        self.file.flush()?;
        Ok(())
    }

    /// Flush, sync, and publish the completed part without replacing a target.
    ///
    /// Publication uses `hard_link(part, final)` followed by removal of the
    /// `.part` name. Creating the final link fails atomically if any destination
    /// already exists, unlike `rename` on platforms where rename overwrites.
    ///
    /// # Errors
    /// Returns [`TransferError`] unless a final marker completed the exact
    /// manifest length, or when sync/link/removal fails. On publication failure
    /// before linking, the `.part` file remains available for recovery.
    pub fn finalize(mut self) -> Result<PathBuf, TransferError> {
        if !self.complete || self.offset != self.expected_len {
            return Err(TransferError::Codec(
                "cannot finalize incomplete transfer part".into(),
            ));
        }
        self.file.flush()?;
        self.file.sync_all()?;
        drop(self.file);

        reject_existing_path(&self.final_path)?;
        match fs::hard_link(&self.part_path, &self.final_path) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                return Err(TransferError::DestinationExists(
                    self.final_path.display().to_string(),
                ));
            }
            Err(error) => return Err(error.into()),
        }
        fs::remove_file(&self.part_path)?;
        Ok(self.final_path)
    }

    /// Delete the resumable part file.
    ///
    /// # Errors
    /// Returns [`TransferError::Io`] if deletion fails.
    pub fn cancel(self) -> Result<(), TransferError> {
        let part_path = self.part_path.clone();
        drop(self.file);
        fs::remove_file(part_path)?;
        Ok(())
    }

    /// Current contiguous byte offset.
    #[must_use]
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Temporary sibling `.part` path.
    #[must_use]
    pub fn part_path(&self) -> &Path {
        &self.part_path
    }

    /// Final destination path.
    #[must_use]
    pub fn final_path(&self) -> &Path {
        &self.final_path
    }
}

/// Create a manifest directory entry component-by-component without following
/// symlinks or replacing files.
///
/// Existing real directories are accepted, making the operation idempotent.
/// See the module-level trusted-root/TOCTOU contract.
///
/// # Errors
/// Returns [`TransferError`] for traversal/non-canonical paths, symlink
/// ancestors, a file occupying any component, or filesystem I/O failure.
pub fn create_transfer_directory(
    root: impl AsRef<Path>,
    relative_path: &str,
) -> Result<PathBuf, TransferError> {
    validate_relative_path(relative_path)?;
    let root = root.as_ref();
    validate_root(root)?;
    let mut current = root.to_path_buf();
    for component in Path::new(relative_path).components() {
        let Component::Normal(name) = component else {
            return Err(TransferError::InvalidPath(relative_path.into()));
        };
        current.push(name);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(TransferError::UnsafeDestination(
                    current.display().to_string(),
                ));
            }
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                return Err(TransferError::DestinationExists(
                    current.display().to_string(),
                ));
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                if let Err(create_error) = fs::create_dir(&current) {
                    if create_error.kind() != ErrorKind::AlreadyExists {
                        return Err(create_error.into());
                    }
                    validate_existing_directory(&current)?;
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(current)
}

fn prepare_file_destination(root: &Path, relative_path: &str) -> Result<PathBuf, TransferError> {
    validate_relative_path(relative_path)?;
    validate_root(root)?;
    let relative = Path::new(relative_path);
    let mut current = root.to_path_buf();
    if let Some(parent) = relative.parent() {
        for component in parent.components() {
            let Component::Normal(name) = component else {
                return Err(TransferError::InvalidPath(relative_path.into()));
            };
            current.push(name);
            validate_existing_directory(&current)?;
        }
    }
    let final_path = root.join(relative);
    reject_existing_path(&final_path)?;
    Ok(final_path)
}

fn validate_root(root: &Path) -> Result<(), TransferError> {
    let metadata = fs::symlink_metadata(root)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(TransferError::UnsafeDestination(root.display().to_string()));
    }
    Ok(())
}

fn validate_existing_directory(path: &Path) -> Result<(), TransferError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(TransferError::UnsafeDestination(path.display().to_string()));
    }
    if !metadata.is_dir() {
        return Err(TransferError::DestinationExists(path.display().to_string()));
    }
    Ok(())
}

fn reject_existing_path(path: &Path) -> Result<(), TransferError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(TransferError::UnsafeDestination(path.display().to_string()))
        }
        Ok(_) => Err(TransferError::DestinationExists(path.display().to_string())),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn part_path_for(final_path: &Path) -> Result<PathBuf, TransferError> {
    let file_name = final_path
        .file_name()
        .ok_or_else(|| TransferError::InvalidPath(final_path.display().to_string()))?;
    let mut part_name = file_name.to_os_string();
    part_name.push(".part");
    Ok(final_path.with_file_name(part_name))
}

fn validate_file_index(file_index: u32) -> Result<(), TransferError> {
    if file_index as usize >= MAX_TRANSFER_MANIFEST_ENTRIES {
        return Err(TransferError::TooLarge {
            size: file_index as usize,
            limit: MAX_TRANSFER_MANIFEST_ENTRIES - 1,
        });
    }
    Ok(())
}

fn enforce_file_size(expected_len: u64) -> Result<(), TransferError> {
    if expected_len > MAX_TRANSFER_TOTAL_BYTES {
        return Err(TransferError::TooLarge {
            size: usize::try_from(expected_len).unwrap_or(usize::MAX),
            limit: usize::try_from(MAX_TRANSFER_TOTAL_BYTES).unwrap_or(usize::MAX),
        });
    }
    Ok(())
}
