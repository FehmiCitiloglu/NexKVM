//! Reliable, ordered streaming for bulk and media payloads.
//!
//! The event bus is intentionally lossy (freshness over completeness) and suits
//! real-time input. Use-cases that need *every byte in order* — file/drag-drop
//! transfer, follow-mouse audio, future screen streaming — flow here instead,
//! over a dedicated QUIC stream/datagram lane rather than the broadcast bus.
//!
//! This crate now also includes the file transfer foundation used by drag/drop
//! and picker-initiated file/folder transfer:
//! - Transfer manifest and entry model.
//! - Background transfer queue with progress snapshots.
//! - Resume checkpoints for interrupted transfers.
//! - Chunked large-file streaming.
//! - Optional chunk compression and required encryption boundary.
//!
//! Audio routing is modeled here too: follow-mouse audio, shared headset mode,
//! device switching, frame metadata, and the platform [`AudioBackend`] boundary.
//!
//! Screen streaming is modeled as a low-latency encrypted media lane: GPU-aware
//! capture/encoding capability negotiation, hardware encoder selection
//! (NVENC/VAAPI/VideoToolbox), mini previews, window peeking, and instant app
//! preview session planning. Platform-specific capture APIs remain behind the
//! safe [`ScreenCaptureBackend`] trait.

use async_trait::async_trait;
use bytes::Bytes;
use thiserror::Error;

mod audio;
mod audio_sync;
mod file_transfer_cipher;
mod file_transfer_compression;
mod file_transfer_queue;
mod file_transfer_reassembly;
mod file_transfer_session;
mod file_transfer_types;
mod preview;
mod screen;

pub use audio::{
    AudioBackend, AudioCodec, AudioDevice, AudioDeviceId, AudioDeviceProfile, AudioDeviceRole,
    AudioFormat, AudioFrame, AudioRoute, AudioRouteMode, AudioRouter, SampleFormat,
};
pub use audio_sync::{AudioJitterBuffer, JitterConfig, JitterOutput, JitterStats, PushOutcome};
pub use file_transfer_cipher::{PlaintextTransferCipher, SessionTransferCipher, TransferCipher};
pub use file_transfer_compression::{
    CompressionStrategy, TransferCompression, TransferCompressionPolicy,
    compress as compress_transfer_bytes,
    compress_with_policy as compress_transfer_bytes_with_policy,
    decompress as decompress_transfer_bytes,
};
pub use file_transfer_queue::{QueueState, QueuedTransfer, TransferProgress, TransferQueue};
pub use file_transfer_reassembly::{CompletedFile, TransferReassembler};
pub use file_transfer_session::{
    DecodedChunk, TransferCheckpoint, TransferChunk, TransferReceiver, TransferSender,
};
pub use file_transfer_types::{
    TransferEntry, TransferFileData, TransferId, TransferManifest, TransferSource,
};
pub use preview::{HoverPreviewController, PreviewDecision, PreviewPolicy};
pub use screen::{
    CaptureSource, CaptureSourceId, EncodedScreenFrame, FrameDependency, GpuMemoryKind,
    HardwareEncoder, PixelFormat, ScreenCaptureBackend, ScreenCodec, ScreenEncoderBackend,
    ScreenFeatureSet, ScreenFrame, ScreenFrameType, ScreenPermissions, ScreenQualityPreset,
    ScreenResolution, ScreenStreamCapabilities, ScreenStreamIntent, ScreenStreamPlan,
    ScreenStreamRequest, WindowVisibility, negotiate_screen_stream,
};

/// Errors from audio routing, frame codec, and platform backend operations.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AudioError {
    /// Codec decode/encode failure.
    #[error("audio codec error: {0}")]
    Codec(String),

    /// Platform backend failure.
    #[error("audio backend error: {0}")]
    Backend(String),

    /// Requested audio device is not available.
    #[error("audio device unavailable: {0}")]
    DeviceUnavailable(String),

    /// Requested operation is unsupported on the current backend/session.
    #[error("unsupported audio capability: {0}")]
    Unsupported(&'static str),

    /// Data exceeds configured/wire limit.
    #[error("audio payload too large: {size} bytes (limit {limit})")]
    TooLarge {
        /// Actual size.
        size: usize,
        /// Limit.
        limit: usize,
    },
}

/// Errors from file transfer model/codec/session operations.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum TransferError {
    /// Manifest cannot be empty.
    #[error("transfer manifest is empty")]
    EmptyManifest,

    /// Unsafe or invalid relative path entry.
    #[error("invalid transfer path: {0}")]
    InvalidPath(String),

    /// Codec decode/encode failure.
    #[error("transfer codec error: {0}")]
    Codec(String),

    /// Compression backend failure.
    #[error("transfer compression error: {0}")]
    Compression(String),

    /// Encryption backend/authentication failure.
    #[error("transfer encryption error: {0}")]
    Encryption(String),

    /// Feature not compiled in.
    #[error("unsupported transfer capability: {0}")]
    Unsupported(&'static str),

    /// Data exceeds configured/wire limit.
    #[error("transfer payload too large: {size} bytes (limit {limit})")]
    TooLarge {
        /// Actual size.
        size: usize,
        /// Limit.
        limit: usize,
    },
}

/// Errors from screen capture, encoding, and stream planning.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ScreenError {
    /// Capture or encoder backend failed.
    #[error("screen backend error: {0}")]
    Backend(String),

    /// OS permission is missing or denied.
    #[error("screen permission denied: {0}")]
    PermissionDenied(&'static str),

    /// Requested source is not available.
    #[error("screen source unavailable: {0}")]
    SourceUnavailable(String),

    /// No mutually supported codec/encoder/capture mode exists.
    #[error("screen capability mismatch: {0}")]
    CapabilityMismatch(&'static str),

    /// Codec encode/decode failure.
    #[error("screen codec error: {0}")]
    Codec(String),

    /// Data exceeds configured/wire limit.
    #[error("screen payload too large: {size} bytes (limit {limit})")]
    TooLarge {
        /// Actual size.
        size: usize,
        /// Limit.
        limit: usize,
    },
}

/// Errors from a stream.
#[derive(Debug, Error)]
pub enum StreamError {
    /// The stream ended unexpectedly.
    #[error("stream closed")]
    Closed,

    /// Backend transport error.
    #[error("stream backend error: {0}")]
    Backend(String),
}

/// Logical purpose of a stream, used to pick a transport lane and priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    /// Bulk file / drag-and-drop transfer (reliable, ordered).
    File,
    /// Low-latency audio (loss-tolerant, real-time).
    Audio,
    /// Low-latency screen/video media.
    Screen,
}

/// An ordered byte stream to a peer.
#[async_trait]
pub trait Stream: Send + Sync {
    /// The stream's purpose.
    fn kind(&self) -> StreamKind;

    /// Write the next chunk.
    ///
    /// # Errors
    /// Returns [`StreamError`] if the stream is closed or the backend fails.
    async fn write_chunk(&self, chunk: Bytes) -> Result<(), StreamError>;

    /// Read the next chunk, or `None` at end of stream.
    ///
    /// # Errors
    /// Returns [`StreamError`] on backend failure.
    async fn read_chunk(&self) -> Result<Option<Bytes>, StreamError>;
}
