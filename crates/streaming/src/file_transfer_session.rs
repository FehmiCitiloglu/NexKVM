//! Chunk streaming + resume for large file transfers.

use bytes::{Buf, BufMut, Bytes, BytesMut};

use crate::file_transfer_cipher::TransferCipher;
use crate::file_transfer_compression::{self, TransferCompression, TransferCompressionPolicy};
use crate::file_transfer_types::{TransferFileData, TransferId};
use crate::{MAX_TRANSFER_CHUNK_SIZE, TransferError};

const CHUNK_HEADER: usize = 16 + 4 + 8 + 4 + 1 + 1 + 4;
const FLAG_FINAL_CHUNK: u8 = 0b0000_0001;

/// Resume point for an interrupted transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferCheckpoint {
    /// Transfer id.
    pub id: TransferId,
    /// Current file index in manifest.
    pub file_index: u32,
    /// Next byte offset inside current file.
    pub offset: u64,
    /// Total bytes transferred over all files.
    pub transferred_bytes: u64,
}

/// One encrypted/compressed chunk on the file transfer stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferChunk {
    /// Transfer id.
    pub transfer_id: TransferId,
    /// Index of file entry in the manifest.
    pub file_index: u32,
    /// Offset of plaintext bytes in that file.
    pub offset: u64,
    /// Plaintext payload length before compression.
    pub plain_len: u32,
    /// Compression algorithm on ciphertext plaintext.
    pub compression: TransferCompression,
    /// True when this is the final chunk for this file.
    pub final_chunk_for_file: bool,
    /// Encrypted payload bytes.
    pub payload: Bytes,
}

impl TransferChunk {
    /// Encode chunk into wire bytes.
    ///
    /// # Errors
    /// Returns [`TransferError::TooLarge`] if payload exceeds `u32`.
    pub fn encode(&self) -> Result<Bytes, TransferError> {
        if self.plain_len as usize > MAX_TRANSFER_CHUNK_SIZE {
            return Err(TransferError::TooLarge {
                size: self.plain_len as usize,
                limit: MAX_TRANSFER_CHUNK_SIZE,
            });
        }
        let payload_len =
            u32::try_from(self.payload.len()).map_err(|_| TransferError::TooLarge {
                size: self.payload.len(),
                limit: u32::MAX as usize,
            })?;
        let mut out = BytesMut::with_capacity(CHUNK_HEADER + self.payload.len());
        out.put_slice(self.transfer_id.0.as_bytes());
        out.put_u32(self.file_index);
        out.put_u64(self.offset);
        out.put_u32(self.plain_len);
        out.put_u8(self.compression.as_u8());
        out.put_u8(if self.final_chunk_for_file {
            FLAG_FINAL_CHUNK
        } else {
            0
        });
        out.put_u32(payload_len);
        out.put_slice(&self.payload);
        Ok(out.freeze())
    }

    /// Decode chunk from wire bytes.
    ///
    /// # Errors
    /// Returns [`TransferError::Codec`] on malformed input.
    pub fn decode(mut buf: Bytes) -> Result<Self, TransferError> {
        if buf.remaining() < CHUNK_HEADER {
            return Err(TransferError::Codec("truncated chunk".into()));
        }

        let mut id = [0u8; 16];
        buf.copy_to_slice(&mut id);
        let file_index = buf.get_u32();
        let offset = buf.get_u64();
        let plain_len = buf.get_u32();
        if plain_len as usize > MAX_TRANSFER_CHUNK_SIZE {
            return Err(TransferError::TooLarge {
                size: plain_len as usize,
                limit: MAX_TRANSFER_CHUNK_SIZE,
            });
        }
        let compression = TransferCompression::from_u8(buf.get_u8())?;
        let flags = buf.get_u8();
        if flags & !FLAG_FINAL_CHUNK != 0 {
            return Err(TransferError::Codec("unknown chunk flags".into()));
        }
        let payload_len = buf.get_u32() as usize;
        if payload_len != buf.remaining() {
            return Err(TransferError::Codec("payload length mismatch".into()));
        }

        Ok(Self {
            transfer_id: TransferId(uuid::Uuid::from_bytes(id)),
            file_index,
            offset,
            plain_len,
            compression,
            final_chunk_for_file: (flags & FLAG_FINAL_CHUNK) != 0,
            payload: buf,
        })
    }
}

