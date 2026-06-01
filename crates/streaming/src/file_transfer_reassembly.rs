//! Receive-side reassembly of decoded chunks into completed files.
//!
//! [`TransferReceiver`](crate::TransferReceiver) opens and decompresses one
//! chunk at a time but does not collect them; [`TransferReassembler`] is the
//! drag-and-drop *endpoint* that orders [`DecodedChunk`]s back into whole files
//! against their [`TransferManifest`]. It performs no filesystem I/O — the
//! daemon writes the returned [`CompletedFile`]s to disk under the (already
//! traversal-validated) manifest paths.
//!
//! Safety: chunk `file_index`/`offset`/length are peer-supplied, so each is
//! validated against the manifest before any bytes are buffered — an unknown
//! index, a non-contiguous offset, or an overrun past the declared file size is
//! rejected rather than trusted.

use std::collections::HashMap;

use bytes::Bytes;

use crate::TransferError;
use crate::file_transfer_session::DecodedChunk;
use crate::file_transfer_types::TransferManifest;

/// A fully reassembled file ready to persist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletedFile {
    /// Manifest entry index this file came from.
    pub entry_index: u32,
    /// Traversal-safe relative path from the manifest.
    pub relative_path: String,
    /// Complete file contents.
    pub bytes: Bytes,
}

/// Per-file accumulation state.
#[derive(Debug)]
struct FileBuffer {
    relative_path: String,
    expected_len: u64,
    data: Vec<u8>,
    done: bool,
}

/// Reassembles ordered chunks into whole files for one transfer.
#[derive(Debug)]
pub struct TransferReassembler {
    files: HashMap<u32, FileBuffer>,
    remaining_files: usize,
}

