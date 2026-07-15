//! Versioned, bounded wire codec for authenticated file-transfer messages.
//!
//! This codec intentionally does not add another encryption layer. Callers
//! must carry the encoded bytes inside the authenticated [`Connection`](https://docs.rs/nexkvm-network)
//! session used by the rest of NexKVM.

use std::str;

use bytes::{Buf, BufMut, Bytes, BytesMut};
use nexkvm_core::DeviceId;
use uuid::Uuid;

use crate::file_transfer_session::{TransferCheckpoint, TransferChunk};
use crate::file_transfer_types::{
    TransferEntry, TransferId, TransferManifest, TransferSource, validate_manifest_entries,
};
use crate::{
    FILE_TRANSFER_SHA256_BYTES, FILE_TRANSFER_WIRE_VERSION, MAX_FILE_TRANSFER_WIRE_BYTES,
    MAX_TRANSFER_CHUNK_SIZE, MAX_TRANSFER_MANIFEST_ENTRIES, MAX_TRANSFER_PATH_BYTES,
    MAX_TRANSFER_TOTAL_BYTES, TransferError,
};

const MAGIC: &[u8; 4] = b"NXFT";
const HEADER_LEN: usize = 4 + 2 + 1 + 1 + 4;
const RESERVED_FLAGS: u8 = 0;
const CHECKPOINT_LEN: usize = 16 + 4 + 8 + 8;
const MAX_REASON_BYTES: usize = 1024;
const MAX_CHUNK_PAYLOAD_BYTES: usize = MAX_TRANSFER_CHUNK_SIZE + 64 * 1024;

const TAG_OFFER: u8 = 1;
const TAG_ACCEPT: u8 = 2;
const TAG_REJECT: u8 = 3;
const TAG_CHUNK: u8 = 4;
const TAG_CHECKPOINT: u8 = 5;
const TAG_ACK: u8 = 6;
const TAG_COMPLETE: u8 = 7;
const TAG_CANCEL: u8 = 8;

/// One message on the file-transfer control/data lane.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FileTransferMessage {
    /// Offer a validated file/folder manifest to a peer.
    Offer(TransferManifest),
    /// Accept an offer, optionally asking the sender to resume at a checkpoint.
    Accept {
        /// Accepted transfer.
        transfer_id: TransferId,
        /// Previously persisted receive checkpoint.
        checkpoint: Option<TransferCheckpoint>,
    },
    /// Reject an offer.
    Reject {
        /// Rejected transfer.
        transfer_id: TransferId,
        /// Bounded human-readable reason.
        reason: String,
    },
    /// One bounded data chunk.
    Chunk(TransferChunk),
    /// Receiver persistence checkpoint.
    Checkpoint(TransferCheckpoint),
    /// Sender acknowledgement of a checkpoint.
    Ack(TransferCheckpoint),
    /// All expected bytes were transferred.
    Complete {
        /// Completed transfer.
        transfer_id: TransferId,
        /// Logical bytes completed.
        transferred_bytes: u64,
    },
    /// Cancel an in-progress transfer.
    Cancel {
        /// Canceled transfer.
        transfer_id: TransferId,
        /// Bounded human-readable reason.
        reason: String,
    },
}

