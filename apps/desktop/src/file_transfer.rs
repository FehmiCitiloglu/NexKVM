//! Trusted-peer file-transfer runtime and durable local send queue.
//!
//! Queue records are deliberately stored as one owner-only file per transfer.
//! That makes concurrent `file-send` invocations atomic without a shared queue
//! rewrite that could lose another process' entry.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, ensure};
use nexkvm_core::DeviceId;
use nexkvm_network::Connection;
use nexkvm_protocol::{Envelope, MessageId, MessageKind, PROTOCOL_VERSION};
use nexkvm_storage::FileTransferConfig;
use nexkvm_streaming::{
    FILE_TRANSFER_SHA256_BYTES, FileTransferMessage, MAX_TRANSFER_CHUNK_SIZE,
    MAX_TRANSFER_MANIFEST_ENTRIES, MAX_TRANSFER_TOTAL_BYTES, TransferCheckpoint, TransferEntry,
    TransferFileReader, TransferId, TransferManifest, TransferManifestCodec, TransferPartWriter,
    TransferSource, create_transfer_directory,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::connection::PeerConnectionHandler;

const QUEUE_SCHEMA_VERSION: u16 = 1;
const QUEUE_DIRECTORY_NAME: &str = "file-transfer-queue";
const QUEUE_RECORD_MAX_BYTES: usize = 32 * 1024 * 1024;
const RECEIVE_STATE_FILE: &str = ".nexkvm-transfer.toml";
const RECEIVE_STATE_SCHEMA_VERSION: u16 = 2;
const RECEIVE_STATE_MAX_BYTES: usize = 64 * 1024;
const QUEUE_POLL_INTERVAL: Duration = Duration::from_millis(750);
const RUNTIME_CHUNK_SIZE: usize = 1024 * 1024;
const PROTOCOL_PROGRESS_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct QueuedSource {
    absolute_path: String,
    relative_path: String,
    is_dir: bool,
    size_bytes: u64,
    sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct QueuedTransfer {
    schema_version: u16,
    transfer_id: String,
    queued_at_unix_nanos: u64,
    entries: Vec<QueuedSource>,
    total_bytes: u64,
}

#[derive(Debug)]
struct QueueRecord {
    path: PathBuf,
    transfer: QueuedTransfer,
}

#[derive(Debug)]
struct FileTransferRuntime {
    config_path: PathBuf,
    config: FileTransferConfig,
    local_device_id: DeviceId,
    active_peer: crate::ActivePeerSelection,
    active_transfer: Arc<Semaphore>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PersistedCheckpoint {
    file_index: u32,
    offset: u64,
    transferred_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ReceiveState {
    schema_version: u16,
    transfer_id: String,
    manifest_sha256: String,
    checkpoint: Option<PersistedCheckpoint>,
    complete: bool,
}

impl ReceiveState {
    fn checkpoint(&self, id: TransferId) -> Option<TransferCheckpoint> {
        self.checkpoint
            .as_ref()
            .map(|checkpoint| TransferCheckpoint {
                id,
                file_index: checkpoint.file_index,
                offset: checkpoint.offset,
                transferred_bytes: checkpoint.transferred_bytes,
            })
    }

    fn set_checkpoint(&mut self, checkpoint: TransferCheckpoint) {
        self.checkpoint = Some(PersistedCheckpoint {
            file_index: checkpoint.file_index,
            offset: checkpoint.offset,
            transferred_bytes: checkpoint.transferred_bytes,
        });
    }

    fn replace_checkpoint(&mut self, checkpoint: Option<TransferCheckpoint>) {
        self.checkpoint = checkpoint.map(|checkpoint| PersistedCheckpoint {
            file_index: checkpoint.file_index,
            offset: checkpoint.offset,
            transferred_bytes: checkpoint.transferred_bytes,
        });
    }
}

struct ReceiveSession {
    manifest: TransferManifest,
    root: PathBuf,
    state: ReceiveState,
    writer: Option<Box<HashedPartWriter>>,
}

struct HashedPartWriter {
    writer: TransferPartWriter,
    hasher: Sha256,
    expected_sha256: [u8; FILE_TRANSFER_SHA256_BYTES],
}

enum VerifiedPublication {
    Published,
    QuarantinedDigestMismatch,
}

enum ReceiveWriteOutcome {
    Partial(Box<HashedPartWriter>),
    Published,
    QuarantinedDigestMismatch,
}

impl HashedPartWriter {
    fn create(
        root: &Path,
        relative_path: &str,
        file_index: u32,
        expected_len: u64,
        expected_sha256: [u8; FILE_TRANSFER_SHA256_BYTES],
    ) -> anyhow::Result<Self> {
        Ok(Self {
            writer: TransferPartWriter::create(root, relative_path, file_index, expected_len)?,
            hasher: Sha256::new(),
            expected_sha256,
        })
    }

    fn resume(
        root: &Path,
        relative_path: &str,
        file_index: u32,
        expected_len: u64,
        offset: u64,
        expected_sha256: [u8; FILE_TRANSFER_SHA256_BYTES],
    ) -> anyhow::Result<Self> {
        let writer =
            TransferPartWriter::resume(root, relative_path, file_index, expected_len, offset)?;
        let mut file = File::open(writer.part_path())?;
        let mut hasher = Sha256::new();
        hash_reader_prefix(&mut file, offset, &mut hasher)?;
        Ok(Self {
            writer,
            hasher,
            expected_sha256,
        })
    }

    fn write_raw_chunk(&mut self, chunk: &nexkvm_streaming::TransferChunk) -> anyhow::Result<()> {
        self.writer.write_raw_chunk(chunk)?;
        self.hasher.update(&chunk.payload);
        Ok(())
    }

    fn flush_and_sync(&mut self) -> anyhow::Result<()> {
        self.writer.flush()?;
        File::open(self.writer.part_path())?.sync_all()?;
        Ok(())
    }

    fn finalize_verified(self) -> anyhow::Result<VerifiedPublication> {
        let actual_sha256: [u8; FILE_TRANSFER_SHA256_BYTES] = self.hasher.finalize().into();
        if actual_sha256 != self.expected_sha256 {
            let part_path = self.writer.part_path().to_path_buf();
            drop(self.writer);
            quarantine_file(&part_path)?;
            return Ok(VerifiedPublication::QuarantinedDigestMismatch);
        }
        self.writer.finalize()?;
        Ok(VerifiedPublication::Published)
    }
}

struct HashedTransferReader {
    reader: TransferFileReader<File>,
    hasher: Sha256,
    expected_sha256: [u8; FILE_TRANSFER_SHA256_BYTES],
}

impl HashedTransferReader {
    fn open(
        path: &Path,
        transfer_id: TransferId,
        file_index: u32,
        expected_len: u64,
        offset: u64,
        expected_sha256: [u8; FILE_TRANSFER_SHA256_BYTES],
    ) -> anyhow::Result<Self> {
        let metadata = fs::symlink_metadata(path)?;
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "queued source is no longer a regular file"
        );
        let mut file = File::open(path)?;
        let mut hasher = Sha256::new();
        hash_reader_prefix(&mut file, offset, &mut hasher)?;
        let reader = if offset == 0 {
            TransferFileReader::new(
                file,
                transfer_id,
                file_index,
                expected_len,
                RUNTIME_CHUNK_SIZE.min(MAX_TRANSFER_CHUNK_SIZE),
            )?
        } else {
            TransferFileReader::resume(
                file,
                transfer_id,
                file_index,
                expected_len,
                offset,
                RUNTIME_CHUNK_SIZE.min(MAX_TRANSFER_CHUNK_SIZE),
            )?
        };
        Ok(Self {
            reader,
            hasher,
            expected_sha256,
        })
    }

    fn next_chunk(&mut self) -> anyhow::Result<Option<nexkvm_streaming::TransferChunk>> {
        let chunk = self.reader.next_chunk()?;
        if let Some(chunk) = &chunk {
            self.hasher.update(&chunk.payload);
        }
        Ok(chunk)
    }

    fn verify(self) -> anyhow::Result<()> {
        let actual_sha256: [u8; FILE_TRANSFER_SHA256_BYTES] = self.hasher.finalize().into();
        ensure!(
            actual_sha256 == self.expected_sha256,
            "source content changed after it was queued"
        );
        Ok(())
    }
}

fn hash_reader_prefix(file: &mut File, length: u64, hasher: &mut Sha256) -> anyhow::Result<()> {
    file.seek(SeekFrom::Start(0))?;
    let mut remaining = length;
    let mut buffer = [0u8; 64 * 1024];
    while remaining > 0 {
        let wanted = usize::try_from(remaining.min(buffer.len() as u64))
            .context("hash prefix length does not fit this platform")?;
        let read = file.read(&mut buffer[..wanted])?;
        ensure!(
            read > 0,
            "source was truncated while hashing its resume prefix"
        );
        hasher.update(&buffer[..read]);
        remaining -= read as u64;
    }
    Ok(())
}

impl std::fmt::Debug for ReceiveSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReceiveSession")
            .field("transfer_id", &self.manifest.id)
            .field("checkpoint", &self.state.checkpoint)
            .field("writer_active", &self.writer.is_some())
            .finish_non_exhaustive()
    }
}

enum OutboundResult {
    Acknowledged,
    Retained,
}

/// Build the authenticated file-transfer lane when the feature is enabled.
pub(super) fn create_peer_handler(
    config_path: PathBuf,
    config: FileTransferConfig,
    local_device_id: DeviceId,
    active_peer: crate::ActivePeerSelection,
) -> Option<PeerConnectionHandler> {
    if !config.enabled {
        return None;
    }
    let runtime = Arc::new(FileTransferRuntime {
        config_path,
        config,
        local_device_id,
        active_peer,
        active_transfer: Arc::new(Semaphore::new(1)),
    });
    Some(Arc::new(move |connection, _context| {
        let Some(peer_identity) = connection.peer_identity() else {
            tracing::warn!("file-transfer lane rejected an unauthenticated connection");
            return;
        };
        if !runtime.active_peer.allows(Some(&peer_identity)) {
            tracing::debug!("file-transfer lane ignored a non-selected trusted peer");
            return;
        }
        let peer_device_id = crate::stable_device_id(&peer_identity);
        if peer_device_id == runtime.local_device_id {
            tracing::warn!("file-transfer lane rejected a self-identity connection");
            return;
        }
        let runtime = Arc::clone(&runtime);
        tokio::spawn(async move {
            if let Err(error) = run_connection(connection, runtime, peer_device_id).await {
                tracing::warn!(error = %error, "file-transfer peer session ended");
            }
        });
    }))
}

async fn run_connection(
    connection: Box<dyn Connection>,
    runtime: Arc<FileTransferRuntime>,
    peer_device_id: DeviceId,
) -> anyhow::Result<()> {
    let mut poll = tokio::time::interval(QUEUE_POLL_INTERVAL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let config_path = runtime.config_path.clone();
        let config = runtime.config.clone();
        if let Ok(permit) = Arc::clone(&runtime.active_transfer).try_acquire_owned() {
            let queued =
                tokio::task::spawn_blocking(move || load_oldest_queue_entry(&config_path, &config))
                    .await
                    .context("joining file-transfer queue scan")?;
            match queued {
                Ok(Some(record)) => {
                    let transfer_id = record.transfer.id()?;
                    let result = run_outbound(
                        connection.as_ref(),
                        &runtime,
                        peer_device_id,
                        &record,
                        permit,
                    )
                    .await;
                    match result {
                        Ok(OutboundResult::Acknowledged) => {
                            tracing::info!(transfer_id = %transfer_id.0, "file transfer acknowledged");
                        }
                        Ok(OutboundResult::Retained) => {
                            tracing::warn!(transfer_id = %transfer_id.0, "file transfer retained for retry");
                            tokio::time::sleep(QUEUE_POLL_INTERVAL).await;
                        }
                        Err(error) => {
                            tracing::warn!(
                                transfer_id = %transfer_id.0,
                                error = %error,
                                "file transfer interrupted; queue retained"
                            );
                            return Err(error);
                        }
                    }
                    continue;
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(error = %error, "file-transfer queue validation failed");
                }
            }
        }

        tokio::select! {
            received = connection.recv() => {
                let envelope = received.context("receiving idle file-transfer message")?;
                let message = decode_envelope(envelope)?;
                handle_idle_message(connection.as_ref(), &runtime, peer_device_id, message).await?;
            }
            _ = poll.tick() => {}
        }
    }
}

async fn handle_idle_message(
    connection: &dyn Connection,
    runtime: &Arc<FileTransferRuntime>,
    peer_device_id: DeviceId,
    message: FileTransferMessage,
) -> anyhow::Result<()> {
    match message {
        FileTransferMessage::Offer(manifest) => {
            let transfer_id = manifest.id;
            if validate_manifest_policy(
                &manifest,
                &runtime.config,
                peer_device_id,
                runtime.local_device_id,
            )
            .is_err()
            {
                send_message(
                    connection,
                    FileTransferMessage::Reject {
                        transfer_id,
                        reason: "offer violates local transfer policy".into(),
                    },
                )
                .await?;
                return Ok(());
            }
            let Ok(permit) = Arc::clone(&runtime.active_transfer).try_acquire_owned() else {
                send_message(
                    connection,
                    FileTransferMessage::Reject {
                        transfer_id,
                        reason: "another transfer is active".into(),
                    },
                )
                .await?;
                return Ok(());
            };
            if let Err(error) =
                run_inbound(connection, runtime, peer_device_id, manifest, permit).await
            {
                let _ = send_message(
                    connection,
                    FileTransferMessage::Cancel {
                        transfer_id,
                        reason: "transfer validation or I/O failed".into(),
                    },
                )
                .await;
                tracing::warn!(transfer_id = %transfer_id.0, error = %error, "inbound file transfer stopped");
            }
        }
        FileTransferMessage::Chunk(chunk) => {
            send_message(
                connection,
                FileTransferMessage::Cancel {
                    transfer_id: chunk.transfer_id,
                    reason: "no transfer is active".into(),
                },
            )
            .await?;
        }
        FileTransferMessage::Complete { transfer_id, .. }
        | FileTransferMessage::Accept { transfer_id, .. }
        | FileTransferMessage::Reject { transfer_id, .. }
        | FileTransferMessage::Cancel { transfer_id, .. } => {
            send_message(
                connection,
                FileTransferMessage::Cancel {
                    transfer_id,
                    reason: "no matching transfer is active".into(),
                },
            )
            .await?;
        }
        FileTransferMessage::Checkpoint(checkpoint) | FileTransferMessage::Ack(checkpoint) => {
            send_message(
                connection,
                FileTransferMessage::Cancel {
                    transfer_id: checkpoint.id,
                    reason: "no matching transfer is active".into(),
                },
            )
            .await?;
        }
        _ => {}
    }
    Ok(())
}

async fn send_message(
    connection: &dyn Connection,
    message: FileTransferMessage,
) -> anyhow::Result<()> {
    let body = message.encode().context("encoding file-transfer message")?;
    connection
        .send(Envelope::new(
            PROTOCOL_VERSION,
            MessageId::ZERO,
            MessageKind::FileTransfer,
            body,
        ))
        .await
        .context("sending file-transfer message")
}

fn decode_envelope(envelope: Envelope) -> anyhow::Result<FileTransferMessage> {
    ensure!(
        envelope.kind == MessageKind::FileTransfer,
        "non-file message reached file-transfer lane"
    );
    FileTransferMessage::decode(envelope.body).context("decoding file-transfer message")
}

fn manifest_from_queue(
    transfer: &QueuedTransfer,
    local_device_id: DeviceId,
    peer_device_id: DeviceId,
) -> anyhow::Result<TransferManifest> {
    let entries = transfer
        .entries
        .iter()
        .map(|entry| -> anyhow::Result<TransferEntry> {
            if entry.is_dir {
                Ok(TransferEntry::dir(entry.relative_path.clone())?)
            } else {
                let digest = entry
                    .sha256
                    .as_deref()
                    .context("queued file is missing its content digest")
                    .and_then(parse_sha256_hex)?;
                Ok(TransferEntry::file(
                    entry.relative_path.clone(),
                    entry.size_bytes,
                    digest,
                )?)
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    let manifest = TransferManifest::new(
        transfer.id()?,
        local_device_id,
        Some(peer_device_id),
        TransferSource::Picker,
        entries,
    )?;
    ensure!(
        manifest.total_bytes == transfer.total_bytes,
        "queue total does not match manifest"
    );
    Ok(manifest)
}

fn validate_manifest_policy(
    manifest: &TransferManifest,
    config: &FileTransferConfig,
    expected_sender: DeviceId,
    local_device_id: DeviceId,
) -> anyhow::Result<()> {
    ensure!(
        manifest.from == expected_sender,
        "manifest sender does not match authenticated peer"
    );
    ensure!(
        manifest.to == Some(local_device_id),
        "manifest is not targeted to this device"
    );
    ensure!(
        !manifest.entries.is_empty()
            && manifest.entries.len() <= config.max_entries.min(MAX_TRANSFER_MANIFEST_ENTRIES),
        "manifest entry count exceeds policy"
    );
    ensure!(
        manifest.total_bytes <= config.max_transfer_bytes.min(MAX_TRANSFER_TOTAL_BYTES),
        "manifest byte count exceeds policy"
    );
    let mut portable = HashSet::with_capacity(manifest.entries.len());
    let mut declared = HashSet::with_capacity(manifest.entries.len());
    for entry in &manifest.entries {
        validate_transfer_entry_path(&entry.relative_path, entry.is_dir, entry.size_bytes)?;
        ensure!(
            portable.insert(entry.relative_path.to_lowercase()),
            "manifest contains case-colliding paths"
        );
        if let Some((parent, _)) = entry.relative_path.rsplit_once('/') {
            let mut prefix = String::new();
            for component in parent.split('/') {
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(component);
                ensure!(
                    declared.contains(&prefix),
                    "manifest child precedes a declared parent directory"
                );
            }
        }
        if entry.is_dir {
            declared.insert(entry.relative_path.clone());
        }
    }
    Ok(())
}

fn bytes_before_entry(manifest: &TransferManifest, entry_index: usize) -> anyhow::Result<u64> {
    manifest.entries[..entry_index]
        .iter()
        .filter(|entry| !entry.is_dir)
        .try_fold(0u64, |total, entry| {
            total
                .checked_add(entry.size_bytes)
                .context("manifest byte total overflow")
        })
}

fn validate_checkpoint(
    manifest: &TransferManifest,
    checkpoint: TransferCheckpoint,
    allow_directory_terminal: bool,
) -> anyhow::Result<()> {
    ensure!(
        checkpoint.id == manifest.id,
        "checkpoint transfer id mismatch"
    );
    let entry_index = usize::try_from(checkpoint.file_index)
        .context("checkpoint entry index does not fit this platform")?;
    let entry = manifest
        .entries
        .get(entry_index)
        .context("checkpoint entry index is outside the manifest")?;
    if entry.is_dir {
        ensure!(allow_directory_terminal, "checkpoint points to a directory");
        ensure!(
            checkpoint.offset == 0,
            "directory checkpoint has a non-zero offset"
        );
    } else {
        ensure!(
            checkpoint.offset <= entry.size_bytes,
            "checkpoint exceeds file length"
        );
    }
    let expected_transferred = bytes_before_entry(manifest, entry_index)?
        .checked_add(checkpoint.offset)
        .context("checkpoint byte total overflow")?;
    ensure!(
        checkpoint.transferred_bytes == expected_transferred,
        "checkpoint aggregate byte count is inconsistent"
    );
    Ok(())
}

fn next_file_position(
    manifest: &TransferManifest,
    checkpoint: Option<TransferCheckpoint>,
) -> anyhow::Result<Option<(usize, u64, u64)>> {
    let (start_index, start_offset) = match checkpoint {
        Some(checkpoint) => {
            validate_checkpoint(manifest, checkpoint, false)?;
            let index = checkpoint.file_index as usize;
            let entry = &manifest.entries[index];
            if checkpoint.offset < entry.size_bytes {
                return Ok(Some((
                    index,
                    checkpoint.offset,
                    checkpoint.transferred_bytes,
                )));
            }
            (index + 1, 0)
        }
        None => (0, 0),
    };
    for (index, entry) in manifest.entries.iter().enumerate().skip(start_index) {
        if !entry.is_dir {
            return Ok(Some((
                index,
                start_offset,
                bytes_before_entry(manifest, index)?,
            )));
        }
    }
    Ok(None)
}

fn terminal_checkpoint(manifest: &TransferManifest) -> anyhow::Result<TransferCheckpoint> {
    let (file_index, offset) = manifest
        .entries
        .iter()
        .enumerate()
        .rev()
        .find(|(_, entry)| !entry.is_dir)
        .map_or_else(
            || {
                manifest
                    .entries
                    .len()
                    .checked_sub(1)
                    .and_then(|index| u32::try_from(index).ok())
                    .map(|index| (index, 0))
                    .context("cannot checkpoint an empty manifest")
            },
            |(index, entry)| {
                u32::try_from(index)
                    .map(|index| (index, entry.size_bytes))
                    .context("terminal entry index does not fit wire format")
            },
        )?;
    Ok(TransferCheckpoint {
        id: manifest.id,
        file_index,
        offset,
        transferred_bytes: manifest.total_bytes,
    })
}

fn checkpoint_after_chunk(
    manifest: &TransferManifest,
    file_index: usize,
    offset: u64,
) -> anyhow::Result<TransferCheckpoint> {
    let transferred_bytes = bytes_before_entry(manifest, file_index)?
        .checked_add(offset)
        .context("chunk checkpoint byte total overflow")?;
    Ok(TransferCheckpoint {
        id: manifest.id,
        file_index: u32::try_from(file_index).context("file index does not fit wire format")?,
        offset,
        transferred_bytes,
    })
}

fn checkpoint_before_entry(
    manifest: &TransferManifest,
    entry_index: usize,
) -> anyhow::Result<Option<TransferCheckpoint>> {
    manifest.entries[..entry_index]
        .iter()
        .enumerate()
        .rev()
        .find(|(_, entry)| !entry.is_dir)
        .map(|(index, entry)| checkpoint_after_chunk(manifest, index, entry.size_bytes))
        .transpose()
}

async fn run_outbound(
    connection: &dyn Connection,
    runtime: &FileTransferRuntime,
    peer_device_id: DeviceId,
    record: &QueueRecord,
    permit: OwnedSemaphorePermit,
) -> anyhow::Result<OutboundResult> {
    let manifest = manifest_from_queue(&record.transfer, runtime.local_device_id, peer_device_id)?;
    send_message(connection, FileTransferMessage::Offer(manifest.clone())).await?;

    let mut permit = Some(permit);
    let checkpoint = loop {
        let message = recv_message_with_timeout(connection, PROTOCOL_PROGRESS_TIMEOUT).await?;
        if let FileTransferMessage::Offer(incoming) = message {
            if validate_manifest_policy(
                &incoming,
                &runtime.config,
                peer_device_id,
                runtime.local_device_id,
            )
            .is_err()
            {
                send_message(
                    connection,
                    FileTransferMessage::Reject {
                        transfer_id: incoming.id,
                        reason: "offer violates local transfer policy".into(),
                    },
                )
                .await?;
                continue;
            }
            if outbound_wins_collision(&manifest, &incoming) {
                send_message(
                    connection,
                    FileTransferMessage::Reject {
                        transfer_id: incoming.id,
                        reason: "deterministic transfer collision loser".into(),
                    },
                )
                .await?;
                continue;
            }
            let permit = permit.take().context("active transfer permit is missing")?;
            run_inbound(connection, runtime, peer_device_id, incoming, permit).await?;
            return Ok(OutboundResult::Retained);
        }
        if let Some(message_id) = message_transfer_id(&message)
            && message_id != manifest.id
        {
            tracing::debug!(transfer_id = %message_id.0, "ignored stale file-transfer message");
            continue;
        }
        match message {
            FileTransferMessage::Accept {
                transfer_id,
                checkpoint,
            } if transfer_id == manifest.id => {
                if let Some(checkpoint) = checkpoint {
                    validate_checkpoint(&manifest, checkpoint, false)?;
                }
                break checkpoint;
            }
            FileTransferMessage::Reject { transfer_id, .. }
            | FileTransferMessage::Cancel { transfer_id, .. }
                if transfer_id == manifest.id =>
            {
                return Ok(OutboundResult::Retained);
            }
            _ => {
                send_message(
                    connection,
                    FileTransferMessage::Cancel {
                        transfer_id: manifest.id,
                        reason: "unexpected offer response".into(),
                    },
                )
                .await?;
                anyhow::bail!("peer returned an invalid offer response");
            }
        }
    };

    for (file_index, entry) in manifest.entries.iter().enumerate() {
        if entry.is_dir {
            continue;
        }
        let queued_source = record
            .transfer
            .entries
            .get(file_index)
            .context("manifest and queue entry ordering diverged")?;
        ensure!(
            !queued_source.is_dir,
            "manifest and queue entry types diverged"
        );
        let offset = checkpoint.map_or(0, |checkpoint| {
            let checkpoint_index = checkpoint.file_index as usize;
            if file_index < checkpoint_index {
                entry.size_bytes
            } else if file_index == checkpoint_index {
                checkpoint.offset
            } else {
                0
            }
        });
        let source_path = PathBuf::from(&queued_source.absolute_path);
        let transfer_id = manifest.id;
        let expected_len = entry.size_bytes;
        let wire_index = u32::try_from(file_index).context("file index does not fit wire")?;
        let expected_sha256 = entry
            .sha256
            .context("manifest file is missing its content digest")?;
        let reader = tokio::task::spawn_blocking(move || {
            HashedTransferReader::open(
                &source_path,
                transfer_id,
                wire_index,
                expected_len,
                offset,
                expected_sha256,
            )
        })
        .await
        .context("joining source-file open")??;
        let mut reader = Some(reader);

        loop {
            let mut current_reader = reader.take().context("source reader is missing")?;
            let (returned_reader, chunk) = tokio::task::spawn_blocking(move || {
                let chunk = current_reader.next_chunk();
                (current_reader, chunk)
            })
            .await
            .context("joining bounded source read")?;
            let chunk = match chunk? {
                Some(chunk) => {
                    reader = Some(returned_reader);
                    chunk
                }
                None => {
                    reader = Some(returned_reader);
                    break;
                }
            };
            let expected_offset = chunk
                .offset
                .checked_add(u64::from(chunk.plain_len))
                .context("chunk offset overflow")?;
            let expected_checkpoint =
                checkpoint_after_chunk(&manifest, file_index, expected_offset)?;
            send_message(connection, FileTransferMessage::Chunk(chunk)).await?;

            match recv_active_message(connection, manifest.id).await? {
                FileTransferMessage::Checkpoint(actual) if actual == expected_checkpoint => {
                    send_message(connection, FileTransferMessage::Ack(actual)).await?;
                }
                FileTransferMessage::Reject { transfer_id, .. }
                | FileTransferMessage::Cancel { transfer_id, .. }
                    if transfer_id == manifest.id =>
                {
                    return Ok(OutboundResult::Retained);
                }
                _ => {
                    send_message(
                        connection,
                        FileTransferMessage::Cancel {
                            transfer_id: manifest.id,
                            reason: "invalid persistence checkpoint".into(),
                        },
                    )
                    .await?;
                    anyhow::bail!("peer returned an invalid persistence checkpoint");
                }
            }
        }
        let reader = reader.context("source reader is missing after streaming")?;
        if let Err(error) = tokio::task::spawn_blocking(move || reader.verify())
            .await
            .context("joining source-content verification")?
        {
            send_message(
                connection,
                FileTransferMessage::Cancel {
                    transfer_id: manifest.id,
                    reason: "source changed after it was queued".into(),
                },
            )
            .await?;
            tracing::warn!(
                transfer_id = %manifest.id.0,
                error = %error,
                "source digest changed; queue retained"
            );
            return Ok(OutboundResult::Retained);
        }
    }

    send_message(
        connection,
        FileTransferMessage::Complete {
            transfer_id: manifest.id,
            transferred_bytes: manifest.total_bytes,
        },
    )
    .await?;
    let terminal = terminal_checkpoint(&manifest)?;
    match recv_active_message(connection, manifest.id).await? {
        FileTransferMessage::Ack(checkpoint) if checkpoint == terminal => {
            let queue_path = record.path.clone();
            let transfer = record.transfer.clone();
            tokio::task::spawn_blocking(move || {
                remove_queue_entry_after_terminal_ack(&QueueRecord {
                    path: queue_path,
                    transfer,
                })
            })
            .await
            .context("joining acknowledged queue removal")??;
            Ok(OutboundResult::Acknowledged)
        }
        FileTransferMessage::Reject { transfer_id, .. }
        | FileTransferMessage::Cancel { transfer_id, .. }
            if transfer_id == manifest.id =>
        {
            Ok(OutboundResult::Retained)
        }
        _ => {
            send_message(
                connection,
                FileTransferMessage::Cancel {
                    transfer_id: manifest.id,
                    reason: "invalid terminal acknowledgement".into(),
                },
            )
            .await?;
            anyhow::bail!("peer returned an invalid terminal acknowledgement")
        }
    }
}

async fn recv_active_message(
    connection: &dyn Connection,
    active_transfer: TransferId,
) -> anyhow::Result<FileTransferMessage> {
    loop {
        let message = recv_message_with_timeout(connection, PROTOCOL_PROGRESS_TIMEOUT).await?;
        if let FileTransferMessage::Offer(manifest) = &message {
            send_message(
                connection,
                FileTransferMessage::Reject {
                    transfer_id: manifest.id,
                    reason: "another transfer is active".into(),
                },
            )
            .await?;
            continue;
        }
        if let Some(message_id) = message_transfer_id(&message)
            && message_id != active_transfer
        {
            tracing::debug!(transfer_id = %message_id.0, "ignored stale file-transfer message");
            continue;
        }
        return Ok(message);
    }
}

async fn recv_message_with_timeout(
    connection: &dyn Connection,
    timeout: Duration,
) -> anyhow::Result<FileTransferMessage> {
    let envelope = tokio::time::timeout(timeout, connection.recv())
        .await
        .context("file-transfer peer made no protocol progress before timeout")?
        .context("receiving active file-transfer message")?;
    decode_envelope(envelope)
}

fn outbound_wins_collision(outbound: &TransferManifest, incoming: &TransferManifest) -> bool {
    let outbound_key = (outbound.from.0.as_bytes(), outbound.id.0.as_bytes());
    let incoming_key = (incoming.from.0.as_bytes(), incoming.id.0.as_bytes());
    outbound_key < incoming_key
}

fn message_transfer_id(message: &FileTransferMessage) -> Option<TransferId> {
    match message {
        FileTransferMessage::Offer(manifest) => Some(manifest.id),
        FileTransferMessage::Accept { transfer_id, .. }
        | FileTransferMessage::Reject { transfer_id, .. }
        | FileTransferMessage::Complete { transfer_id, .. }
        | FileTransferMessage::Cancel { transfer_id, .. } => Some(*transfer_id),
        FileTransferMessage::Chunk(chunk) => Some(chunk.transfer_id),
        FileTransferMessage::Checkpoint(checkpoint) | FileTransferMessage::Ack(checkpoint) => {
            Some(checkpoint.id)
        }
        _ => None,
    }
}

async fn run_inbound(
    connection: &dyn Connection,
    runtime: &FileTransferRuntime,
    peer_device_id: DeviceId,
    manifest: TransferManifest,
    _permit: OwnedSemaphorePermit,
) -> anyhow::Result<()> {
    validate_manifest_policy(
        &manifest,
        &runtime.config,
        peer_device_id,
        runtime.local_device_id,
    )?;
    let config_path = runtime.config_path.clone();
    let config = runtime.config.clone();
    let manifest_for_prepare = manifest.clone();
    let mut session = tokio::task::spawn_blocking(move || {
        prepare_receive_session(&config_path, &config, manifest_for_prepare)
    })
    .await
    .context("joining receive-session preparation")??;
    let resume_checkpoint = if manifest.file_count() == 0 {
        None
    } else {
        session.state.checkpoint(manifest.id)
    };
    send_message(
        connection,
        FileTransferMessage::Accept {
            transfer_id: manifest.id,
            checkpoint: resume_checkpoint,
        },
    )
    .await?;

    loop {
        match recv_active_message(connection, manifest.id).await? {
            FileTransferMessage::Chunk(chunk) => {
                let expected_checkpoint = match receive_chunk(&mut session, chunk).await {
                    Ok(checkpoint) => checkpoint,
                    Err(error) => {
                        send_message(
                            connection,
                            FileTransferMessage::Cancel {
                                transfer_id: manifest.id,
                                reason: "received file failed content validation".into(),
                            },
                        )
                        .await?;
                        tracing::warn!(
                            transfer_id = %manifest.id.0,
                            error = %error,
                            "inbound file validation failed; transfer cancelled"
                        );
                        return Ok(());
                    }
                };
                send_message(
                    connection,
                    FileTransferMessage::Checkpoint(expected_checkpoint),
                )
                .await?;
                match recv_active_message(connection, manifest.id).await? {
                    FileTransferMessage::Ack(actual) if actual == expected_checkpoint => {}
                    FileTransferMessage::Cancel { transfer_id, .. }
                        if transfer_id == manifest.id =>
                    {
                        return Ok(());
                    }
                    _ => anyhow::bail!("sender returned an invalid checkpoint acknowledgement"),
                }
            }
            FileTransferMessage::Complete {
                transfer_id,
                transferred_bytes,
            } if transfer_id == manifest.id => {
                ensure!(
                    transferred_bytes == manifest.total_bytes,
                    "complete byte count does not match manifest"
                );
                ensure!(
                    next_file_position(&manifest, session.state.checkpoint(manifest.id),)?
                        .is_none(),
                    "complete arrived before all files were persisted"
                );
                ensure!(
                    session.writer.is_none(),
                    "complete arrived with an open part writer"
                );
                let terminal = terminal_checkpoint(&manifest)?;
                session.state.set_checkpoint(terminal);
                session.state.complete = true;
                let root = session.root.clone();
                let state = session.state.clone();
                tokio::task::spawn_blocking(move || persist_receive_state(&root, &state))
                    .await
                    .context("joining terminal receive-state persistence")??;
                send_message(connection, FileTransferMessage::Ack(terminal)).await?;
                tracing::info!(transfer_id = %manifest.id.0, "inbound file transfer completed");
                return Ok(());
            }
            FileTransferMessage::Cancel { transfer_id, .. } if transfer_id == manifest.id => {
                return Ok(());
            }
            FileTransferMessage::Reject { transfer_id, .. } if transfer_id == manifest.id => {
                return Ok(());
            }
            _ => anyhow::bail!("sender returned an invalid transfer message"),
        }
    }
}

async fn receive_chunk(
    session: &mut ReceiveSession,
    chunk: nexkvm_streaming::TransferChunk,
) -> anyhow::Result<TransferCheckpoint> {
    ensure!(
        chunk.transfer_id == session.manifest.id,
        "chunk transfer id mismatch"
    );
    ensure!(
        chunk.compression == nexkvm_streaming::TransferCompression::None,
        "runtime accepts only bounded raw chunks"
    );
    ensure!(
        chunk.payload.len() == chunk.plain_len as usize,
        "chunk payload length does not match plaintext length"
    );
    ensure!(
        chunk.plain_len as usize <= MAX_TRANSFER_CHUNK_SIZE,
        "chunk exceeds the runtime plaintext bound"
    );
    let current_checkpoint = session.state.checkpoint(session.manifest.id);
    let Some((expected_index, expected_offset, _)) =
        next_file_position(&session.manifest, current_checkpoint)?
    else {
        anyhow::bail!("chunk arrived after every manifest file completed");
    };
    ensure!(
        chunk.file_index as usize == expected_index,
        "chunk file index is not the next manifest file"
    );
    ensure!(
        chunk.offset == expected_offset,
        "chunk offset is not contiguous"
    );
    let expected_entry = &session.manifest.entries[expected_index];
    let expected_sha256 = expected_entry
        .sha256
        .context("manifest file is missing its content digest")?;
    let resume_existing_part = current_checkpoint
        .is_some_and(|checkpoint| checkpoint.file_index as usize == expected_index);
    let end = chunk
        .offset
        .checked_add(u64::from(chunk.plain_len))
        .context("chunk offset overflow")?;
    ensure!(
        end <= expected_entry.size_bytes,
        "chunk exceeds manifest file length"
    );
    ensure!(
        chunk.final_chunk_for_file == (end == expected_entry.size_bytes),
        "chunk final marker does not match manifest file length"
    );

    let root = session.root.clone();
    let relative_path = expected_entry.relative_path.clone();
    let expected_len = expected_entry.size_bytes;
    let writer = session.writer.take();
    let final_chunk = chunk.final_chunk_for_file;
    let outcome = tokio::task::spawn_blocking(move || -> anyhow::Result<ReceiveWriteOutcome> {
        let mut writer = match writer {
            Some(writer) => writer,
            None if !resume_existing_part => Box::new(HashedPartWriter::create(
                &root,
                &relative_path,
                u32::try_from(expected_index).context("file index does not fit wire")?,
                expected_len,
                expected_sha256,
            )?),
            None => Box::new(HashedPartWriter::resume(
                &root,
                &relative_path,
                u32::try_from(expected_index).context("file index does not fit wire")?,
                expected_len,
                expected_offset,
                expected_sha256,
            )?),
        };
        writer.write_raw_chunk(&chunk)?;
        writer.flush_and_sync()?;
        if final_chunk {
            match (*writer).finalize_verified()? {
                VerifiedPublication::Published => Ok(ReceiveWriteOutcome::Published),
                VerifiedPublication::QuarantinedDigestMismatch => {
                    Ok(ReceiveWriteOutcome::QuarantinedDigestMismatch)
                }
            }
        } else {
            Ok(ReceiveWriteOutcome::Partial(writer))
        }
    })
    .await
    .context("joining bounded destination write")??;

    match outcome {
        ReceiveWriteOutcome::Partial(writer) => session.writer = Some(writer),
        ReceiveWriteOutcome::Published => session.writer = None,
        ReceiveWriteOutcome::QuarantinedDigestMismatch => {
            session.writer = None;
            session
                .state
                .replace_checkpoint(checkpoint_before_entry(&session.manifest, expected_index)?);
            session.state.complete = false;
            let root = session.root.clone();
            let state = session.state.clone();
            tokio::task::spawn_blocking(move || persist_receive_state(&root, &state))
                .await
                .context("joining digest-mismatch receive-state persistence")??;
            anyhow::bail!("received file content digest mismatch");
        }
    }

    let checkpoint = checkpoint_after_chunk(&session.manifest, expected_index, end)?;
    session.state.set_checkpoint(checkpoint);
    let root = session.root.clone();
    let state = session.state.clone();
    tokio::task::spawn_blocking(move || persist_receive_state(&root, &state))
        .await
        .context("joining receive checkpoint persistence")??;
    Ok(checkpoint)
}

fn prepare_receive_session(
    config_path: &Path,
    config: &FileTransferConfig,
    manifest: TransferManifest,
) -> anyhow::Result<ReceiveSession> {
    let base = receive_base_directory(config_path, config)?;
    let relative_id = manifest.id.0.to_string();
    let root = base.join(&relative_id);
    let manifest_sha256 = manifest_digest(&manifest)?;
    let mut state = match fs::symlink_metadata(&root) {
        Ok(metadata) => {
            ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "existing transfer destination is unsafe"
            );
            let state = read_receive_state(&root)?;
            ensure!(
                state.schema_version == RECEIVE_STATE_SCHEMA_VERSION
                    && state.transfer_id == relative_id
                    && state.manifest_sha256 == manifest_sha256,
                "existing transfer destination belongs to a different manifest"
            );
            state
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            create_transfer_directory(&base, &relative_id)?;
            ReceiveState {
                schema_version: RECEIVE_STATE_SCHEMA_VERSION,
                transfer_id: relative_id,
                manifest_sha256,
                checkpoint: None,
                complete: false,
            }
        }
        Err(error) => return Err(error.into()),
    };

    for entry in &manifest.entries {
        if entry.is_dir {
            create_transfer_directory(&root, &entry.relative_path)?;
        }
    }
    let reconciled = reconcile_receive_checkpoint(&root, &manifest)?;
    let all_files_verified = next_file_position(&manifest, reconciled)?.is_none();
    if !all_files_verified {
        state.complete = false;
    }
    if state.complete {
        if manifest.file_count() == 0 {
            ensure!(
                reconciled.is_none(),
                "completed directory transfer has unexpected files"
            );
        } else {
            ensure!(
                reconciled == Some(terminal_checkpoint(&manifest)?),
                "completed transfer destination no longer matches its manifest"
            );
        }
    }
    if let Some(checkpoint) = reconciled {
        validate_checkpoint(&manifest, checkpoint, false)?;
    }
    state.replace_checkpoint(reconciled);
    persist_receive_state(&root, &state)?;
    Ok(ReceiveSession {
        manifest,
        root,
        state,
        writer: None,
    })
}

fn receive_base_directory(
    config_path: &Path,
    config: &FileTransferConfig,
) -> anyhow::Result<PathBuf> {
    let downloads = match config
        .download_dir
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
    {
        Some(path) => {
            let path = PathBuf::from(path);
            if path.is_absolute() {
                path
            } else {
                config_path
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join(path)
            }
        }
        None => default_download_directory()?,
    };
    fs::create_dir_all(&downloads)?;
    let downloads = fs::canonicalize(downloads)?;
    let metadata = fs::symlink_metadata(&downloads)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "download root is not a safe directory"
    );
    create_transfer_directory(downloads, "NexKVM").map_err(anyhow::Error::from)
}

fn default_download_directory() -> anyhow::Result<PathBuf> {
    let home = if cfg!(target_os = "windows") {
        std::env::var_os("USERPROFILE")
    } else {
        std::env::var_os("HOME")
    }
    .map(PathBuf::from)
    .context("cannot resolve the platform Downloads directory")?;
    Ok(home.join("Downloads"))
}

fn manifest_digest(manifest: &TransferManifest) -> anyhow::Result<String> {
    let encoded = TransferManifestCodec::encode(manifest)?;
    Ok(format!("{:x}", Sha256::digest(&encoded)))
}

fn read_receive_state(root: &Path) -> anyhow::Result<ReceiveState> {
    let path = root.join(RECEIVE_STATE_FILE);
    let encoded =
        read_bounded_regular_utf8(&path, RECEIVE_STATE_MAX_BYTES, "receive state", false)?;
    toml::from_str(&encoded).context("decoding receive state")
}

fn read_bounded_regular_utf8(
    path: &Path,
    max_bytes: usize,
    description: &str,
    require_owner_only: bool,
) -> anyhow::Result<String> {
    let path_metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspecting {description} `{}`", path.display()))?;
    ensure!(
        path_metadata.is_file() && !path_metadata.file_type().is_symlink(),
        "{description} is not a safe regular file"
    );
    validate_bounded_regular_metadata(
        path,
        &path_metadata,
        max_bytes,
        description,
        require_owner_only,
    )?;

    let file =
        File::open(path).with_context(|| format!("opening {description} `{}`", path.display()))?;
    let path_metadata_after_open = fs::symlink_metadata(path)
        .with_context(|| format!("revalidating {description} `{}`", path.display()))?;
    ensure!(
        path_metadata_after_open.is_file() && !path_metadata_after_open.file_type().is_symlink(),
        "{description} is not a safe regular file"
    );
    validate_bounded_regular_metadata(
        path,
        &path_metadata_after_open,
        max_bytes,
        description,
        require_owner_only,
    )?;
    read_bounded_regular_utf8_from_file(file, path, max_bytes, description, require_owner_only)
}

fn read_bounded_regular_utf8_from_file(
    file: File,
    path: &Path,
    max_bytes: usize,
    description: &str,
    require_owner_only: bool,
) -> anyhow::Result<String> {
    let metadata = file
        .metadata()
        .with_context(|| format!("inspecting open {description} descriptor"))?;
    validate_bounded_regular_metadata(path, &metadata, max_bytes, description, require_owner_only)?;

    let read_limit = u64::try_from(max_bytes)
        .context("bounded persisted-file limit does not fit u64")?
        .checked_add(1)
        .context("bounded persisted-file read limit overflow")?;
    let mut bytes = Vec::new();
    file.take(read_limit)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading {description} from its open descriptor"))?;
    ensure!(
        bytes.len() <= max_bytes,
        "{description} exceeds the {max_bytes}-byte limit"
    );
    String::from_utf8(bytes).with_context(|| format!("{description} is not valid UTF-8"))
}

fn validate_bounded_regular_metadata(
    path: &Path,
    metadata: &fs::Metadata,
    max_bytes: usize,
    description: &str,
    require_owner_only: bool,
) -> anyhow::Result<()> {
    ensure!(
        metadata.is_file(),
        "{description} is not a safe regular file"
    );
    if require_owner_only {
        ensure_owner_only_permissions(path, metadata, false)?;
    }
    let max_bytes_u64 =
        u64::try_from(max_bytes).context("bounded persisted-file limit does not fit u64")?;
    ensure!(
        metadata.len() <= max_bytes_u64,
        "{description} exceeds the {max_bytes}-byte limit"
    );
    Ok(())
}

fn persist_receive_state(root: &Path, state: &ReceiveState) -> anyhow::Result<()> {
    let destination = root.join(RECEIVE_STATE_FILE);
    match fs::symlink_metadata(&destination) {
        Ok(metadata) => ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "receive state destination is unsafe"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let temporary = root.join(format!(
        ".{RECEIVE_STATE_FILE}.{}.tmp",
        uuid::Uuid::new_v4()
    ));
    let encoded = toml::to_string(state)?;
    let mut file = open_private_new(&temporary)?;
    let result = (|| -> anyhow::Result<()> {
        file.write_all(encoded.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, &destination)?;
        sync_directory(root)?;
        Ok(())
    })();
    drop(file);
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

fn reconcile_receive_checkpoint(
    root: &Path,
    manifest: &TransferManifest,
) -> anyhow::Result<Option<TransferCheckpoint>> {
    let mut checkpoint = None;
    let mut incomplete_seen = false;
    for (index, entry) in manifest.entries.iter().enumerate() {
        if entry.is_dir {
            continue;
        }
        let final_path = root.join(Path::new(&entry.relative_path));
        let part_path = part_path_for(&final_path)?;
        let final_metadata = safe_optional_file_metadata(&final_path)?;
        let part_metadata = safe_optional_file_metadata(&part_path)?;
        if incomplete_seen {
            if final_metadata.is_some() {
                quarantine_file(&final_path)?;
            }
            if part_metadata.is_some() {
                quarantine_file(&part_path)?;
            }
            continue;
        }

        let expected_sha256 = entry
            .sha256
            .context("manifest file is missing its content digest")?;
        if let Some(final_metadata) = final_metadata {
            let final_matches = final_metadata.len() == entry.size_bytes
                && hash_file_bounded_digest(&final_path)? == expected_sha256;
            if final_matches {
                if part_metadata.is_some() {
                    quarantine_file(&part_path)?;
                }
                checkpoint = Some(checkpoint_after_chunk(manifest, index, entry.size_bytes)?);
                continue;
            }
            quarantine_file(&final_path)?;
        }

        let Some(part_metadata) = part_metadata else {
            incomplete_seen = true;
            continue;
        };
        if part_metadata.len() > entry.size_bytes {
            quarantine_file(&part_path)?;
            incomplete_seen = true;
            continue;
        }
        if part_metadata.len() < entry.size_bytes {
            checkpoint = Some(checkpoint_after_chunk(
                manifest,
                index,
                part_metadata.len(),
            )?);
            incomplete_seen = true;
            continue;
        }

        let writer = HashedPartWriter::resume(
            root,
            &entry.relative_path,
            u32::try_from(index).context("file index does not fit wire")?,
            entry.size_bytes,
            part_metadata.len(),
            expected_sha256,
        )?;
        match writer.finalize_verified()? {
            VerifiedPublication::Published => {
                checkpoint = Some(checkpoint_after_chunk(manifest, index, entry.size_bytes)?);
            }
            VerifiedPublication::QuarantinedDigestMismatch => incomplete_seen = true,
        }
    }
    Ok(checkpoint)
}

fn safe_optional_file_metadata(path: &Path) -> anyhow::Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "receive destination contains an unsafe file"
            );
            Ok(Some(metadata))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn part_path_for(final_path: &Path) -> anyhow::Result<PathBuf> {
    let name = final_path
        .file_name()
        .context("manifest file has no destination basename")?;
    let mut part_name = name.to_os_string();
    part_name.push(".part");
    Ok(final_path.with_file_name(part_name))
}

fn quarantine_file(path: &Path) -> anyhow::Result<PathBuf> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "refusing to quarantine an unsafe receive artifact"
    );
    let parent = path
        .parent()
        .context("receive artifact has no parent directory")?;
    let name = path
        .file_name()
        .context("receive artifact has no basename")?;
    for _ in 0..8 {
        let mut quarantine_name = name.to_os_string();
        quarantine_name.push(format!(".corrupt-{}", uuid::Uuid::new_v4()));
        let quarantine_path = path.with_file_name(quarantine_name);
        match fs::hard_link(path, &quarantine_path) {
            Ok(()) => {
                fs::remove_file(path)?;
                sync_directory(parent)?;
                return Ok(quarantine_path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    anyhow::bail!("could not allocate a unique quarantine filename")
}

impl QueuedTransfer {
    fn id(&self) -> anyhow::Result<TransferId> {
        let id = uuid::Uuid::parse_str(&self.transfer_id)
            .context("queued transfer has an invalid identifier")?;
        Ok(TransferId(id))
    }
}

/// Validate local paths and atomically persist one outbound transfer request.
pub fn enqueue_paths(
    config_path: &Path,
    config: &FileTransferConfig,
    paths: &[PathBuf],
) -> anyhow::Result<TransferId> {
    ensure!(config.enabled, "file transfer is disabled in config");
    let queued = build_queued_transfer(config, paths)?;
    let id = queued.id()?;
    persist_queue_entry(config_path, &queued)?;
    Ok(id)
}

fn build_queued_transfer(
    config: &FileTransferConfig,
    paths: &[PathBuf],
) -> anyhow::Result<QueuedTransfer> {
    ensure!(!paths.is_empty(), "at least one source path is required");
    ensure!(
        config.max_entries > 0,
        "file-transfer max_entries must be positive"
    );
    ensure!(
        config.max_transfer_bytes <= MAX_TRANSFER_TOTAL_BYTES,
        "configured file-transfer byte limit exceeds the wire limit"
    );

    let max_entries = config.max_entries.min(MAX_TRANSFER_MANIFEST_ENTRIES);
    ensure!(
        paths.len() <= max_entries,
        "source exceeds the configured manifest-entry limit"
    );
    let mut roots = Vec::with_capacity(paths.len());
    let mut root_names = HashSet::with_capacity(paths.len());
    for path in paths {
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("cannot inspect source `{}`", path.display()))?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "source symlinks are not allowed"
        );
        ensure!(
            metadata.is_file() || metadata.is_dir(),
            "source must be a regular file or directory"
        );
        let canonical = fs::canonicalize(path)
            .with_context(|| format!("cannot resolve source `{}`", path.display()))?;
        let basename = canonical
            .file_name()
            .and_then(|name| name.to_str())
            .context("source basename must be valid UTF-8")?
            .to_owned();
        validate_transfer_entry_path(&basename, metadata.is_dir(), metadata.len())?;
        ensure!(
            root_names.insert(basename.to_lowercase()),
            "source basenames collide on a case-insensitive destination"
        );
        roots.push((basename, canonical));
    }
    roots.sort_by(|left, right| {
        left.0
            .to_lowercase()
            .cmp(&right.0.to_lowercase())
            .then_with(|| left.0.cmp(&right.0))
    });

    let mut entries = Vec::new();
    let mut portable_paths = HashSet::new();
    let mut total_bytes = 0u64;
    for (basename, canonical) in roots {
        collect_source_entries(
            &canonical,
            &basename,
            &mut entries,
            &mut portable_paths,
            &mut total_bytes,
            max_entries,
            config.max_transfer_bytes,
        )?;
    }

    let transfer_id = TransferId::generate();
    Ok(QueuedTransfer {
        schema_version: QUEUE_SCHEMA_VERSION,
        transfer_id: transfer_id.0.to_string(),
        queued_at_unix_nanos: u64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
        )
        .unwrap_or(u64::MAX),
        entries,
        total_bytes,
    })
}

fn collect_bounded_results<T>(
    items: impl IntoIterator<Item = anyhow::Result<T>>,
    max_items: usize,
    overflow_message: &str,
) -> anyhow::Result<Vec<T>> {
    ensure!(
        max_items <= MAX_TRANSFER_MANIFEST_ENTRIES,
        "bounded collection limit exceeds the manifest-entry cap"
    );
    let mut collected = Vec::with_capacity(max_items);
    for item in items {
        let item = item?;
        if collected.len() == max_items {
            anyhow::bail!("{overflow_message}");
        }
        collected.push(item);
    }
    Ok(collected)
}

#[allow(clippy::too_many_arguments)]
fn collect_source_entries(
    absolute_path: &Path,
    relative_path: &str,
    entries: &mut Vec<QueuedSource>,
    portable_paths: &mut HashSet<String>,
    total_bytes: &mut u64,
    max_entries: usize,
    max_bytes: u64,
) -> anyhow::Result<()> {
    let metadata = fs::symlink_metadata(absolute_path)
        .with_context(|| format!("cannot inspect source `{}`", absolute_path.display()))?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "source trees may not contain symlinks"
    );
    ensure!(
        metadata.is_file() || metadata.is_dir(),
        "source trees may contain only regular files and directories"
    );
    ensure!(
        entries.len() < max_entries,
        "source exceeds the configured manifest-entry limit"
    );
    validate_transfer_entry_path(relative_path, metadata.is_dir(), metadata.len())?;
    ensure!(
        portable_paths.insert(relative_path.to_lowercase()),
        "source paths collide on a case-insensitive destination"
    );

    let size_bytes = if metadata.is_file() {
        metadata.len()
    } else {
        0
    };
    if metadata.is_file() {
        *total_bytes = total_bytes
            .checked_add(size_bytes)
            .context("source byte total overflow")?;
        ensure!(
            *total_bytes <= max_bytes,
            "source exceeds the configured transfer-byte limit"
        );
    }
    let absolute_path = absolute_path
        .to_str()
        .context("source path must be valid UTF-8")?
        .to_owned();
    let sha256 = if metadata.is_file() {
        Some(hash_file_bounded(Path::new(&absolute_path))?)
    } else {
        None
    };
    entries.push(QueuedSource {
        absolute_path: absolute_path.clone(),
        relative_path: relative_path.to_owned(),
        is_dir: metadata.is_dir(),
        size_bytes,
        sha256,
    });

    if metadata.is_dir() {
        let remaining_entries = max_entries
            .checked_sub(entries.len())
            .context("source entry count exceeds the configured manifest-entry limit")?;
        let enumeration_context = format!("cannot enumerate source `{absolute_path}`");
        let children = fs::read_dir(&absolute_path)
            .with_context(|| enumeration_context.clone())?
            .map(|child| child.with_context(|| enumeration_context.clone()));
        let mut children = collect_bounded_results(
            children,
            remaining_entries,
            "source exceeds the configured manifest-entry limit",
        )?;
        children.sort_by(|left, right| {
            left.file_name()
                .to_string_lossy()
                .cmp(&right.file_name().to_string_lossy())
        });
        for child in children {
            let name = child
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("source entry name must be valid UTF-8"))?;
            let child_relative = format!("{relative_path}/{name}");
            collect_source_entries(
                &child.path(),
                &child_relative,
                entries,
                portable_paths,
                total_bytes,
                max_entries,
                max_bytes,
            )?;
        }
    }
    Ok(())
}

