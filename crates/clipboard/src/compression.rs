//! Clipboard sync payload compression.
//!
//! Compression is negotiated **per update**: each [`ClipboardUpdate`] carries
//! the [`CompressionAlgorithm`] used, so a sender with the `compression` feature
//! still interoperates with a peer that lacks it (it sends [`None`] and decodes
//! whatever it can). The real codec ([`Deflate`]) is the pure-Rust
//! flate2/miniz_oxide backend, gated behind the `compression` feature so a
//! minimal build pulls in no extra dependencies.
//!
//! [`None`]: CompressionAlgorithm::None
//! [`Deflate`]: CompressionAlgorithm::Deflate
//! [`ClipboardUpdate`]: crate::ClipboardUpdate

use crate::ClipboardError;
use crate::content::ClipboardSnapshot;

/// Compression algorithm tag carried on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CompressionAlgorithm {
    /// Stored verbatim.
    None,
    /// Raw DEFLATE (RFC 1951).
    Deflate,
}

impl CompressionAlgorithm {
    /// Stable wire discriminant.
    #[must_use]
    pub fn as_u8(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Deflate => 1,
        }
    }

    /// Parse a wire discriminant.
    ///
    /// # Errors
    /// Returns [`ClipboardError::Codec`] for an unknown discriminant.
    pub fn from_u8(raw: u8) -> Result<Self, ClipboardError> {
        match raw {
            0 => Ok(Self::None),
            1 => Ok(Self::Deflate),
            other => Err(ClipboardError::Codec(format!(
                "unknown compression algorithm {other}"
            ))),
        }
    }
}

/// Compress `data` with `alg`.
///
/// # Errors
/// Returns [`ClipboardError::Unsupported`] if `alg` requires a feature that is
/// not compiled in, or [`ClipboardError::Compression`] on backend failure.
pub fn compress(alg: CompressionAlgorithm, data: &[u8]) -> Result<Vec<u8>, ClipboardError> {
    match alg {
        CompressionAlgorithm::None => Ok(data.to_vec()),
        CompressionAlgorithm::Deflate => deflate_compress(data),
    }
}

/// Decompress `data` previously produced with `alg`.
///
/// # Errors
/// Returns [`ClipboardError::Unsupported`] if `alg` requires a feature that is
/// not compiled in, or [`ClipboardError::Compression`] on corrupt input.
pub fn decompress(alg: CompressionAlgorithm, data: &[u8]) -> Result<Vec<u8>, ClipboardError> {
    match alg {
        CompressionAlgorithm::None => Ok(data.to_vec()),
        CompressionAlgorithm::Deflate => deflate_decompress(data),
    }
}

#[cfg(feature = "compression")]
fn deflate_compress(data: &[u8]) -> Result<Vec<u8>, ClipboardError> {
    use std::io::Write;

    use flate2::Compression;
    use flate2::write::DeflateEncoder;

    let mut enc = DeflateEncoder::new(Vec::new(), Compression::default());
    enc.write_all(data)
        .map_err(|e| ClipboardError::Compression(e.to_string()))?;
    enc.finish()
        .map_err(|e| ClipboardError::Compression(e.to_string()))
}

#[cfg(feature = "compression")]
fn deflate_decompress(data: &[u8]) -> Result<Vec<u8>, ClipboardError> {
    use std::io::Read;

    use flate2::read::DeflateDecoder;

    let mut dec = DeflateDecoder::new(data);
    let mut out = Vec::new();
    dec.read_to_end(&mut out)
        .map_err(|e| ClipboardError::Compression(e.to_string()))?;
    Ok(out)
}

#[cfg(not(feature = "compression"))]
fn deflate_compress(_data: &[u8]) -> Result<Vec<u8>, ClipboardError> {
    Err(ClipboardError::Unsupported(
        "deflate (compression feature off)",
    ))
}

#[cfg(not(feature = "compression"))]
fn deflate_decompress(_data: &[u8]) -> Result<Vec<u8>, ClipboardError> {
    Err(ClipboardError::Unsupported(
        "deflate (compression feature off)",
    ))
}

/// Decides when (and with what) to compress an outbound snapshot.
///
/// The policy avoids the two failure modes of naive compression: spending CPU
/// on tiny payloads where headers dominate, and re-compressing already-dense
/// image data where DEFLATE only adds overhead.
#[derive(Debug, Clone)]
pub struct CompressionPolicy {
    /// Algorithm to use when compression is chosen.
    pub prefer: CompressionAlgorithm,
    /// Skip compression below this many bytes.
    pub min_size: usize,
}

impl Default for CompressionPolicy {
    fn default() -> Self {
        Self {
            // Prefer Deflate when the feature is on; otherwise stay verbatim.
            prefer: if cfg!(feature = "compression") {
                CompressionAlgorithm::Deflate
            } else {
                CompressionAlgorithm::None
            },
            min_size: 256,
        }
    }
}

impl CompressionPolicy {
    /// Choose an algorithm for `snapshot`, or [`CompressionAlgorithm::None`].
    #[must_use]
    pub fn choose(&self, snapshot: &ClipboardSnapshot) -> CompressionAlgorithm {
        if self.prefer == CompressionAlgorithm::None
            || snapshot.total_len() < self.min_size
            || snapshot.is_mostly_precompressed()
        {
            CompressionAlgorithm::None
        } else {
            self.prefer
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::ClipboardContent;
    use bytes::Bytes;

    #[test]
    fn algorithm_round_trips_discriminant() {
        for alg in [CompressionAlgorithm::None, CompressionAlgorithm::Deflate] {
            assert_eq!(CompressionAlgorithm::from_u8(alg.as_u8()).unwrap(), alg);
        }
        assert!(CompressionAlgorithm::from_u8(99).is_err());
    }

    #[test]
    fn none_is_identity() {
        let data = b"abc";
        let c = compress(CompressionAlgorithm::None, data).unwrap();
        assert_eq!(c, data);
        assert_eq!(decompress(CompressionAlgorithm::None, &c).unwrap(), data);
    }

    #[cfg(feature = "compression")]
    #[test]
    fn deflate_round_trips_and_shrinks() {
        let data = vec![b'a'; 4096];
        let compressed = compress(CompressionAlgorithm::Deflate, &data).unwrap();
        assert!(compressed.len() < data.len());
        let restored = decompress(CompressionAlgorithm::Deflate, &compressed).unwrap();
        assert_eq!(restored, data);
    }

    #[test]
    fn policy_skips_small_payloads() {
        let policy = CompressionPolicy::default();
        let small = ClipboardSnapshot::from_text("tiny");
        assert_eq!(policy.choose(&small), CompressionAlgorithm::None);
    }

    #[test]
    fn policy_skips_image_dominated() {
        let policy = CompressionPolicy::default();
        let img = ClipboardSnapshot::new(vec![ClipboardContent::image_png(Bytes::from(vec![
            7u8;
            8192
        ]))]);
        assert_eq!(policy.choose(&img), CompressionAlgorithm::None);
    }
}