impl FileTransferMessage {
    /// Encode a message with magic, version, tag, reserved flags, and body size.
    ///
    /// # Errors
    /// Returns [`TransferError`] when any field violates the production wire
    /// limits or the full message would exceed its authenticated frame budget.
    pub fn encode(&self) -> Result<Bytes, TransferError> {
        let (tag, body) = match self {
            Self::Offer(manifest) => (TAG_OFFER, TransferManifestCodec::encode(manifest)?),
            Self::Accept {
                transfer_id,
                checkpoint,
            } => (TAG_ACCEPT, encode_accept(*transfer_id, *checkpoint)?),
            Self::Reject {
                transfer_id,
                reason,
            } => (TAG_REJECT, encode_reason_message(*transfer_id, reason)?),
            Self::Chunk(chunk) => {
                validate_chunk(chunk)?;
                (TAG_CHUNK, chunk.encode()?)
            }
            Self::Checkpoint(checkpoint) => {
                validate_checkpoint(checkpoint)?;
                (TAG_CHECKPOINT, encode_checkpoint(*checkpoint))
            }
            Self::Ack(checkpoint) => {
                validate_checkpoint(checkpoint)?;
                (TAG_ACK, encode_checkpoint(*checkpoint))
            }
            Self::Complete {
                transfer_id,
                transferred_bytes,
            } => (
                TAG_COMPLETE,
                encode_complete(*transfer_id, *transferred_bytes)?,
            ),
            Self::Cancel {
                transfer_id,
                reason,
            } => (TAG_CANCEL, encode_reason_message(*transfer_id, reason)?),
        };

        let max_body = MAX_FILE_TRANSFER_WIRE_BYTES - HEADER_LEN;
        if body.len() > max_body {
            return Err(TransferError::TooLarge {
                size: body.len(),
                limit: max_body,
            });
        }
        let body_len = u32::try_from(body.len()).map_err(|_| TransferError::TooLarge {
            size: body.len(),
            limit: max_body,
        })?;
        let mut out = BytesMut::with_capacity(HEADER_LEN + body.len());
        out.put_slice(MAGIC);
        out.put_u16(FILE_TRANSFER_WIRE_VERSION);
        out.put_u8(tag);
        out.put_u8(RESERVED_FLAGS);
        out.put_u32(body_len);
        out.put_slice(&body);
        Ok(out.freeze())
    }

    /// Decode one complete authenticated file-transfer message.
    ///
    /// Lengths and counts are checked before allocation. The decoder rejects
    /// unknown versions/tags/flags, truncation, trailing bytes, unsafe paths,
    /// and inconsistent totals.
    ///
    /// # Errors
    /// Returns [`TransferError`] for malformed or out-of-policy peer input.
    pub fn decode(input: Bytes) -> Result<Self, TransferError> {
        if input.len() > MAX_FILE_TRANSFER_WIRE_BYTES {
            return Err(TransferError::TooLarge {
                size: input.len(),
                limit: MAX_FILE_TRANSFER_WIRE_BYTES,
            });
        }
        let mut decoder = Decoder::new(input);
        if decoder.take(4)?.as_ref() != MAGIC {
            return Err(TransferError::Codec("invalid file-transfer magic".into()));
        }
        let version = decoder.u16()?;
        if version == 1 {
            return Err(TransferError::Codec(
                "legacy file-transfer wire version 1 lacks required file digests".into(),
            ));
        }
        if version != FILE_TRANSFER_WIRE_VERSION {
            return Err(TransferError::Codec(format!(
                "unsupported file-transfer wire version {version}"
            )));
        }
        let tag = decoder.u8()?;
        if decoder.u8()? != RESERVED_FLAGS {
            return Err(TransferError::Codec(
                "non-zero file-transfer reserved flags".into(),
            ));
        }
        let body_len = decoder.u32()? as usize;
        let max_body = MAX_FILE_TRANSFER_WIRE_BYTES - HEADER_LEN;
        if body_len > max_body {
            return Err(TransferError::TooLarge {
                size: body_len,
                limit: max_body,
            });
        }
        if decoder.remaining() != body_len {
            return Err(TransferError::Codec(
                "file-transfer body length mismatch or trailing bytes".into(),
            ));
        }
        let body = decoder.take(body_len)?;
        decoder.finish()?;

        match tag {
            TAG_OFFER => TransferManifestCodec::decode(body).map(Self::Offer),
            TAG_ACCEPT => decode_accept(body),
            TAG_REJECT => decode_reason_message(body, false),
            TAG_CHUNK => {
                let chunk = TransferChunk::decode(body)?;
                validate_chunk(&chunk)?;
                Ok(Self::Chunk(chunk))
            }
            TAG_CHECKPOINT => decode_checkpoint_message(body, false),
            TAG_ACK => decode_checkpoint_message(body, true),
            TAG_COMPLETE => decode_complete(body),
            TAG_CANCEL => decode_reason_message(body, true),
            _ => Err(TransferError::Codec(format!(
                "unknown file-transfer message tag {tag}"
            ))),
        }
    }
}