fn hash_file_bounded(path: &Path) -> anyhow::Result<String> {
    let digest = hash_file_bounded_digest(path)?;
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(FILE_TRANSFER_SHA256_BYTES * 2);
    for byte in digest {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    Ok(encoded)
}

fn hash_file_bounded_digest(path: &Path) -> anyhow::Result<[u8; FILE_TRANSFER_SHA256_BYTES]> {
    let before = fs::symlink_metadata(path)?;
    ensure!(
        before.is_file() && !before.file_type().is_symlink(),
        "file is not a safe regular file"
    );
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let after = fs::symlink_metadata(path)?;
    ensure!(
        after.is_file()
            && !after.file_type().is_symlink()
            && before.len() == after.len()
            && before.modified().ok() == after.modified().ok(),
        "file changed while its content digest was computed"
    );
    Ok(hasher.finalize().into())
}

fn parse_sha256_hex(value: &str) -> anyhow::Result<[u8; FILE_TRANSFER_SHA256_BYTES]> {
    ensure!(
        value.len() == 64,
        "SHA-256 digest must contain 64 hex digits"
    );
    let mut digest = [0u8; FILE_TRANSFER_SHA256_BYTES];
    for (index, byte) in digest.iter_mut().enumerate() {
        let start = index * 2;
        *byte = u8::from_str_radix(&value[start..start + 2], 16)
            .context("SHA-256 digest contains invalid hex")?;
    }
    Ok(digest)
}

fn validate_transfer_entry_path(path: &str, is_dir: bool, size: u64) -> anyhow::Result<()> {
    if is_dir {
        TransferEntry::dir(path).context("source path is not portable")?;
    } else {
        TransferEntry::file(path, size, [0; 32]).context("source path is not portable")?;
    }
    Ok(())
}

fn queue_directory(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(QUEUE_DIRECTORY_NAME)
}

fn persist_queue_entry(config_path: &Path, queued: &QueuedTransfer) -> anyhow::Result<()> {
    let directory = queue_directory(config_path);
    create_private_directory(&directory)?;
    let destination = directory.join(format!("{}.toml", queued.transfer_id));
    ensure!(
        !destination.exists(),
        "queued transfer identifier already exists"
    );
    let temporary = directory.join(format!(".{}.tmp", queued.transfer_id));
    let encoded = toml::to_string(queued).context("serializing queued transfer")?;

    let mut file = open_private_new(&temporary).context("creating durable transfer queue entry")?;
    let persist_result = (|| -> anyhow::Result<()> {
        file.write_all(encoded.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, &destination)?;
        sync_directory(&directory)?;
        Ok(())
    })();
    drop(file);
    if persist_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    persist_result.context("persisting durable transfer queue entry")
}

fn load_oldest_queue_entry(
    config_path: &Path,
    config: &FileTransferConfig,
) -> anyhow::Result<Option<QueueRecord>> {
    let directory = queue_directory(config_path);
    let directory_metadata = match fs::symlink_metadata(&directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        directory_metadata.is_dir() && !directory_metadata.file_type().is_symlink(),
        "file-transfer queue is not a safe directory"
    );
    ensure_owner_only_permissions(&directory, &directory_metadata, true)?;
    let mut oldest: Option<QueueRecord> = None;
    for entry in fs::read_dir(&directory)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("toml") {
            continue;
        }
        let encoded = read_bounded_regular_utf8(
            &path,
            QUEUE_RECORD_MAX_BYTES,
            "file-transfer queue record",
            true,
        )?;
        let transfer: QueuedTransfer =
            toml::from_str(&encoded).context("decoding file-transfer queue record")?;
        let candidate = QueueRecord { path, transfer };
        let replace = oldest.as_ref().is_none_or(|current| {
            candidate.transfer.queued_at_unix_nanos < current.transfer.queued_at_unix_nanos
                || (candidate.transfer.queued_at_unix_nanos
                    == current.transfer.queued_at_unix_nanos
                    && candidate.transfer.transfer_id < current.transfer.transfer_id)
        });
        if replace {
            oldest = Some(candidate);
        }
    }
    if let Some(record) = &oldest {
        validate_queue_record(&record.path, config, &record.transfer)?;
    }
    Ok(oldest)
}

