//! Cross-platform clipboard content model.
//!
//! A real clipboard holds the *same logical selection* in several
//! representations at once (e.g. styled HTML plus a plain-text fallback, or a
//! PNG plus a TIFF). [`ClipboardSnapshot`] captures that multi-format reality so
//! the receiver can pick the richest representation it understands, while
//! [`ClipboardContent`] is a single MIME-tagged representation.
//!
//! # Wire codec
//! Snapshots cross the wire as a compact, dependency-free binary layout (no
//! base64 blow-up for images, unlike JSON). All length/count fields are
//! validated against the remaining buffer on decode — peer input is never
//! trusted.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use bytes::{Buf, BufMut, Bytes, BytesMut};
use serde::{Deserialize, Serialize};

use crate::ClipboardError;

/// Hard caps on a decoded snapshot to bound memory from a hostile peer.
const MAX_FORMATS: usize = 32;
/// Maximum total decoded payload (64 MiB) across all formats.
const MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;

/// Coarse classification of a clipboard representation, derived from its MIME.
///
/// Used to drive policy decisions (what compresses well, what to prefer when
/// applying) without string-matching MIME types all over the codebase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ClipboardFormat {
    /// `text/plain` — the universal fallback.
    PlainText,
    /// `text/html` — rich/styled text.
    Html,
    /// `text/rtf` / `application/rtf` — rich text.
    Rtf,
    /// `image/*` — raster image (PNG/JPEG/…).
    Image,
    /// `text/uri-list` — file references (drag-and-drop adjacent).
    Files,
    /// Anything else (vendor-specific pasteboard types, etc.).
    Other,
}

impl ClipboardFormat {
    /// Classify a MIME type string.
    #[must_use]
    pub fn from_mime(mime: &str) -> Self {
        let base = mime.split(';').next().unwrap_or(mime).trim();
        match base {
            "text/plain" => Self::PlainText,
            "text/html" => Self::Html,
            "text/rtf" | "application/rtf" => Self::Rtf,
            "text/uri-list" => Self::Files,
            _ if base.starts_with("image/") => Self::Image,
            _ => Self::Other,
        }
    }

    /// Whether payloads of this format are typically already compressed, so a
    /// generic compressor would only add overhead.
    #[must_use]
    pub fn is_precompressed(self) -> bool {
        matches!(self, Self::Image)
    }
}

/// A single MIME-tagged clipboard representation.
///
/// The body is [`Bytes`] for zero-copy handoff between the platform backend,
/// the sync pipeline, and the streaming transport.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardContent {
    /// MIME type (e.g. `text/plain;charset=utf-8`, `image/png`).
    pub mime: String,
    /// Raw content bytes.
    pub data: Bytes,
}

impl ClipboardContent {
    /// Construct UTF-8 plain-text content.
    #[must_use]
    pub fn text(s: impl Into<String>) -> Self {
        Self {
            mime: "text/plain;charset=utf-8".into(),
            data: Bytes::from(s.into().into_bytes()),
        }
    }

    /// Construct HTML (rich text) content.
    #[must_use]
    pub fn html(s: impl Into<String>) -> Self {
        Self {
            mime: "text/html;charset=utf-8".into(),
            data: Bytes::from(s.into().into_bytes()),
        }
    }

    /// Construct RTF (rich text) content.
    #[must_use]
    pub fn rtf(bytes: impl Into<Bytes>) -> Self {
        Self {
            mime: "text/rtf".into(),
            data: bytes.into(),
        }
    }

    /// Construct PNG image content.
    #[must_use]
    pub fn image_png(bytes: impl Into<Bytes>) -> Self {
        Self {
            mime: "image/png".into(),
            data: bytes.into(),
        }
    }

    /// The coarse format classification.
    #[must_use]
    pub fn format(&self) -> ClipboardFormat {
        ClipboardFormat::from_mime(&self.mime)
    }

    /// Interpret the body as UTF-8 text, if it is a text format and valid UTF-8.
    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        matches!(
            self.format(),
            ClipboardFormat::PlainText | ClipboardFormat::Html | ClipboardFormat::Files
        )
        .then(|| std::str::from_utf8(&self.data).ok())
        .flatten()
    }

    /// Whether this representation is marked sensitive by a password manager
    /// or OS "concealed" hint, and so should not be persisted to history.
    ///
    /// Recognizes the de-facto hints used across platforms:
    /// - Linux/KDE: `x-kde-passwordManagerHint`
    /// - macOS: `org.nspasteboard.ConcealedType`
    /// - generic: any MIME mentioning `password`/`secret`/`concealed`.
    #[must_use]
    pub fn is_concealed(&self) -> bool {
        let m = self.mime.to_ascii_lowercase();
        m.contains("passwordmanagerhint")
            || m.contains("concealed")
            || m.contains("secret")
            || m.contains("password")
    }
}

/// A non-cryptographic fingerprint of clipboard state.
///
/// Used for **echo suppression** and **history dedup** only. It is a SipHash
/// digest (via [`DefaultHasher`]) — fast and collision-resistant enough for
/// equality checks, but **never** used for authentication or integrity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ContentFingerprint(pub u64);