/// Produces outbound chunks and checkpoints from in-memory file buffers.
#[derive(Debug)]
pub struct TransferSender {
    id: TransferId,
    files: Vec<TransferFileData>,
    cursor_file: usize,
    cursor_offset: usize,
    chunk_size: usize,
    compression: TransferCompressionPolicy,
    cipher: Box<dyn TransferCipher>,
    transferred_bytes: u64,
}

impl TransferSender {
    /// Create a sender at offset zero.
    ///
    /// # Errors
    /// Returns [`TransferError::InvalidChunkSize`] unless `chunk_size` is in
    /// `1..=`[`MAX_TRANSFER_CHUNK_SIZE`].
    pub fn new(
        id: TransferId,
        files: Vec<TransferFileData>,
        chunk_size: usize,
        compression: TransferCompressionPolicy,
        cipher: Box<dyn TransferCipher>,
    ) -> Result<Self, TransferError> {
        validate_chunk_size(chunk_size)?;
        Ok(Self {
            id,
            files,
            cursor_file: 0,
            cursor_offset: 0,
            chunk_size,
            compression,
            cipher,
            transferred_bytes: 0,
        })
    }

    /// Resume sender from an existing checkpoint.
    ///
    /// `checkpoint.file_index` is an index in the transfer manifest, not an
    /// index in the file-only `files` vector. A checkpoint at the exact end of
    /// a file resumes from the following file.
    ///
    /// # Errors
    /// Returns [`TransferError::InvalidChunkSize`] for an unsupported chunk
    /// size or [`TransferError::Codec`] for an unknown/out-of-range checkpoint.
    pub fn from_checkpoint(
        checkpoint: TransferCheckpoint,
        files: Vec<TransferFileData>,
        chunk_size: usize,
        compression: TransferCompressionPolicy,
        cipher: Box<dyn TransferCipher>,
    ) -> Result<Self, TransferError> {
        validate_chunk_size(chunk_size)?;
        let file_position = files
            .iter()
            .position(|file| file.entry_index == checkpoint.file_index)
            .ok_or_else(|| {
                TransferError::Codec(format!(
                    "checkpoint references unknown manifest file index {}",
                    checkpoint.file_index
                ))
            })?;
        let offset = usize::try_from(checkpoint.offset)
            .map_err(|_| TransferError::Codec("checkpoint offset does not fit usize".into()))?;
        let file_len = files[file_position].bytes.len();
        if offset > file_len {
            return Err(TransferError::Codec(
                "checkpoint offset exceeds file length".into(),
            ));
        }
        let (cursor_file, cursor_offset) = if offset == file_len {
            (file_position + 1, 0)
        } else {
            (file_position, offset)
        };

        Ok(Self {
            id: checkpoint.id,
            files,
            cursor_file,
            cursor_offset,
            chunk_size,
            compression,
            cipher,
            transferred_bytes: checkpoint.transferred_bytes,
        })
    }

    /// Snapshot current resume point.
    #[must_use]
    pub fn checkpoint(&self) -> TransferCheckpoint {
        let (file_index, offset) = self.files.get(self.cursor_file).map_or_else(
            || {
                self.files
                    .last()
                    .map_or((0, 0), |file| (file.entry_index, file.bytes.len() as u64))
            },
            |file| (file.entry_index, self.cursor_offset as u64),
        );
        TransferCheckpoint {
            id: self.id,
            file_index,
            offset,
            transferred_bytes: self.transferred_bytes,
        }
    }