impl TransferReassembler {
    /// Build a reassembler for `manifest`'s file entries (directories excluded).
    #[must_use]
    pub fn new(manifest: &TransferManifest) -> Self {
        let files: HashMap<u32, FileBuffer> = manifest
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.is_dir)
            .map(|(idx, e)| {
                (
                    idx as u32,
                    FileBuffer {
                        relative_path: e.relative_path.clone(),
                        expected_len: e.size_bytes,
                        data: Vec::new(),
                        done: false,
                    },
                )
            })
            .collect();
        let remaining_files = files.len();
        Self {
            files,
            remaining_files,
        }
    }

    /// Whether every file entry has been fully received.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.remaining_files == 0
    }

    /// Number of file entries not yet fully received.
    #[must_use]
    pub fn remaining_files(&self) -> usize {
        self.remaining_files
    }

    /// Accept one decoded chunk, returning the [`CompletedFile`] when this chunk
    /// finishes its file.
    ///
    /// Chunks for a given file must arrive in contiguous offset order (the
    /// natural output of [`TransferSender`](crate::TransferSender), including
    /// after a checkpoint resume).
    ///
    /// # Errors
    /// Returns [`TransferError::Codec`] for an unknown file index, a duplicate
    /// chunk on an already-finished file, a non-contiguous offset, or a payload
    /// that would overrun the manifest-declared file size.
    pub fn accept(&mut self, chunk: DecodedChunk) -> Result<Option<CompletedFile>, TransferError> {
        let buffer = self.files.get_mut(&chunk.file_index).ok_or_else(|| {
            TransferError::Codec(format!("unknown file index {}", chunk.file_index))
        })?;

        if buffer.done {
            return Err(TransferError::Codec("chunk for completed file".into()));
        }
        if chunk.offset != buffer.data.len() as u64 {
            return Err(TransferError::Codec("non-contiguous chunk offset".into()));
        }

        let new_len = buffer.data.len() as u64 + chunk.bytes.len() as u64;
        if new_len > buffer.expected_len {
            return Err(TransferError::TooLarge {
                size: new_len as usize,
                limit: buffer.expected_len as usize,
            });
        }
        buffer.data.extend_from_slice(&chunk.bytes);

        if !chunk.final_chunk_for_file {
            return Ok(None);
        }

        if buffer.data.len() as u64 != buffer.expected_len {
            return Err(TransferError::Codec(
                "file shorter than declared size".into(),
            ));
        }

        buffer.done = true;
        self.remaining_files -= 1;
        Ok(Some(CompletedFile {
            entry_index: chunk.file_index,
            relative_path: buffer.relative_path.clone(),
            bytes: Bytes::from(std::mem::take(&mut buffer.data)),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_transfer_cipher::PlaintextTransferCipher;
    use crate::file_transfer_compression::TransferCompressionPolicy;
    use crate::file_transfer_session::{TransferChunk, TransferReceiver, TransferSender};
    use crate::file_transfer_types::{
        TransferEntry, TransferFileData, TransferId, TransferManifest, TransferSource,
    };
    use coklu_core::identity::DeviceId;

    fn manifest() -> TransferManifest {
        TransferManifest::new(
            TransferId::generate(),
            DeviceId::generate(),
            None,
            TransferSource::DragDrop,
            vec![
                TransferEntry::dir("photos").unwrap(),
                TransferEntry::file("photos/a.bin", 2048).unwrap(),
                TransferEntry::file("photos/b.bin", 1536).unwrap(),
            ],
        )
        .unwrap()
    }

    fn files() -> Vec<TransferFileData> {
        vec![
            TransferFileData {
                entry_index: 1,
                bytes: Bytes::from(vec![b'a'; 2048]),
            },
            TransferFileData {
                entry_index: 2,
                bytes: Bytes::from(vec![b'b'; 1536]),
            },
        ]
    }

    #[test]
    fn end_to_end_reassembles_all_files() {
        let m = manifest();
        let mut sender = TransferSender::new(
            m.id,
            files(),
            1024,
            TransferCompressionPolicy::default(),
            Box::new(PlaintextTransferCipher),
        );
        let mut receiver = TransferReceiver::new(Box::new(PlaintextTransferCipher));
        let mut reassembler = TransferReassembler::new(&m);

        let mut completed = Vec::new();
        while let Some(chunk) = sender.next_chunk().unwrap() {
            // Round-trip the wire encoding to exercise the full path.
            let parsed = TransferChunk::decode(chunk.encode().unwrap()).unwrap();
            let decoded = receiver.accept(parsed).unwrap();
            if let Some(file) = reassembler.accept(decoded).unwrap() {
                completed.push(file);
            }
        }

        assert!(reassembler.is_complete());
        assert_eq!(completed.len(), 2);
        assert_eq!(completed[0].relative_path, "photos/a.bin");
        assert_eq!(completed[0].bytes, Bytes::from(vec![b'a'; 2048]));
        assert_eq!(completed[1].relative_path, "photos/b.bin");
        assert_eq!(completed[1].bytes.len(), 1536);
    }

    #[test]
    fn rejects_unknown_file_index() {
        let mut r = TransferReassembler::new(&manifest());
        let bad = DecodedChunk {
            file_index: 99,
            offset: 0,
            bytes: Bytes::from_static(b"x"),
            final_chunk_for_file: false,
        };
        assert!(matches!(r.accept(bad), Err(TransferError::Codec(_))));
    }

    #[test]
    fn rejects_non_contiguous_offset() {
        let mut r = TransferReassembler::new(&manifest());
        let gap = DecodedChunk {
            file_index: 1,
            offset: 16,
            bytes: Bytes::from_static(b"x"),
            final_chunk_for_file: false,
        };
        assert!(matches!(r.accept(gap), Err(TransferError::Codec(_))));
    }

    #[test]
    fn rejects_overrun_past_declared_size() {
        let mut r = TransferReassembler::new(&manifest());
        let overrun = DecodedChunk {
            file_index: 2,
            offset: 0,
            bytes: Bytes::from(vec![0u8; 4096]), // declared 1536
            final_chunk_for_file: true,
        };
        assert!(matches!(
            r.accept(overrun),
            Err(TransferError::TooLarge { .. })
        ));
    }

    #[test]
    fn rejects_short_file_on_final_chunk() {
        let mut r = TransferReassembler::new(&manifest());
        let short = DecodedChunk {
            file_index: 1,
            offset: 0,
            bytes: Bytes::from(vec![b'a'; 10]), // declared 2048
            final_chunk_for_file: true,
        };
        assert!(matches!(r.accept(short), Err(TransferError::Codec(_))));
    }
}