fn validate_queue_record(
    path: &Path,
    config: &FileTransferConfig,
    transfer: &QueuedTransfer,
) -> anyhow::Result<()> {
    ensure!(
        transfer.schema_version == QUEUE_SCHEMA_VERSION,
        "unsupported file-transfer queue schema"
    );
    let id = transfer.id()?;
    ensure!(
        path.file_stem().and_then(|stem| stem.to_str()) == Some(transfer.transfer_id.as_str()),
        "file-transfer queue filename does not match its identifier"
    );
    ensure!(!transfer.entries.is_empty(), "queued transfer is empty");
    ensure!(
        transfer.entries.len() <= config.max_entries.min(MAX_TRANSFER_MANIFEST_ENTRIES),
        "queued transfer exceeds the configured entry limit"
    );
    ensure!(
        transfer.total_bytes <= config.max_transfer_bytes.min(MAX_TRANSFER_TOTAL_BYTES),
        "queued transfer exceeds the configured byte limit"
    );
    ensure!(
        id.0.to_string() == transfer.transfer_id,
        "non-canonical transfer identifier"
    );

    let root_paths = transfer
        .entries
        .iter()
        .filter(|entry| !entry.relative_path.contains('/'))
        .map(|entry| PathBuf::from(&entry.absolute_path))
        .collect::<Vec<_>>();
    ensure!(
        !root_paths.is_empty(),
        "queued transfer has no source roots"
    );
    let rebuilt = build_queued_transfer(config, &root_paths)?;
    ensure!(
        rebuilt.entries == transfer.entries && rebuilt.total_bytes == transfer.total_bytes,
        "queued sources changed after they were selected"
    );
    Ok(())
}