/// Strict codec for the manifest body used by [`FileTransferMessage::Offer`].
#[derive(Debug, Clone, Copy, Default)]
pub struct TransferManifestCodec;

impl TransferManifestCodec {
    /// Encode a manifest after revalidating all public fields.
    ///
    /// # Errors
    /// Returns [`TransferError`] for unsafe paths, duplicate entries,
    /// inconsistent totals, or bounded length/count violations.
    pub fn encode(manifest: &TransferManifest) -> Result<Bytes, TransferError> {
        let computed_total = validate_manifest_entries(&manifest.entries)?;
        if computed_total != manifest.total_bytes {
            return Err(TransferError::Codec(
                "manifest declared total does not match entries".into(),
            ));
        }

        let mut encoded_len = 16usize + 16 + 1 + 1 + 8 + 4;
        if manifest.to.is_some() {
            encoded_len = encoded_len
                .checked_add(16)
                .ok_or_else(|| TransferError::Codec("manifest encoded length overflow".into()))?;
        }
        for entry in &manifest.entries {
            encoded_len = encoded_len
                .checked_add(1 + 2 + 8)
                .and_then(|len| {
                    len.checked_add(if entry.is_dir {
                        0
                    } else {
                        FILE_TRANSFER_SHA256_BYTES
                    })
                })
                .and_then(|len| len.checked_add(entry.relative_path.len()))
                .ok_or_else(|| TransferError::Codec("manifest encoded length overflow".into()))?;
        }
        let limit = MAX_FILE_TRANSFER_WIRE_BYTES - HEADER_LEN;
        if encoded_len > limit {
            return Err(TransferError::TooLarge {
                size: encoded_len,
                limit,
            });
        }

        let entry_count =
            u32::try_from(manifest.entries.len()).map_err(|_| TransferError::TooLarge {
                size: manifest.entries.len(),
                limit: MAX_TRANSFER_MANIFEST_ENTRIES,
            })?;
        let mut out = BytesMut::with_capacity(encoded_len);
        put_transfer_id(&mut out, manifest.id);
        put_device_id(&mut out, manifest.from);
        match manifest.to {
            Some(target) => {
                out.put_u8(1);
                put_device_id(&mut out, target);
            }
            None => out.put_u8(0),
        }
        out.put_u8(encode_source(manifest.source));
        out.put_u64(manifest.total_bytes);
        out.put_u32(entry_count);
        for entry in &manifest.entries {
            out.put_u8(u8::from(entry.is_dir));
            out.put_u16(u16::try_from(entry.relative_path.len()).map_err(|_| {
                TransferError::TooLarge {
                    size: entry.relative_path.len(),
                    limit: MAX_TRANSFER_PATH_BYTES,
                }
            })?);
            out.put_u64(entry.size_bytes);
            if !entry.is_dir {
                let digest = entry.sha256.ok_or_else(|| {
                    TransferError::Codec("file entry is missing its SHA-256 digest".into())
                })?;
                out.put_slice(&digest);
            }
            out.put_slice(entry.relative_path.as_bytes());
        }
        Ok(out.freeze())
    }