/// The full multi-format state of the clipboard at one instant.
///
/// Formats are stored sorted by MIME so fingerprints are order-independent and
/// equality is well-defined regardless of platform enumeration order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardSnapshot {
    formats: Vec<ClipboardContent>,
}

impl ClipboardSnapshot {
    /// Build a snapshot from representations, de-duplicating by MIME (first
    /// occurrence wins) and sorting for a canonical ordering.
    #[must_use]
    pub fn new(mut formats: Vec<ClipboardContent>) -> Self {
        formats.sort_by(|a, b| a.mime.cmp(&b.mime));
        formats.dedup_by(|a, b| a.mime == b.mime);
        Self { formats }
    }

    /// Convenience: a text-only snapshot.
    #[must_use]
    pub fn from_text(s: impl Into<String>) -> Self {
        Self::new(vec![ClipboardContent::text(s)])
    }

    /// The contained representations (canonical order).
    #[must_use]
    pub fn formats(&self) -> &[ClipboardContent] {
        &self.formats
    }

    /// First representation matching `format`, if any.
    #[must_use]
    pub fn get(&self, format: ClipboardFormat) -> Option<&ClipboardContent> {
        self.formats.iter().find(|c| c.format() == format)
    }

    /// Best available plain-text rendering (prefers `text/plain`).
    #[must_use]
    pub fn best_text(&self) -> Option<&str> {
        self.get(ClipboardFormat::PlainText)
            .and_then(ClipboardContent::as_text)
            .or_else(|| self.formats.iter().find_map(ClipboardContent::as_text))
    }

    /// Whether the snapshot carries no representations.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.formats.is_empty()
    }

    /// Total byte size across all representations.
    #[must_use]
    pub fn total_len(&self) -> usize {
        self.formats.iter().map(|c| c.data.len()).sum()
    }

    /// Whether any representation is marked concealed/secret.
    #[must_use]
    pub fn is_concealed(&self) -> bool {
        self.formats.iter().any(ClipboardContent::is_concealed)
    }

    /// Whether the snapshot is dominated by precompressed (image) bytes, used
    /// by the compression policy.
    #[must_use]
    pub fn is_mostly_precompressed(&self) -> bool {
        let total = self.total_len();
        if total == 0 {
            return false;
        }
        let pre: usize = self
            .formats
            .iter()
            .filter(|c| c.format().is_precompressed())
            .map(|c| c.data.len())
            .sum();
        pre * 2 > total
    }

    /// Stable, order-independent fingerprint of the content.
    #[must_use]
    pub fn fingerprint(&self) -> ContentFingerprint {
        let mut hasher = DefaultHasher::new();
        self.formats.len().hash(&mut hasher);
        for c in &self.formats {
            c.mime.hash(&mut hasher);
            c.data.as_ref().hash(&mut hasher);
        }
        ContentFingerprint(hasher.finish())
    }

    /// Encode the snapshot to the compact binary wire layout.
    ///
    /// ```text
    /// u32 format_count
    /// repeat:
    ///   u32 mime_len, mime bytes (utf-8)
    ///   u32 data_len, data bytes
    /// ```
    ///
    /// # Errors
    /// Returns [`ClipboardError::TooLarge`] if a field exceeds `u32`/total caps.
    pub fn encode(&self) -> Result<Bytes, ClipboardError> {
        if self.formats.len() > MAX_FORMATS {
            return Err(ClipboardError::TooLarge {
                size: self.formats.len(),
                limit: MAX_FORMATS,
            });
        }
        let total = self.total_len();
        if total > MAX_TOTAL_BYTES {
            return Err(ClipboardError::TooLarge {
                size: total,
                limit: MAX_TOTAL_BYTES,
            });
        }

        let mut buf = BytesMut::with_capacity(4 + total + self.formats.len() * 8);
        buf.put_u32(self.formats.len() as u32);
        for c in &self.formats {
            let mime = c.mime.as_bytes();
            let mime_len = u32::try_from(mime.len()).map_err(|_| ClipboardError::TooLarge {
                size: mime.len(),
                limit: u32::MAX as usize,
            })?;
            let data_len = u32::try_from(c.data.len()).map_err(|_| ClipboardError::TooLarge {
                size: c.data.len(),
                limit: u32::MAX as usize,
            })?;
            buf.put_u32(mime_len);
            buf.put_slice(mime);
            buf.put_u32(data_len);
            buf.put_slice(&c.data);
        }
        Ok(buf.freeze())
    }

    /// Decode a snapshot from the binary wire layout.
    ///
    /// Every length/count is validated against the remaining buffer and the
    /// hard caps before allocating, so malformed or hostile input yields a
    /// [`ClipboardError::Codec`]/[`ClipboardError::TooLarge`] rather than a
    /// panic or huge allocation.
    ///
    /// # Errors
    /// Returns [`ClipboardError::Codec`] on truncated/invalid input or
    /// [`ClipboardError::TooLarge`] if the caps are exceeded.
    pub fn decode(mut buf: Bytes) -> Result<Self, ClipboardError> {
        if buf.remaining() < 4 {
            return Err(ClipboardError::Codec("truncated header".into()));
        }
        let count = buf.get_u32() as usize;
        if count > MAX_FORMATS {
            return Err(ClipboardError::TooLarge {
                size: count,
                limit: MAX_FORMATS,
            });
        }

        let mut formats = Vec::with_capacity(count);
        let mut running = 0usize;
        for _ in 0..count {
            let mime = read_field(&mut buf, MAX_TOTAL_BYTES)?;
            let data = read_field(&mut buf, MAX_TOTAL_BYTES)?;
            running = running.saturating_add(data.len());
            if running > MAX_TOTAL_BYTES {
                return Err(ClipboardError::TooLarge {
                    size: running,
                    limit: MAX_TOTAL_BYTES,
                });
            }
            let mime = String::from_utf8(mime.to_vec())
                .map_err(|_| ClipboardError::Codec("non-utf8 mime".into()))?;
            formats.push(ClipboardContent { mime, data });
        }
        if buf.has_remaining() {
            return Err(ClipboardError::Codec("trailing bytes".into()));
        }
        Ok(Self::new(formats))
    }
}