    /// Produce next chunk, or `None` when all files are complete.
    ///
    /// # Errors
    /// Returns [`TransferError`] for compression/encryption failures.
    pub fn next_chunk(&mut self) -> Result<Option<TransferChunk>, TransferError> {
        while self.cursor_file < self.files.len() {
            let file = &self.files[self.cursor_file];
            let bytes = &file.bytes;

            if bytes.is_empty() && self.cursor_offset == 0 {
                let (compression, compressed) =
                    file_transfer_compression::compress_with_policy(self.compression, bytes)?;
                let sealed = self.cipher.seal(&compressed)?;
                let chunk = TransferChunk {
                    transfer_id: self.id,
                    file_index: file.entry_index,
                    offset: 0,
                    plain_len: 0,
                    compression,
                    final_chunk_for_file: true,
                    payload: Bytes::from(sealed),
                };
                self.cursor_file += 1;
                return Ok(Some(chunk));
            }

            if self.cursor_offset >= bytes.len() {
                self.cursor_file += 1;
                self.cursor_offset = 0;
                continue;
            }

            let end = self
                .cursor_offset
                .saturating_add(self.chunk_size)
                .min(bytes.len());
            let plain = &bytes[self.cursor_offset..end];
            let (compression, compressed) =
                file_transfer_compression::compress_with_policy(self.compression, plain)?;
            let sealed = self.cipher.seal(&compressed)?;

            let chunk = TransferChunk {
                transfer_id: self.id,
                file_index: file.entry_index,
                offset: self.cursor_offset as u64,
                plain_len: u32::try_from(plain.len()).map_err(|_| TransferError::TooLarge {
                    size: plain.len(),
                    limit: u32::MAX as usize,
                })?,
                compression,
                final_chunk_for_file: end == bytes.len(),
                payload: Bytes::from(sealed),
            };

            self.cursor_offset = end;
            self.transferred_bytes = self.transferred_bytes.saturating_add(plain.len() as u64);
            return Ok(Some(chunk));
        }
        Ok(None)
    }
}

/// Decoded inbound chunk payload.
#[derive(Debug, Clone)]
pub struct DecodedChunk {
    /// File index in manifest.
    pub file_index: u32,
    /// Byte offset in file.
    pub offset: u64,
    /// Plain chunk bytes.
    pub bytes: Bytes,
    /// True if this finishes the file.
    pub final_chunk_for_file: bool,
}

/// Inbound side that opens + decompresses chunks and tracks resume state.
#[derive(Debug)]
pub struct TransferReceiver {
    checkpoint: Option<TransferCheckpoint>,
    cipher: Box<dyn TransferCipher>,
}

impl TransferReceiver {
    /// Create receiver.
    #[must_use]
    pub fn new(cipher: Box<dyn TransferCipher>) -> Self {
        Self {
            checkpoint: None,
            cipher,
        }
    }

    /// Current checkpoint after latest accepted chunk.
    #[must_use]
    pub fn checkpoint(&self) -> Option<TransferCheckpoint> {
        self.checkpoint
    }

    /// Accept one inbound chunk.
    ///
    /// # Errors
    /// Returns [`TransferError`] on decryption/decompression/corrupt payload.
    pub fn accept(&mut self, chunk: TransferChunk) -> Result<DecodedChunk, TransferError> {
        let declared_len = chunk.plain_len as usize;
        if declared_len > MAX_TRANSFER_CHUNK_SIZE {
            return Err(TransferError::TooLarge {
                size: declared_len,
                limit: MAX_TRANSFER_CHUNK_SIZE,
            });
        }
        let compressed = self.cipher.open(&chunk.payload)?;
        let plain = file_transfer_compression::decompress_bounded(
            chunk.compression,
            &compressed,
            declared_len,
        )?;
        if plain.len() != declared_len {
            return Err(TransferError::Codec("plain length mismatch".into()));
        }

        let plain_len = plain.len() as u64;
        let transferred_bytes = self
            .checkpoint
            .map_or(Some(plain_len), |c| {
                c.transferred_bytes.checked_add(plain_len)
            })
            .ok_or_else(|| TransferError::Codec("transferred byte count overflow".into()))?;
        let next_offset = chunk
            .offset
            .checked_add(plain_len)
            .ok_or_else(|| TransferError::Codec("chunk offset overflow".into()))?;
        self.checkpoint = Some(TransferCheckpoint {
            id: chunk.transfer_id,
            file_index: chunk.file_index,
            offset: next_offset,
            transferred_bytes,
        });

        Ok(DecodedChunk {
            file_index: chunk.file_index,
            offset: chunk.offset,
            bytes: Bytes::from(plain),
            final_chunk_for_file: chunk.final_chunk_for_file,
        })
    }
}