    /// Decode a complete manifest body with allocation limits applied first.
    ///
    /// # Errors
    /// Returns [`TransferError`] for malformed fields, trailing bytes, unsafe
    /// paths, inconsistent totals, or bounded length/count violations.
    pub fn decode(input: Bytes) -> Result<TransferManifest, TransferError> {
        if input.len() > MAX_FILE_TRANSFER_WIRE_BYTES - HEADER_LEN {
            return Err(TransferError::TooLarge {
                size: input.len(),
                limit: MAX_FILE_TRANSFER_WIRE_BYTES - HEADER_LEN,
            });
        }
        let mut decoder = Decoder::new(input);
        let id = decoder.transfer_id()?;
        let from = decoder.device_id()?;
        let to = match decoder.u8()? {
            0 => None,
            1 => Some(decoder.device_id()?),
            other => {
                return Err(TransferError::Codec(format!(
                    "invalid manifest target marker {other}"
                )));
            }
        };
        let source = decode_source(decoder.u8()?)?;
        let declared_total = decoder.u64()?;
        enforce_total_limit(declared_total)?;
        let entry_count = decoder.u32()? as usize;
        if entry_count > MAX_TRANSFER_MANIFEST_ENTRIES {
            return Err(TransferError::TooLarge {
                size: entry_count,
                limit: MAX_TRANSFER_MANIFEST_ENTRIES,
            });
        }
        if entry_count == 0 {
            return Err(TransferError::EmptyManifest);
        }

        let mut entries = Vec::with_capacity(entry_count);
        for _ in 0..entry_count {
            let is_dir = match decoder.u8()? {
                0 => false,
                1 => true,
                other => {
                    return Err(TransferError::Codec(format!(
                        "invalid manifest entry kind {other}"
                    )));
                }
            };
            let path_len = decoder.u16()? as usize;
            if path_len > MAX_TRANSFER_PATH_BYTES {
                return Err(TransferError::TooLarge {
                    size: path_len,
                    limit: MAX_TRANSFER_PATH_BYTES,
                });
            }
            if path_len == 0 {
                return Err(TransferError::InvalidPath(String::new()));
            }
            let size_bytes = decoder.u64()?;
            let sha256 = if is_dir {
                None
            } else {
                let bytes = decoder.take(FILE_TRANSFER_SHA256_BYTES)?;
                let mut digest = [0u8; FILE_TRANSFER_SHA256_BYTES];
                digest.copy_from_slice(&bytes);
                Some(digest)
            };
            let path_bytes = decoder.take(path_len)?;
            let path = str::from_utf8(&path_bytes)
                .map_err(|_| TransferError::Codec("manifest path is not UTF-8".into()))?
                .to_owned();
            let entry = if is_dir {
                if size_bytes != 0 {
                    return Err(TransferError::Codec(
                        "directory entry has non-zero size".into(),
                    ));
                }
                TransferEntry::dir(path)?
            } else {
                TransferEntry::file(
                    path,
                    size_bytes,
                    sha256.ok_or_else(|| {
                        TransferError::Codec("file entry is missing its SHA-256 digest".into())
                    })?,
                )?
            };
            entries.push(entry);
        }
        decoder.finish()?;

        let manifest = TransferManifest::new(id, from, to, source, entries)?;
        if manifest.total_bytes != declared_total {
            return Err(TransferError::Codec(
                "manifest declared total does not match entries".into(),
            ));
        }
        Ok(manifest)
    }
}

fn encode_accept(
    transfer_id: TransferId,
    checkpoint: Option<TransferCheckpoint>,
) -> Result<Bytes, TransferError> {
    if let Some(checkpoint) = checkpoint {
        validate_checkpoint(&checkpoint)?;
        if checkpoint.id != transfer_id {
            return Err(TransferError::Codec(
                "accept checkpoint transfer id mismatch".into(),
            ));
        }
    }
    let mut out = BytesMut::with_capacity(16 + 1 + checkpoint.map_or(0, |_| CHECKPOINT_LEN));
    put_transfer_id(&mut out, transfer_id);
    match checkpoint {
        Some(checkpoint) => {
            out.put_u8(1);
            put_checkpoint(&mut out, checkpoint);
        }
        None => out.put_u8(0),
    }
    Ok(out.freeze())
}

fn decode_accept(body: Bytes) -> Result<FileTransferMessage, TransferError> {
    let mut decoder = Decoder::new(body);
    let transfer_id = decoder.transfer_id()?;
    let checkpoint = match decoder.u8()? {
        0 => None,
        1 => {
            let checkpoint = decoder.checkpoint()?;
            validate_checkpoint(&checkpoint)?;
            if checkpoint.id != transfer_id {
                return Err(TransferError::Codec(
                    "accept checkpoint transfer id mismatch".into(),
                ));
            }
            Some(checkpoint)
        }
        other => {
            return Err(TransferError::Codec(format!(
                "invalid accept checkpoint marker {other}"
            )));
        }
    };
    decoder.finish()?;
    Ok(FileTransferMessage::Accept {
        transfer_id,
        checkpoint,
    })
}