/// Read a `u32`-length-prefixed field, bounded by `max`, as a zero-copy slice.
fn read_field(buf: &mut Bytes, max: usize) -> Result<Bytes, ClipboardError> {
    if buf.remaining() < 4 {
        return Err(ClipboardError::Codec("truncated length".into()));
    }
    let len = buf.get_u32() as usize;
    if len > max || len > buf.remaining() {
        return Err(ClipboardError::Codec("field length out of range".into()));
    }
    Ok(buf.split_to(len))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_mime_types() {
        assert_eq!(
            ClipboardFormat::from_mime("text/plain;charset=utf-8"),
            ClipboardFormat::PlainText
        );
        assert_eq!(
            ClipboardFormat::from_mime("image/png"),
            ClipboardFormat::Image
        );
        assert_eq!(
            ClipboardFormat::from_mime("text/html"),
            ClipboardFormat::Html
        );
        assert_eq!(
            ClipboardFormat::from_mime("application/x-weird"),
            ClipboardFormat::Other
        );
    }

    #[test]
    fn snapshot_is_order_independent() {
        let a = ClipboardSnapshot::new(vec![
            ClipboardContent::text("hi"),
            ClipboardContent::html("<b>hi</b>"),
        ]);
        let b = ClipboardSnapshot::new(vec![
            ClipboardContent::html("<b>hi</b>"),
            ClipboardContent::text("hi"),
        ]);
        assert_eq!(a, b);
        assert_eq!(a.fingerprint(), b.fingerprint());
    }

    #[test]
    fn dedups_by_mime() {
        let s = ClipboardSnapshot::new(vec![
            ClipboardContent::text("first"),
            ClipboardContent::text("second"),
        ]);
        assert_eq!(s.formats().len(), 1);
    }

    #[test]
    fn best_text_prefers_plain() {
        let s = ClipboardSnapshot::new(vec![
            ClipboardContent::html("<b>x</b>"),
            ClipboardContent::text("x"),
        ]);
        assert_eq!(s.best_text(), Some("x"));
    }

    #[test]
    fn detects_concealed() {
        let s = ClipboardSnapshot::new(vec![ClipboardContent {
            mime: "x-kde-passwordManagerHint".into(),
            data: Bytes::from_static(b"secret"),
        }]);
        assert!(s.is_concealed());
    }

    #[test]
    fn binary_round_trips() {
        let s = ClipboardSnapshot::new(vec![
            ClipboardContent::text("hello world"),
            ClipboardContent::image_png(Bytes::from_static(&[0u8, 1, 2, 3, 255])),
        ]);
        let encoded = s.encode().unwrap();
        let decoded = ClipboardSnapshot::decode(encoded).unwrap();
        assert_eq!(s, decoded);
    }

    #[test]
    fn decode_rejects_truncated() {
        // count says 1 format but no field data follows.
        let mut buf = BytesMut::new();
        buf.put_u32(1);
        assert!(matches!(
            ClipboardSnapshot::decode(buf.freeze()),
            Err(ClipboardError::Codec(_))
        ));
    }

    #[test]
    fn decode_rejects_oversized_length() {
        let mut buf = BytesMut::new();
        buf.put_u32(1); // one format
        buf.put_u32(u32::MAX); // mime length far exceeds buffer
        assert!(matches!(
            ClipboardSnapshot::decode(buf.freeze()),
            Err(ClipboardError::Codec(_))
        ));
    }

    #[test]
    fn mostly_precompressed_detection() {
        let s = ClipboardSnapshot::new(vec![
            ClipboardContent::text("x"),
            ClipboardContent::image_png(Bytes::from(vec![0u8; 1000])),
        ]);
        assert!(s.is_mostly_precompressed());
    }
}