pub(crate) fn validate_chunk_size(chunk_size: usize) -> Result<(), TransferError> {
    if chunk_size == 0 || chunk_size > MAX_TRANSFER_CHUNK_SIZE {
        return Err(TransferError::InvalidChunkSize {
            size: chunk_size,
            max: MAX_TRANSFER_CHUNK_SIZE,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_transfer_cipher::PlaintextTransferCipher;
    #[cfg(feature = "transfer-compression")]
    use crate::file_transfer_compression::compress;

    fn sample_files() -> Vec<TransferFileData> {
        vec![
            TransferFileData {
                entry_index: 0,
                bytes: Bytes::from(vec![b'a'; 2048]),
            },
            TransferFileData {
                entry_index: 1,
                bytes: Bytes::from(vec![b'b'; 1536]),
            },
        ]
    }

    #[test]
    fn sender_receiver_round_trip_with_resume_checkpoint() {
        let id = TransferId::generate();
        let mut sender = TransferSender::new(
            id,
            sample_files(),
            1024,
            TransferCompressionPolicy::default(),
            Box::new(PlaintextTransferCipher),
        )
        .unwrap();
        let mut receiver = TransferReceiver::new(Box::new(PlaintextTransferCipher));

        // First chunk then interrupt.
        let first = sender.next_chunk().unwrap().unwrap();
        let wire_first = first.encode().unwrap();
        let parsed = TransferChunk::decode(wire_first).unwrap();
        let d1 = receiver.accept(parsed).unwrap();
        assert_eq!(d1.offset, 0);

        let checkpoint = sender.checkpoint();

        // Resume sender from checkpoint and drain all remaining chunks.
        let mut resumed = TransferSender::from_checkpoint(
            checkpoint,
            sample_files(),
            1024,
            TransferCompressionPolicy::default(),
            Box::new(PlaintextTransferCipher),
        )
        .unwrap();

        let mut total = d1.bytes.len() as u64;
        while let Some(chunk) = resumed.next_chunk().unwrap() {
            let parsed = TransferChunk::decode(chunk.encode().unwrap()).unwrap();
            let d = receiver.accept(parsed).unwrap();
            total += d.bytes.len() as u64;
        }

        assert_eq!(total, 2048 + 1536);
        let cp = receiver.checkpoint().unwrap();
        assert_eq!(cp.transferred_bytes, total);
    }

    #[cfg(feature = "transfer-compression")]
    #[test]
    fn receiver_bounds_decompression_by_declared_plain_length() {
        let plain = vec![b'z'; 4096];
        let compressed = compress(TransferCompression::Deflate, &plain).unwrap();
        assert!(compressed.len() < 1024, "fixture must be highly compressed");
        let chunk = TransferChunk {
            transfer_id: TransferId::generate(),
            file_index: 0,
            offset: 0,
            plain_len: 1024,
            compression: TransferCompression::Deflate,
            final_chunk_for_file: true,
            payload: Bytes::from(compressed),
        };
        let mut receiver = TransferReceiver::new(Box::new(PlaintextTransferCipher));

        assert!(matches!(
            receiver.accept(chunk),
            Err(TransferError::TooLarge {
                size: 1025,
                limit: 1024
            })
        ));
        assert!(receiver.checkpoint().is_none());
    }

    #[test]
    fn sender_rejects_zero_chunk_size() {
        assert!(matches!(
            TransferSender::new(
                TransferId::generate(),
                sample_files(),
                0,
                TransferCompressionPolicy::default(),
                Box::new(PlaintextTransferCipher),
            ),
            Err(TransferError::InvalidChunkSize { size: 0, .. })
        ));
    }
}