fn encode_reason_message(transfer_id: TransferId, reason: &str) -> Result<Bytes, TransferError> {
    if reason.len() > MAX_REASON_BYTES {
        return Err(TransferError::TooLarge {
            size: reason.len(),
            limit: MAX_REASON_BYTES,
        });
    }
    let reason_len = u16::try_from(reason.len()).map_err(|_| TransferError::TooLarge {
        size: reason.len(),
        limit: MAX_REASON_BYTES,
    })?;
    let mut out = BytesMut::with_capacity(16 + 2 + reason.len());
    put_transfer_id(&mut out, transfer_id);
    out.put_u16(reason_len);
    out.put_slice(reason.as_bytes());
    Ok(out.freeze())
}

fn decode_reason_message(body: Bytes, cancel: bool) -> Result<FileTransferMessage, TransferError> {
    let mut decoder = Decoder::new(body);
    let transfer_id = decoder.transfer_id()?;
    let reason_len = decoder.u16()? as usize;
    if reason_len > MAX_REASON_BYTES {
        return Err(TransferError::TooLarge {
            size: reason_len,
            limit: MAX_REASON_BYTES,
        });
    }
    let reason_bytes = decoder.take(reason_len)?;
    let reason = str::from_utf8(&reason_bytes)
        .map_err(|_| TransferError::Codec("transfer reason is not UTF-8".into()))?
        .to_owned();
    decoder.finish()?;
    if cancel {
        Ok(FileTransferMessage::Cancel {
            transfer_id,
            reason,
        })
    } else {
        Ok(FileTransferMessage::Reject {
            transfer_id,
            reason,
        })
    }
}

fn encode_checkpoint(checkpoint: TransferCheckpoint) -> Bytes {
    let mut out = BytesMut::with_capacity(CHECKPOINT_LEN);
    put_checkpoint(&mut out, checkpoint);
    out.freeze()
}

fn decode_checkpoint_message(body: Bytes, ack: bool) -> Result<FileTransferMessage, TransferError> {
    let mut decoder = Decoder::new(body);
    let checkpoint = decoder.checkpoint()?;
    decoder.finish()?;
    validate_checkpoint(&checkpoint)?;
    if ack {
        Ok(FileTransferMessage::Ack(checkpoint))
    } else {
        Ok(FileTransferMessage::Checkpoint(checkpoint))
    }
}

fn encode_complete(
    transfer_id: TransferId,
    transferred_bytes: u64,
) -> Result<Bytes, TransferError> {
    enforce_total_limit(transferred_bytes)?;
    let mut out = BytesMut::with_capacity(16 + 8);
    put_transfer_id(&mut out, transfer_id);
    out.put_u64(transferred_bytes);
    Ok(out.freeze())
}

fn decode_complete(body: Bytes) -> Result<FileTransferMessage, TransferError> {
    let mut decoder = Decoder::new(body);
    let transfer_id = decoder.transfer_id()?;
    let transferred_bytes = decoder.u64()?;
    decoder.finish()?;
    enforce_total_limit(transferred_bytes)?;
    Ok(FileTransferMessage::Complete {
        transfer_id,
        transferred_bytes,
    })
}

fn validate_checkpoint(checkpoint: &TransferCheckpoint) -> Result<(), TransferError> {
    if checkpoint.file_index as usize >= MAX_TRANSFER_MANIFEST_ENTRIES {
        return Err(TransferError::TooLarge {
            size: checkpoint.file_index as usize,
            limit: MAX_TRANSFER_MANIFEST_ENTRIES - 1,
        });
    }
    enforce_total_limit(checkpoint.offset)?;
    enforce_total_limit(checkpoint.transferred_bytes)?;
    if checkpoint.transferred_bytes < checkpoint.offset {
        return Err(TransferError::Codec(
            "checkpoint transferred bytes precede file offset".into(),
        ));
    }
    Ok(())
}

fn validate_chunk(chunk: &TransferChunk) -> Result<(), TransferError> {
    if chunk.file_index as usize >= MAX_TRANSFER_MANIFEST_ENTRIES {
        return Err(TransferError::TooLarge {
            size: chunk.file_index as usize,
            limit: MAX_TRANSFER_MANIFEST_ENTRIES - 1,
        });
    }
    if chunk.payload.len() > MAX_CHUNK_PAYLOAD_BYTES {
        return Err(TransferError::TooLarge {
            size: chunk.payload.len(),
            limit: MAX_CHUNK_PAYLOAD_BYTES,
        });
    }
    let end = chunk
        .offset
        .checked_add(u64::from(chunk.plain_len))
        .ok_or_else(|| TransferError::Codec("chunk offset overflow".into()))?;
    enforce_total_limit(end)
}

