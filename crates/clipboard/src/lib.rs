//! Clipboard synchronization.
//!
//! This crate owns the cross-platform clipboard model and the sans-IO sync
//! pipeline behind the protocol `Clipboard` message kind.
//! Platform pasteboard access (Wayland data-control/portals, macOS `NSPasteboard`,
//! Windows clipboard) lives in the `platform-*` crates behind the [`Clipboard`]
//! trait; everything else here is pure, testable logic.
//!
//! # Pieces
//! - [`ClipboardSnapshot`] / [`ClipboardContent`] — the multi-format content
//!   model (plain text, rich text, images, …) with a compact binary codec.
//! - [`compress`]/[`decompress`] — per-update sync compression (feature
//!   `compression`).
//! - [`ClipboardCipher`] — the encryption boundary; production wires an adapter
//!   over a `nexkvm-crypto` session. Plaintext never reaches the transport.
//! - [`ConflictResolver`] — last-writer-wins + echo suppression for concurrent
//!   copies across devices.
//! - [`ClipboardHistory`] — bounded, dedup'd, pinnable history.
//! - [`ClipboardSync`] / [`ClipboardUpdate`] — the wire message and the state
//!   machine tying it all together.
//!
//! # Platform notes
//! - **Wayland** clipboard access requires the `wlr-data-control` protocol or a
//!   portal plus a focused surface.
//! - **macOS/Windows** expose multi-format pasteboards; the explicit MIME type
//!   on each [`ClipboardContent`] preserves format negotiation.
//! - Large blobs (images, files) should travel on the streaming lane rather than
//!   the lossy event bus; this crate produces the framed payloads either lane
//!   can carry.

use async_trait::async_trait;
use thiserror::Error;

mod cipher;
mod compression;
mod conflict;
mod content;
mod engine;
mod history;
mod sync;
mod timeline;

pub use cipher::{ClipboardCipher, PlaintextCipher, SessionClipboardCipher};
pub use compression::{
    CompressionAlgorithm, CompressionPolicy, compress, decompress, decompress_bounded,
};
pub use conflict::{ConflictResolver, InboundDecision, LocalDecision, OriginStamp};
pub use content::{ClipboardContent, ClipboardFormat, ClipboardSnapshot, ContentFingerprint};
pub use engine::ClipboardEngine;
pub use history::{ClipboardHistory, HistoryConfig, HistoryEntry, SkipReason};
pub use sync::{ClipboardSync, ClipboardUpdate};
pub use timeline::{ClipboardRestorePlan, SharedClipboardTimeline, TimelineConfig, TimelineEntry};

/// Errors from clipboard access and synchronization.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ClipboardError {
    /// The platform denied clipboard access.
    #[error("clipboard access denied")]
    AccessDenied,

    /// The platform backend failed.
    #[error("clipboard backend error: {0}")]
    Backend(String),

    /// A payload failed to encode or decode (malformed/peer-supplied data).
    #[error("clipboard codec error: {0}")]
    Codec(String),

    /// Compression or decompression failed.
    #[error("clipboard compression error: {0}")]
    Compression(String),

    /// Sealing or opening (encryption) failed.
    #[error("clipboard encryption error: {0}")]
    Encryption(String),

    /// A required capability is not compiled in (e.g. a compression feature).
    #[error("unsupported clipboard capability: {0}")]
    Unsupported(&'static str),

    /// A field or payload exceeded its safety limit.
    #[error("clipboard payload too large: {size} bytes (limit {limit})")]
    TooLarge {
        /// Offending size.
        size: usize,
        /// Maximum allowed.
        limit: usize,
    },

    /// The logical sequence reached its maximum and cannot safely advance.
    #[error("clipboard logical clock exhausted; reconnect required")]
    ClockExhausted,
}

/// Platform clipboard access.
///
/// Implemented per platform. A read returns the full multi-format
/// [`ClipboardSnapshot`] so the sync layer can preserve rich/image content; a
/// write replaces the pasteboard with the provided representations.
#[async_trait]
pub trait Clipboard: Send + Sync {
    /// Read the current clipboard contents, if any.
    ///
    /// # Errors
    /// Returns [`ClipboardError`] on access/backend failure.
    async fn read(&self) -> Result<Option<ClipboardSnapshot>, ClipboardError>;

    /// Replace the clipboard contents.
    ///
    /// # Errors
    /// Returns [`ClipboardError`] on access/backend failure.
    async fn write(&self, snapshot: ClipboardSnapshot) -> Result<(), ClipboardError>;
}