fn remove_queue_entry_after_terminal_ack(record: &QueueRecord) -> anyhow::Result<()> {
    let encoded = read_bounded_regular_utf8(
        &record.path,
        QUEUE_RECORD_MAX_BYTES,
        "file-transfer queue record",
        true,
    )?;
    let persisted: QueuedTransfer = toml::from_str(&encoded)?;
    ensure!(
        persisted == record.transfer,
        "file-transfer queue record changed before acknowledgement"
    );
    fs::remove_file(&record.path)?;
    if let Some(directory) = record.path.parent() {
        sync_directory(directory)?;
    }
    Ok(())
}

fn create_private_directory(path: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(path)?;
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "file-transfer queue path is not a private directory"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn ensure_owner_only_permissions(
    path: &Path,
    metadata: &fs::Metadata,
    directory: bool,
) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode() & 0o777;
        let expected = if directory { 0o700 } else { 0o600 };
        ensure!(
            mode == expected,
            "owner-only permissions are required for `{}`",
            path.display()
        );
    }
    #[cfg(not(unix))]
    {
        let _ = (path, metadata, directory);
    }
    Ok(())
}

fn open_private_new(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn sync_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use nexkvm_crypto::PublicKey;
    use nexkvm_network::{NetworkError, TransportKind};
    use std::net::{Ipv4Addr, SocketAddr};
    use std::sync::atomic::{AtomicBool, Ordering};
    use tempfile::TempDir;
    use tokio::sync::{Mutex, mpsc};

    fn enabled_config() -> FileTransferConfig {
        FileTransferConfig {
            enabled: true,
            download_dir: None,
            max_transfer_bytes: 1024,
            max_entries: 16,
        }
    }

    fn digest(bytes: &[u8]) -> [u8; 32] {
        Sha256::digest(bytes).into()
    }

    #[derive(Debug)]
    struct MemoryConnection {
        outbound: mpsc::Sender<Envelope>,
        inbound: Mutex<mpsc::Receiver<Envelope>>,
        identity: Option<PublicKey>,
    }

    #[async_trait]
    impl Connection for MemoryConnection {
        fn kind(&self) -> TransportKind {
            TransportKind::Tcp
        }

        fn peer_addr(&self) -> SocketAddr {
            SocketAddr::from((Ipv4Addr::LOCALHOST, 47_654))
        }

        fn peer_identity(&self) -> Option<PublicKey> {
            self.identity.clone()
        }

        async fn send(&self, envelope: Envelope) -> Result<(), NetworkError> {
            self.outbound
                .send(envelope)
                .await
                .map_err(|_| NetworkError::Closed)
        }

        async fn recv(&self) -> Result<Envelope, NetworkError> {
            self.inbound
                .lock()
                .await
                .recv()
                .await
                .ok_or(NetworkError::Closed)
        }

        async fn close(&self) -> Result<(), NetworkError> {
            Ok(())
        }
    }

    fn memory_connection_pair() -> (MemoryConnection, MemoryConnection) {
        let (left_to_right, right_inbound) = mpsc::channel(32);
        let (right_to_left, left_inbound) = mpsc::channel(32);
        (
            MemoryConnection {
                outbound: left_to_right,
                inbound: Mutex::new(left_inbound),
                identity: None,
            },
            MemoryConnection {
                outbound: right_to_left,
                inbound: Mutex::new(right_inbound),
                identity: None,
            },
        )
    }

    #[derive(Debug)]
    struct MutatingConnection {
        inner: MemoryConnection,
        source: PathBuf,
        mutated: AtomicBool,
    }

    #[async_trait]
    impl Connection for MutatingConnection {
        fn kind(&self) -> TransportKind {
            self.inner.kind()
        }

        fn peer_addr(&self) -> SocketAddr {
            self.inner.peer_addr()
        }

        fn peer_identity(&self) -> Option<PublicKey> {
            self.inner.peer_identity()
        }

        async fn send(&self, envelope: Envelope) -> Result<(), NetworkError> {
            let first_chunk = envelope.kind == MessageKind::FileTransfer
                && matches!(
                    FileTransferMessage::decode(envelope.body.clone()),
                    Ok(FileTransferMessage::Chunk(chunk)) if chunk.offset == 0
                );
            if first_chunk && !self.mutated.swap(true, Ordering::AcqRel) {
                let mut source = OpenOptions::new().write(true).open(&self.source)?;
                source.seek(SeekFrom::Start(RUNTIME_CHUNK_SIZE as u64))?;
                source.write_all(&vec![b'b'; RUNTIME_CHUNK_SIZE])?;
                source.sync_all()?;
            }
            self.inner.send(envelope).await
        }

        async fn recv(&self) -> Result<Envelope, NetworkError> {
            self.inner.recv().await
        }

        async fn close(&self) -> Result<(), NetworkError> {
            self.inner.close().await
        }
    }

    fn runtime(
        config_path: PathBuf,
        config: FileTransferConfig,
        local_device_id: DeviceId,
    ) -> FileTransferRuntime {
        FileTransferRuntime {
            config_path,
            config,
            local_device_id,
            active_peer: crate::ActivePeerSelection::AnyTrusted,
            active_transfer: Arc::new(Semaphore::new(1)),
        }
    }

    #[test]
    fn traversal_is_deterministic_and_queue_is_durable() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("folder");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("z.txt"), b"z").unwrap();
        fs::create_dir(source.join("empty")).unwrap();
        fs::write(source.join("a.txt"), b"abc").unwrap();
        let config_path = temp.path().join("config/config.toml");

        let id = enqueue_paths(&config_path, &enabled_config(), &[source]).unwrap();
        let queue_path = queue_directory(&config_path).join(format!("{}.toml", id.0));
        let queued: QueuedTransfer =
            toml::from_str(&fs::read_to_string(queue_path).unwrap()).unwrap();
        assert_eq!(
            queued
                .entries
                .iter()
                .map(|entry| entry.relative_path.as_str())
                .collect::<Vec<_>>(),
            ["folder", "folder/a.txt", "folder/empty", "folder/z.txt"]
        );
        assert_eq!(queued.total_bytes, 4);
    }

    #[test]
    fn offered_manifest_uses_the_exact_digests_persisted_by_enqueue() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("payload.bin");
        fs::write(&source, b"manifest digest").unwrap();
        let config_path = temp.path().join("config/config.toml");
        let config = enabled_config();
        enqueue_paths(&config_path, &config, &[source]).unwrap();
        let record = load_oldest_queue_entry(&config_path, &config)
            .unwrap()
            .unwrap();

        let manifest =
            manifest_from_queue(&record.transfer, DeviceId::generate(), DeviceId::generate())
                .unwrap();
        let queued_digest =
            parse_sha256_hex(record.transfer.entries[0].sha256.as_deref().unwrap()).unwrap();

        assert_eq!(manifest.entries[0].sha256, Some(queued_digest));
        assert_eq!(queued_digest, digest(b"manifest digest"));
    }

    #[test]
    fn duplicate_root_basenames_and_limits_are_rejected_without_queue_artifacts() {
        let temp = TempDir::new().unwrap();
        let left = temp.path().join("left/same.txt");
        let right = temp.path().join("right/same.txt");
        fs::create_dir_all(left.parent().unwrap()).unwrap();
        fs::create_dir_all(right.parent().unwrap()).unwrap();
        fs::write(&left, b"a").unwrap();
        fs::write(&right, b"b").unwrap();
        let config_path = temp.path().join("config/config.toml");

        assert!(enqueue_paths(&config_path, &enabled_config(), &[left, right]).is_err());
        assert!(!queue_directory(&config_path).exists());

        let large = temp.path().join("large.bin");
        fs::write(&large, vec![0u8; 1025]).unwrap();
        assert!(enqueue_paths(&config_path, &enabled_config(), &[large]).is_err());
        assert!(!queue_directory(&config_path).exists());
    }

    #[test]
    fn root_count_limit_is_checked_before_source_inspection_or_allocation() {
        let temp = TempDir::new().unwrap();
        let mut config = enabled_config();
        config.max_entries = 1;
        let missing = [temp.path().join("missing-a"), temp.path().join("missing-b")];

        let error = build_queued_transfer(&config, &missing).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("source exceeds the configured manifest-entry limit")
        );
    }

    #[test]
    fn bounded_collection_keeps_only_the_limit_and_pulls_one_overflow_sentinel() {
        let pulled = std::cell::Cell::new(0usize);
        let overflowing = (0..100).map(|value| {
            pulled.set(pulled.get() + 1);
            Ok::<_, anyhow::Error>(value)
        });

        let error = collect_bounded_results(overflowing, 3, "entry overflow").unwrap_err();

        assert_eq!(pulled.get(), 4, "only one sentinel may be consumed");
        assert!(error.to_string().contains("entry overflow"));

        let exact =
            collect_bounded_results((0..3).map(Ok::<_, anyhow::Error>), 3, "entry overflow")
                .unwrap();
        assert_eq!(exact, [0, 1, 2]);
    }

    #[test]
    fn directory_children_accept_exact_remaining_capacity_and_reject_one_more() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("folder");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("a.txt"), b"a").unwrap();
        fs::write(source.join("b.txt"), b"b").unwrap();
        let config_path = temp.path().join("config/config.toml");
        let mut config = enabled_config();
        config.max_entries = 3;

        let exact = build_queued_transfer(&config, std::slice::from_ref(&source)).unwrap();
        assert_eq!(exact.entries.len(), 3);

        fs::write(source.join("c.txt"), b"c").unwrap();
        let error = enqueue_paths(&config_path, &config, &[source]).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("source exceeds the configured manifest-entry limit")
        );
        assert!(!queue_directory(&config_path).exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_and_special_files_are_rejected_and_queue_is_owner_only() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let temp = TempDir::new().unwrap();
        let target = temp.path().join("target.txt");
        fs::write(&target, b"secret").unwrap();
        let link = temp.path().join("link.txt");
        symlink(&target, &link).unwrap();
        let config_path = temp.path().join("config/config.toml");
        assert!(enqueue_paths(&config_path, &enabled_config(), &[link]).is_err());

        assert!(
            enqueue_paths(
                &config_path,
                &enabled_config(),
                &[PathBuf::from("/dev/random")]
            )
            .is_err(),
            "special files must be rejected"
        );

        let source = temp.path().join("ok.txt");
        fs::write(&source, b"ok").unwrap();
        let id = enqueue_paths(&config_path, &enabled_config(), &[source]).unwrap();
        let directory = queue_directory(&config_path);
        let entry = directory.join(format!("{}.toml", id.0));
        assert_eq!(
            fs::metadata(directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(entry).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn queue_record_survives_partial_failure_and_is_removed_only_by_ack_path() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("queued.txt");
        fs::write(&source, b"resume me").unwrap();
        let config_path = temp.path().join("config/config.toml");
        let config = enabled_config();
        let id = enqueue_paths(&config_path, &config, &[source]).unwrap();

        let first = load_oldest_queue_entry(&config_path, &config)
            .unwrap()
            .unwrap();
        assert_eq!(first.transfer.id().unwrap(), id);
        assert!(
            first.path.exists(),
            "loading must not consume a queued transfer"
        );
        remove_queue_entry_after_terminal_ack(&first).unwrap();
        assert!(!first.path.exists());
        assert!(
            load_oldest_queue_entry(&config_path, &config)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn queue_revalidation_detects_source_changes() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("mutable.txt");
        fs::write(&source, b"first").unwrap();
        let config_path = temp.path().join("config/config.toml");
        let config = enabled_config();
        let id = enqueue_paths(&config_path, &config, std::slice::from_ref(&source)).unwrap();
        fs::write(source, b"changed").unwrap();

        assert!(load_oldest_queue_entry(&config_path, &config).is_err());
        assert!(
            queue_directory(&config_path)
                .join(format!("{}.toml", id.0))
                .exists(),
            "a changed source must leave its durable queue record for recovery"
        );
    }

    #[test]
    fn oversized_queue_record_is_rejected_before_toml_decode() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("payload.bin");
        fs::write(&source, b"bounded queue").unwrap();
        let config_path = temp.path().join("config/config.toml");
        let config = enabled_config();
        let id = enqueue_paths(&config_path, &config, &[source]).unwrap();
        let record = load_oldest_queue_entry(&config_path, &config)
            .unwrap()
            .unwrap();
        let queue_path = queue_directory(&config_path).join(format!("{}.toml", id.0));
        OpenOptions::new()
            .write(true)
            .open(queue_path)
            .unwrap()
            .set_len(32 * 1024 * 1024 + 1)
            .unwrap();

        let removal_error = remove_queue_entry_after_terminal_ack(&record).unwrap_err();
        assert!(
            removal_error
                .to_string()
                .contains("file-transfer queue record exceeds the 33554432-byte limit")
        );

        let error = load_oldest_queue_entry(&config_path, &config).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("file-transfer queue record exceeds the 33554432-byte limit")
        );
    }

    #[test]
    fn bounded_descriptor_read_revalidates_and_rejects_growth_past_the_limit() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("growing.toml");
        fs::write(&path, b"small").unwrap();
        let descriptor = File::open(&path).unwrap();
        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(9)
            .unwrap();

        let error =
            read_bounded_regular_utf8_from_file(descriptor, &path, 8, "test persisted file", false)
                .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("test persisted file exceeds the 8-byte limit")
        );
    }

    #[test]
    fn bounded_descriptor_read_accepts_the_exact_byte_limit() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("exact.toml");
        fs::write(&path, b"12345678").unwrap();

        let encoded = read_bounded_regular_utf8(&path, 8, "test persisted file", false).unwrap();

        assert_eq!(encoded, "12345678");
    }

    #[cfg(unix)]
    #[test]
    fn bounded_descriptor_read_does_not_reopen_a_replaced_path() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("anchored.toml");
        let replacement = temp.path().join("replacement.toml");
        fs::write(&path, b"original").unwrap();
        let descriptor = File::open(&path).unwrap();
        fs::write(&replacement, b"replacement").unwrap();
        fs::rename(replacement, &path).unwrap();

        let encoded = read_bounded_regular_utf8_from_file(
            descriptor,
            &path,
            64,
            "test persisted file",
            false,
        )
        .unwrap();

        assert_eq!(encoded, "original");
    }

    #[cfg(unix)]
    #[test]
    fn bounded_persisted_read_rejects_a_symlink_path_before_opening() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let target = temp.path().join("target.toml");
        let path = temp.path().join("linked.toml");
        fs::write(&target, b"secret").unwrap();
        symlink(target, &path).unwrap();

        let error = read_bounded_regular_utf8(&path, 64, "test persisted file", false).unwrap_err();

        assert!(error.to_string().contains("is not a safe regular file"));
    }

    #[test]
    fn inbound_policy_requires_authenticated_sender_target_and_portable_tree() {
        let sender = DeviceId::generate();
        let receiver = DeviceId::generate();
        let config = enabled_config();
        let valid = TransferManifest::new(
            TransferId::generate(),
            sender,
            Some(receiver),
            TransferSource::Picker,
            vec![
                TransferEntry::dir("folder").unwrap(),
                TransferEntry::file("folder/a.txt", 1, digest(b"a")).unwrap(),
            ],
        )
        .unwrap();
        validate_manifest_policy(&valid, &config, sender, receiver).unwrap();

        let mut untargeted = valid.clone();
        untargeted.to = None;
        assert!(validate_manifest_policy(&untargeted, &config, sender, receiver).is_err());
        assert!(validate_manifest_policy(&valid, &config, DeviceId::generate(), receiver).is_err());

        let case_collision = TransferManifest::new(
            TransferId::generate(),
            sender,
            Some(receiver),
            TransferSource::Picker,
            vec![
                TransferEntry::file("Readme.txt", 1, digest(b"a")).unwrap(),
                TransferEntry::file("README.txt", 1, digest(b"b")).unwrap(),
            ],
        );
        assert!(case_collision.is_err());
    }

    #[tokio::test]
    async fn terminal_ack_publishes_files_removes_queue_and_is_idempotently_retryable() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("share");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("data.bin"), b"bounded transfer payload").unwrap();
        fs::create_dir(source.join("empty")).unwrap();
        let sender_config_path = temp.path().join("sender/config.toml");
        let mut sender_config = enabled_config();
        sender_config.max_transfer_bytes = 1024 * 1024;
        let transfer_id = enqueue_paths(&sender_config_path, &sender_config, &[source]).unwrap();
        let record = load_oldest_queue_entry(&sender_config_path, &sender_config)
            .unwrap()
            .unwrap();

        let receiver_downloads = temp.path().join("downloads");
        let mut receiver_config = sender_config.clone();
        receiver_config.download_dir = Some(receiver_downloads.display().to_string());
        let sender_id = DeviceId::generate();
        let receiver_id = DeviceId::generate();
        let sender_runtime = runtime(sender_config_path.clone(), sender_config, sender_id);
        let receiver_runtime = runtime(
            temp.path().join("receiver/config.toml"),
            receiver_config,
            receiver_id,
        );
        let (sender_connection, receiver_connection) = memory_connection_pair();
        let sender_permit = Arc::clone(&sender_runtime.active_transfer)
            .try_acquire_owned()
            .unwrap();
        let receiver_permit = Arc::clone(&receiver_runtime.active_transfer)
            .try_acquire_owned()
            .unwrap();

        let sender = run_outbound(
            &sender_connection,
            &sender_runtime,
            receiver_id,
            &record,
            sender_permit,
        );
        let receiver = async {
            let offer = decode_envelope(receiver_connection.recv().await.unwrap()).unwrap();
            let FileTransferMessage::Offer(manifest) = offer else {
                panic!("expected offer")
            };
            run_inbound(
                &receiver_connection,
                &receiver_runtime,
                sender_id,
                manifest,
                receiver_permit,
            )
            .await
        };
        let (sent, received) = tokio::join!(sender, receiver);
        assert!(matches!(sent.unwrap(), OutboundResult::Acknowledged));
        received.unwrap();

        assert!(
            load_oldest_queue_entry(&sender_config_path, &sender_runtime.config)
                .unwrap()
                .is_none()
        );
        let root = receiver_downloads
            .join("NexKVM")
            .join(transfer_id.0.to_string());
        assert_eq!(
            fs::read(root.join("share/data.bin")).unwrap(),
            b"bounded transfer payload"
        );
        assert!(root.join("share/empty").is_dir());
        assert!(!root.join("share/data.bin.part").exists());

        // Model a lost terminal Ack by restoring the sender's exact queue record
        // after the receiver has durably marked the transfer complete. The same
        // offer must be acknowledged without touching or replacing final files.
        persist_queue_entry(&sender_config_path, &record.transfer).unwrap();
        let retry_record = load_oldest_queue_entry(&sender_config_path, &sender_runtime.config)
            .unwrap()
            .unwrap();
        let (retry_sender_connection, retry_receiver_connection) = memory_connection_pair();
        let retry_sender_permit = Arc::clone(&sender_runtime.active_transfer)
            .try_acquire_owned()
            .unwrap();
        let retry_receiver_permit = Arc::clone(&receiver_runtime.active_transfer)
            .try_acquire_owned()
            .unwrap();
        let retry_sender = run_outbound(
            &retry_sender_connection,
            &sender_runtime,
            receiver_id,
            &retry_record,
            retry_sender_permit,
        );
        let retry_receiver = async {
            let offer = decode_envelope(retry_receiver_connection.recv().await.unwrap()).unwrap();
            let FileTransferMessage::Offer(manifest) = offer else {
                panic!("expected retry offer")
            };
            run_inbound(
                &retry_receiver_connection,
                &receiver_runtime,
                sender_id,
                manifest,
                retry_receiver_permit,
            )
            .await
        };
        let (retried, received_again) = tokio::join!(retry_sender, retry_receiver);
        assert!(matches!(retried.unwrap(), OutboundResult::Acknowledged));
        received_again.unwrap();
        assert_eq!(
            fs::read(root.join("share/data.bin")).unwrap(),
            b"bounded transfer payload"
        );
        assert!(
            load_oldest_queue_entry(&sender_config_path, &sender_runtime.config)
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn zero_offset_partial_file_resumes_instead_of_overwriting_part() {
        let temp = TempDir::new().unwrap();
        let downloads = temp.path().join("downloads");
        let mut config = enabled_config();
        config.download_dir = Some(downloads.display().to_string());
        let sender = DeviceId::generate();
        let receiver = DeviceId::generate();
        let manifest = TransferManifest::new(
            TransferId::generate(),
            sender,
            Some(receiver),
            TransferSource::Picker,
            vec![TransferEntry::file("resume.bin", 4, digest(b"data")).unwrap()],
        )
        .unwrap();
        let config_path = temp.path().join("config.toml");
        let mut session = prepare_receive_session(&config_path, &config, manifest.clone()).unwrap();
        let mut writer = TransferPartWriter::create(&session.root, "resume.bin", 0, 4).unwrap();
        writer.flush().unwrap();
        drop(writer);
        session.state.set_checkpoint(TransferCheckpoint {
            id: manifest.id,
            file_index: 0,
            offset: 0,
            transferred_bytes: 0,
        });
        persist_receive_state(&session.root, &session.state).unwrap();
        drop(session);

        let mut resumed = prepare_receive_session(&config_path, &config, manifest.clone()).unwrap();
        let checkpoint = receive_chunk(
            &mut resumed,
            nexkvm_streaming::TransferChunk {
                transfer_id: manifest.id,
                file_index: 0,
                offset: 0,
                plain_len: 4,
                compression: nexkvm_streaming::TransferCompression::None,
                final_chunk_for_file: true,
                payload: bytes::Bytes::from_static(b"data"),
            },
        )
        .await
        .unwrap();
        assert_eq!(checkpoint.offset, 4);
        assert_eq!(fs::read(resumed.root.join("resume.bin")).unwrap(), b"data");
    }

    #[tokio::test]
    async fn nonzero_resume_hashes_the_existing_prefix_before_publication() {
        let temp = TempDir::new().unwrap();
        let downloads = temp.path().join("downloads");
        let mut config = enabled_config();
        config.download_dir = Some(downloads.display().to_string());
        let manifest = TransferManifest::new(
            TransferId::generate(),
            DeviceId::generate(),
            Some(DeviceId::generate()),
            TransferSource::Picker,
            vec![TransferEntry::file("resume.bin", 6, digest(b"abcdef")).unwrap()],
        )
        .unwrap();
        let config_path = temp.path().join("config.toml");
        let mut session = prepare_receive_session(&config_path, &config, manifest.clone()).unwrap();

        let first = receive_chunk(
            &mut session,
            nexkvm_streaming::TransferChunk {
                transfer_id: manifest.id,
                file_index: 0,
                offset: 0,
                plain_len: 3,
                compression: nexkvm_streaming::TransferCompression::None,
                final_chunk_for_file: false,
                payload: bytes::Bytes::from_static(b"abc"),
            },
        )
        .await
        .unwrap();
        assert_eq!(first.offset, 3);
        drop(session);

        let mut resumed = prepare_receive_session(&config_path, &config, manifest.clone()).unwrap();
        assert_eq!(resumed.state.checkpoint(manifest.id), Some(first));
        let terminal = receive_chunk(
            &mut resumed,
            nexkvm_streaming::TransferChunk {
                transfer_id: manifest.id,
                file_index: 0,
                offset: 3,
                plain_len: 3,
                compression: nexkvm_streaming::TransferCompression::None,
                final_chunk_for_file: true,
                payload: bytes::Bytes::from_static(b"def"),
            },
        )
        .await
        .unwrap();

        assert_eq!(terminal.offset, 6);
        assert_eq!(
            fs::read(resumed.root.join("resume.bin")).unwrap(),
            b"abcdef"
        );
        assert!(!resumed.root.join("resume.bin.part").exists());
    }

    #[tokio::test]
    async fn empty_file_is_digest_verified_and_published() {
        let temp = TempDir::new().unwrap();
        let downloads = temp.path().join("downloads");
        let mut config = enabled_config();
        config.download_dir = Some(downloads.display().to_string());
        let manifest = TransferManifest::new(
            TransferId::generate(),
            DeviceId::generate(),
            Some(DeviceId::generate()),
            TransferSource::Picker,
            vec![TransferEntry::file("empty.bin", 0, digest(b"")).unwrap()],
        )
        .unwrap();
        let config_path = temp.path().join("config.toml");
        let mut session = prepare_receive_session(&config_path, &config, manifest.clone()).unwrap();

        let checkpoint = receive_chunk(
            &mut session,
            nexkvm_streaming::TransferChunk {
                transfer_id: manifest.id,
                file_index: 0,
                offset: 0,
                plain_len: 0,
                compression: nexkvm_streaming::TransferCompression::None,
                final_chunk_for_file: true,
                payload: bytes::Bytes::new(),
            },
        )
        .await
        .unwrap();

        assert_eq!(checkpoint, terminal_checkpoint(&manifest).unwrap());
        assert_eq!(fs::read(session.root.join("empty.bin")).unwrap(), b"");
    }

    #[test]
    fn reconciliation_quarantines_same_size_final_with_the_wrong_digest() {
        let temp = TempDir::new().unwrap();
        let downloads = temp.path().join("downloads");
        let mut config = enabled_config();
        config.download_dir = Some(downloads.display().to_string());
        let manifest = TransferManifest::new(
            TransferId::generate(),
            DeviceId::generate(),
            Some(DeviceId::generate()),
            TransferSource::Picker,
            vec![TransferEntry::file("stale.bin", 4, digest(b"good")).unwrap()],
        )
        .unwrap();
        let config_path = temp.path().join("config.toml");
        let mut initial = prepare_receive_session(&config_path, &config, manifest.clone()).unwrap();
        fs::write(initial.root.join("stale.bin"), b"evil").unwrap();
        initial
            .state
            .set_checkpoint(terminal_checkpoint(&manifest).unwrap());
        initial.state.complete = true;
        persist_receive_state(&initial.root, &initial.state).unwrap();
        let root = initial.root.clone();
        drop(initial);

        let reconciled = prepare_receive_session(&config_path, &config, manifest).unwrap();

        assert!(!reconciled.state.complete);
        assert!(reconciled.state.checkpoint.is_none());
        assert!(!root.join("stale.bin").exists());
        assert!(fs::read_dir(root).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("stale.bin.corrupt-")
        }));
    }

    #[test]
    fn reconciliation_never_finalizes_a_full_part_with_the_wrong_digest() {
        let temp = TempDir::new().unwrap();
        let downloads = temp.path().join("downloads");
        let mut config = enabled_config();
        config.download_dir = Some(downloads.display().to_string());
        let manifest = TransferManifest::new(
            TransferId::generate(),
            DeviceId::generate(),
            Some(DeviceId::generate()),
            TransferSource::Picker,
            vec![TransferEntry::file("stale-part.bin", 4, digest(b"good")).unwrap()],
        )
        .unwrap();
        let config_path = temp.path().join("config.toml");
        let initial = prepare_receive_session(&config_path, &config, manifest.clone()).unwrap();
        let mut writer = TransferPartWriter::create(&initial.root, "stale-part.bin", 0, 4).unwrap();
        writer
            .write_raw_chunk(&nexkvm_streaming::TransferChunk {
                transfer_id: manifest.id,
                file_index: 0,
                offset: 0,
                plain_len: 4,
                compression: nexkvm_streaming::TransferCompression::None,
                final_chunk_for_file: true,
                payload: bytes::Bytes::from_static(b"evil"),
            })
            .unwrap();
        writer.flush().unwrap();
        drop(writer);
        let root = initial.root.clone();
        drop(initial);

        let reconciled = prepare_receive_session(&config_path, &config, manifest).unwrap();

        assert!(reconciled.state.checkpoint.is_none());
        assert!(!root.join("stale-part.bin").exists());
        assert!(!root.join("stale-part.bin.part").exists());
        assert!(fs::read_dir(root).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("stale-part.bin.part.corrupt-")
        }));
    }

    #[test]
    fn oversized_receive_state_is_rejected_before_toml_decode() {
        let temp = TempDir::new().unwrap();
        let downloads = temp.path().join("downloads");
        let mut config = enabled_config();
        config.download_dir = Some(downloads.display().to_string());
        let manifest = TransferManifest::new(
            TransferId::generate(),
            DeviceId::generate(),
            Some(DeviceId::generate()),
            TransferSource::Picker,
            vec![TransferEntry::file("payload.bin", 1, digest(b"x")).unwrap()],
        )
        .unwrap();
        let config_path = temp.path().join("config.toml");
        let session = prepare_receive_session(&config_path, &config, manifest).unwrap();
        OpenOptions::new()
            .write(true)
            .open(session.root.join(RECEIVE_STATE_FILE))
            .unwrap()
            .set_len(64 * 1024 + 1)
            .unwrap();

        let error = read_receive_state(&session.root).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("receive state exceeds the 65536-byte limit")
        );
    }

    #[tokio::test]
    async fn tampered_final_chunk_is_quarantined_without_publishing_a_file() {
        let temp = TempDir::new().unwrap();
        let downloads = temp.path().join("downloads");
        let mut config = enabled_config();
        config.download_dir = Some(downloads.display().to_string());
        let sender = DeviceId::generate();
        let receiver = DeviceId::generate();
        let manifest = TransferManifest::new(
            TransferId::generate(),
            sender,
            Some(receiver),
            TransferSource::Picker,
            vec![TransferEntry::file("tampered.bin", 4, digest(b"good")).unwrap()],
        )
        .unwrap();
        let config_path = temp.path().join("config.toml");
        let mut session = prepare_receive_session(&config_path, &config, manifest.clone()).unwrap();

        let error = receive_chunk(
            &mut session,
            nexkvm_streaming::TransferChunk {
                transfer_id: manifest.id,
                file_index: 0,
                offset: 0,
                plain_len: 4,
                compression: nexkvm_streaming::TransferCompression::None,
                final_chunk_for_file: true,
                payload: bytes::Bytes::from_static(b"evil"),
            },
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("digest mismatch"));
        assert!(!session.root.join("tampered.bin").exists());
        assert!(!session.root.join("tampered.bin.part").exists());
        assert!(
            fs::read_dir(&session.root).unwrap().any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("tampered.bin.part.corrupt-")
            }),
            "mismatched bytes must be retained only under a quarantine name"
        );
        assert!(session.state.checkpoint.is_none());
    }

    #[tokio::test]
    async fn same_size_source_mutation_never_leaves_a_corrupt_receiver_final() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("mutable-receiver.bin");
        fs::write(&source, vec![b'a'; RUNTIME_CHUNK_SIZE * 2]).unwrap();
        let sender_config_path = temp.path().join("sender/config.toml");
        let receiver_config_path = temp.path().join("receiver/config.toml");
        let downloads = temp.path().join("downloads");
        let mut sender_config = enabled_config();
        sender_config.max_transfer_bytes = (RUNTIME_CHUNK_SIZE * 3) as u64;
        let mut receiver_config = sender_config.clone();
        receiver_config.download_dir = Some(downloads.display().to_string());
        let transfer_id = enqueue_paths(
            &sender_config_path,
            &sender_config,
            std::slice::from_ref(&source),
        )
        .unwrap();
        let record = load_oldest_queue_entry(&sender_config_path, &sender_config)
            .unwrap()
            .unwrap();
        let sender_id = DeviceId::generate();
        let receiver_id = DeviceId::generate();
        let sender_runtime = runtime(sender_config_path, sender_config, sender_id);
        let receiver_runtime = runtime(receiver_config_path, receiver_config, receiver_id);
        let (sender_inner, receiver_connection) = memory_connection_pair();
        let sender_connection = MutatingConnection {
            inner: sender_inner,
            source,
            mutated: AtomicBool::new(false),
        };
        let sender_permit = Arc::clone(&sender_runtime.active_transfer)
            .try_acquire_owned()
            .unwrap();
        let receiver_permit = Arc::clone(&receiver_runtime.active_transfer)
            .try_acquire_owned()
            .unwrap();

        let sender = run_outbound(
            &sender_connection,
            &sender_runtime,
            receiver_id,
            &record,
            sender_permit,
        );
        let receiver = async {
            let offer = decode_envelope(receiver_connection.recv().await.unwrap()).unwrap();
            let FileTransferMessage::Offer(manifest) = offer else {
                panic!("expected offer")
            };
            run_inbound(
                &receiver_connection,
                &receiver_runtime,
                sender_id,
                manifest,
                receiver_permit,
            )
            .await
        };
        let (sent, received) = tokio::join!(sender, receiver);

        assert!(matches!(sent.unwrap(), OutboundResult::Retained));
        received.unwrap();
        assert!(record.path.exists());
        let root = downloads.join("NexKVM").join(transfer_id.0.to_string());
        assert!(!root.join("mutable-receiver.bin").exists());
        assert!(fs::read_dir(root).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("mutable-receiver.bin.part.corrupt-")
        }));
    }

    #[tokio::test]
    async fn rejection_and_nonterminal_messages_never_consume_the_queue() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("retry.txt");
        fs::write(&source, b"retry").unwrap();
        let config_path = temp.path().join("sender/config.toml");
        let config = enabled_config();
        enqueue_paths(&config_path, &config, &[source]).unwrap();
        let record = load_oldest_queue_entry(&config_path, &config)
            .unwrap()
            .unwrap();
        let sender_id = DeviceId::generate();
        let receiver_id = DeviceId::generate();
        let sender_runtime = runtime(config_path.clone(), config, sender_id);
        let (sender_connection, receiver_connection) = memory_connection_pair();
        let permit = Arc::clone(&sender_runtime.active_transfer)
            .try_acquire_owned()
            .unwrap();

        let sender = run_outbound(
            &sender_connection,
            &sender_runtime,
            receiver_id,
            &record,
            permit,
        );
        let receiver = async {
            let offer = decode_envelope(receiver_connection.recv().await.unwrap()).unwrap();
            let FileTransferMessage::Offer(manifest) = offer else {
                panic!("expected offer")
            };
            send_message(
                &receiver_connection,
                FileTransferMessage::Reject {
                    transfer_id: manifest.id,
                    reason: "policy".into(),
                },
            )
            .await
            .unwrap();
        };
        let (sent, ()) = tokio::join!(sender, receiver);
        assert!(matches!(sent.unwrap(), OutboundResult::Retained));
        assert!(record.path.exists());
    }

    #[tokio::test]
    async fn simultaneous_offers_have_one_deterministic_winner_without_livelock() {
        let temp = TempDir::new().unwrap();
        let source_a = temp.path().join("from-a.txt");
        let source_b = temp.path().join("from-b.txt");
        fs::write(&source_a, b"a wins").unwrap();
        fs::write(&source_b, b"b waits").unwrap();
        let config_path_a = temp.path().join("a/config.toml");
        let config_path_b = temp.path().join("b/config.toml");
        let mut config_a = enabled_config();
        let mut config_b = enabled_config();
        config_a.download_dir = Some(temp.path().join("downloads-a").display().to_string());
        config_b.download_dir = Some(temp.path().join("downloads-b").display().to_string());
        let id_a = enqueue_paths(&config_path_a, &config_a, &[source_a]).unwrap();
        enqueue_paths(&config_path_b, &config_b, &[source_b]).unwrap();
        let record_a = load_oldest_queue_entry(&config_path_a, &config_a)
            .unwrap()
            .unwrap();
        let record_b = load_oldest_queue_entry(&config_path_b, &config_b)
            .unwrap()
            .unwrap();
        let device_a = DeviceId(uuid::Uuid::from_u128(1));
        let device_b = DeviceId(uuid::Uuid::from_u128(2));
        let runtime_a = runtime(config_path_a.clone(), config_a, device_a);
        let runtime_b = runtime(config_path_b.clone(), config_b, device_b);
        let (connection_a, connection_b) = memory_connection_pair();
        let permit_a = Arc::clone(&runtime_a.active_transfer)
            .try_acquire_owned()
            .unwrap();
        let permit_b = Arc::clone(&runtime_b.active_transfer)
            .try_acquire_owned()
            .unwrap();

        let results = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(
                run_outbound(&connection_a, &runtime_a, device_b, &record_a, permit_a,),
                run_outbound(&connection_b, &runtime_b, device_a, &record_b, permit_b,)
            )
        })
        .await
        .expect("collision resolution must make bounded progress");
        assert!(matches!(results.0.unwrap(), OutboundResult::Acknowledged));
        assert!(matches!(results.1.unwrap(), OutboundResult::Retained));
        assert!(
            load_oldest_queue_entry(&config_path_a, &runtime_a.config)
                .unwrap()
                .is_none()
        );
        assert!(
            record_b.path.exists(),
            "collision loser queue must remain durable"
        );
        assert_eq!(
            fs::read(
                temp.path()
                    .join("downloads-b/NexKVM")
                    .join(id_a.0.to_string())
                    .join("from-a.txt")
            )
            .unwrap(),
            b"a wins"
        );
    }

    #[tokio::test]
    async fn source_mutation_during_stream_cancels_before_complete_and_retains_queue() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("mutable.bin");
        fs::write(&source, vec![b'a'; RUNTIME_CHUNK_SIZE * 2]).unwrap();
        let config_path = temp.path().join("sender/config.toml");
        let mut config = enabled_config();
        config.max_transfer_bytes = (RUNTIME_CHUNK_SIZE * 3) as u64;
        enqueue_paths(&config_path, &config, std::slice::from_ref(&source)).unwrap();
        let record = load_oldest_queue_entry(&config_path, &config)
            .unwrap()
            .unwrap();
        let sender_id = DeviceId::generate();
        let receiver_id = DeviceId::generate();
        let sender_runtime = runtime(config_path, config, sender_id);
        let (sender_connection, receiver_connection) = memory_connection_pair();
        let permit = Arc::clone(&sender_runtime.active_transfer)
            .try_acquire_owned()
            .unwrap();

        let sender = run_outbound(
            &sender_connection,
            &sender_runtime,
            receiver_id,
            &record,
            permit,
        );
        let receiver = async {
            let offer = decode_envelope(receiver_connection.recv().await.unwrap()).unwrap();
            let FileTransferMessage::Offer(manifest) = offer else {
                panic!("expected offer")
            };
            send_message(
                &receiver_connection,
                FileTransferMessage::Accept {
                    transfer_id: manifest.id,
                    checkpoint: None,
                },
            )
            .await
            .unwrap();

            let first = decode_envelope(receiver_connection.recv().await.unwrap()).unwrap();
            let FileTransferMessage::Chunk(first) = first else {
                panic!("expected first chunk")
            };
            assert_eq!(first.offset, 0);
            let mut source_file = OpenOptions::new().write(true).open(&source).unwrap();
            source_file
                .seek(SeekFrom::Start(RUNTIME_CHUNK_SIZE as u64))
                .unwrap();
            source_file
                .write_all(&vec![b'b'; RUNTIME_CHUNK_SIZE])
                .unwrap();
            source_file.sync_all().unwrap();
            let first_checkpoint =
                checkpoint_after_chunk(&manifest, 0, u64::from(first.plain_len)).unwrap();
            send_message(
                &receiver_connection,
                FileTransferMessage::Checkpoint(first_checkpoint),
            )
            .await
            .unwrap();
            assert!(matches!(
                decode_envelope(receiver_connection.recv().await.unwrap()).unwrap(),
                FileTransferMessage::Ack(actual) if actual == first_checkpoint
            ));

            let second = decode_envelope(receiver_connection.recv().await.unwrap()).unwrap();
            let FileTransferMessage::Chunk(second) = second else {
                panic!("expected second chunk")
            };
            let second_checkpoint =
                checkpoint_after_chunk(&manifest, 0, second.offset + u64::from(second.plain_len))
                    .unwrap();
            send_message(
                &receiver_connection,
                FileTransferMessage::Checkpoint(second_checkpoint),
            )
            .await
            .unwrap();
            assert!(matches!(
                decode_envelope(receiver_connection.recv().await.unwrap()).unwrap(),
                FileTransferMessage::Ack(actual) if actual == second_checkpoint
            ));
            assert!(matches!(
                decode_envelope(receiver_connection.recv().await.unwrap()).unwrap(),
                FileTransferMessage::Cancel { transfer_id, .. } if transfer_id == manifest.id
            ));
        };
        let (sent, ()) = tokio::join!(sender, receiver);
        assert!(matches!(sent.unwrap(), OutboundResult::Retained));
        assert!(record.path.exists());
    }

    #[tokio::test]
    async fn resume_prefix_is_included_in_source_digest_verification() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("resume-source.bin");
        fs::write(&source, vec![b'a'; RUNTIME_CHUNK_SIZE * 2]).unwrap();
        let config_path = temp.path().join("sender/config.toml");
        let mut config = enabled_config();
        config.max_transfer_bytes = (RUNTIME_CHUNK_SIZE * 3) as u64;
        enqueue_paths(&config_path, &config, std::slice::from_ref(&source)).unwrap();
        let record = load_oldest_queue_entry(&config_path, &config)
            .unwrap()
            .unwrap();
        let mut source_file = OpenOptions::new().write(true).open(&source).unwrap();
        source_file.seek(SeekFrom::Start(0)).unwrap();
        source_file.write_all(b"changed!").unwrap();
        source_file.sync_all().unwrap();

        let sender_id = DeviceId::generate();
        let receiver_id = DeviceId::generate();
        let sender_runtime = runtime(config_path, config, sender_id);
        let (sender_connection, receiver_connection) = memory_connection_pair();
        let permit = Arc::clone(&sender_runtime.active_transfer)
            .try_acquire_owned()
            .unwrap();
        let sender = run_outbound(
            &sender_connection,
            &sender_runtime,
            receiver_id,
            &record,
            permit,
        );
        let receiver = async {
            let offer = decode_envelope(receiver_connection.recv().await.unwrap()).unwrap();
            let FileTransferMessage::Offer(manifest) = offer else {
                panic!("expected offer")
            };
            let resume = TransferCheckpoint {
                id: manifest.id,
                file_index: 0,
                offset: RUNTIME_CHUNK_SIZE as u64,
                transferred_bytes: RUNTIME_CHUNK_SIZE as u64,
            };
            send_message(
                &receiver_connection,
                FileTransferMessage::Accept {
                    transfer_id: manifest.id,
                    checkpoint: Some(resume),
                },
            )
            .await
            .unwrap();
            let chunk = decode_envelope(receiver_connection.recv().await.unwrap()).unwrap();
            let FileTransferMessage::Chunk(chunk) = chunk else {
                panic!("expected resumed chunk")
            };
            let checkpoint =
                checkpoint_after_chunk(&manifest, 0, chunk.offset + u64::from(chunk.plain_len))
                    .unwrap();
            send_message(
                &receiver_connection,
                FileTransferMessage::Checkpoint(checkpoint),
            )
            .await
            .unwrap();
            let _ack = receiver_connection.recv().await.unwrap();
            assert!(matches!(
                decode_envelope(receiver_connection.recv().await.unwrap()).unwrap(),
                FileTransferMessage::Cancel { transfer_id, .. } if transfer_id == manifest.id
            ));
        };
        let (sent, ()) = tokio::join!(sender, receiver);
        assert!(matches!(sent.unwrap(), OutboundResult::Retained));
        assert!(record.path.exists());
    }

    #[tokio::test]
    async fn silent_peer_receive_is_bounded_by_protocol_timeout() {
        let (connection, peer) = memory_connection_pair();
        let _keep_peer_alive = &peer;
        let error = recv_message_with_timeout(&connection, Duration::from_millis(10))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("no protocol progress"));
    }
}