fn enforce_total_limit(value: u64) -> Result<(), TransferError> {
    if value > MAX_TRANSFER_TOTAL_BYTES {
        return Err(TransferError::TooLarge {
            size: usize::try_from(value).unwrap_or(usize::MAX),
            limit: usize::try_from(MAX_TRANSFER_TOTAL_BYTES).unwrap_or(usize::MAX),
        });
    }
    Ok(())
}

fn encode_source(source: TransferSource) -> u8 {
    match source {
        TransferSource::DragDrop => 0,
        TransferSource::Picker => 1,
        TransferSource::Sync => 2,
    }
}

fn decode_source(value: u8) -> Result<TransferSource, TransferError> {
    match value {
        0 => Ok(TransferSource::DragDrop),
        1 => Ok(TransferSource::Picker),
        2 => Ok(TransferSource::Sync),
        _ => Err(TransferError::Codec(format!(
            "unknown transfer source {value}"
        ))),
    }
}

fn put_transfer_id(out: &mut BytesMut, id: TransferId) {
    out.put_slice(id.0.as_bytes());
}

fn put_device_id(out: &mut BytesMut, id: DeviceId) {
    out.put_slice(id.0.as_bytes());
}

fn put_checkpoint(out: &mut BytesMut, checkpoint: TransferCheckpoint) {
    put_transfer_id(out, checkpoint.id);
    out.put_u32(checkpoint.file_index);
    out.put_u64(checkpoint.offset);
    out.put_u64(checkpoint.transferred_bytes);
}

struct Decoder {
    bytes: Bytes,
}

impl Decoder {
    fn new(bytes: Bytes) -> Self {
        Self { bytes }
    }

    fn remaining(&self) -> usize {
        self.bytes.remaining()
    }

    fn take(&mut self, len: usize) -> Result<Bytes, TransferError> {
        if self.remaining() < len {
            return Err(TransferError::Codec("truncated file-transfer field".into()));
        }
        Ok(self.bytes.split_to(len))
    }

    fn u8(&mut self) -> Result<u8, TransferError> {
        self.require(1)?;
        Ok(self.bytes.get_u8())
    }

    fn u16(&mut self) -> Result<u16, TransferError> {
        self.require(2)?;
        Ok(self.bytes.get_u16())
    }

    fn u32(&mut self) -> Result<u32, TransferError> {
        self.require(4)?;
        Ok(self.bytes.get_u32())
    }

    fn u64(&mut self) -> Result<u64, TransferError> {
        self.require(8)?;
        Ok(self.bytes.get_u64())
    }

    fn transfer_id(&mut self) -> Result<TransferId, TransferError> {
        Ok(TransferId(self.uuid()?))
    }

    fn device_id(&mut self) -> Result<DeviceId, TransferError> {
        Ok(DeviceId(self.uuid()?))
    }

    fn uuid(&mut self) -> Result<Uuid, TransferError> {
        let bytes = self.take(16)?;
        let mut id = [0u8; 16];
        id.copy_from_slice(&bytes);
        Ok(Uuid::from_bytes(id))
    }

    fn checkpoint(&mut self) -> Result<TransferCheckpoint, TransferError> {
        Ok(TransferCheckpoint {
            id: self.transfer_id()?,
            file_index: self.u32()?,
            offset: self.u64()?,
            transferred_bytes: self.u64()?,
        })
    }

    fn finish(self) -> Result<(), TransferError> {
        if self.bytes.is_empty() {
            Ok(())
        } else {
            Err(TransferError::Codec("trailing file-transfer bytes".into()))
        }
    }

    fn require(&self, len: usize) -> Result<(), TransferError> {
        if self.remaining() < len {
            Err(TransferError::Codec("truncated file-transfer field".into()))
        } else {
            Ok(())
        }
    }
}
