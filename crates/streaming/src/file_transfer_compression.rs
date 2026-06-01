//! Transfer payload compression.

use crate::TransferError;

/// Compression algorithm carried with each transfer chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferCompression {
    /// Uncompressed bytes.
    None,
    /// DEFLATE stream.
    Deflate,
}

impl TransferCompression {
    /// Wire discriminant.
    #[must_use]
    pub fn as_u8(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Deflate => 1,
        }
    }

    /// Parse wire discriminant.
    ///
    /// # Errors
    /// Returns [`TransferError::Codec`] on unknown values.
    pub fn from_u8(v: u8) -> Result<Self, TransferError> {
        match v {
            0 => Ok(Self::None),
            1 => Ok(Self::Deflate),
            _ => Err(TransferError::Codec(format!("unknown compression {v}"))),
        }
    }
}

/// Compression selection policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionStrategy {
    /// Prefer minimal CPU latency; compress only larger chunks.
    LatencyFirst,
    /// Balance CPU and bandwidth.
    Balanced,
    /// Prefer bandwidth savings for bulk transfer.
    ThroughputFirst,
}

/// Compression selection policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferCompressionPolicy {
    /// Preferred algorithm when compression is beneficial.
    pub preferred: TransferCompression,
    /// Minimum payload length to attempt compression.
    pub min_len: usize,
    /// CPU/bandwidth tradeoff.
    pub strategy: CompressionStrategy,
}

impl Default for TransferCompressionPolicy {
    fn default() -> Self {
        Self {
            preferred: if cfg!(feature = "transfer-compression") {
                TransferCompression::Deflate
            } else {
                TransferCompression::None
            },
            min_len: 1024,
            strategy: CompressionStrategy::Balanced,
        }
    }
}

impl TransferCompressionPolicy {
    /// Low-latency policy for interactive streams/control-adjacent payloads.
    #[must_use]
    pub fn latency_first() -> Self {
        Self {
            min_len: 64 * 1024,
            strategy: CompressionStrategy::LatencyFirst,
            ..Self::default()
        }
    }

    /// Throughput-oriented policy for large bulk transfer.
    #[must_use]
    pub fn throughput_first() -> Self {
        Self {
            min_len: 512,
            strategy: CompressionStrategy::ThroughputFirst,
            ..Self::default()
        }
    }

    /// Choose compression for a chunk by size.
    #[must_use]
    pub fn choose(&self, plain_len: usize) -> TransferCompression {
        if plain_len < self.min_len {
            TransferCompression::None
        } else {
            self.preferred
        }
    }

    #[cfg(feature = "transfer-compression")]
    fn deflate_level(&self) -> flate2::Compression {
        match self.strategy {
            CompressionStrategy::LatencyFirst => flate2::Compression::fast(),
            CompressionStrategy::Balanced => flate2::Compression::default(),
            CompressionStrategy::ThroughputFirst => flate2::Compression::best(),
        }
    }
}

/// Compress bytes.
///
/// # Errors
/// Returns [`TransferError::Compression`] or [`TransferError::Unsupported`].
pub fn compress(alg: TransferCompression, plain: &[u8]) -> Result<Vec<u8>, TransferError> {
    match alg {
        TransferCompression::None => Ok(plain.to_vec()),
        TransferCompression::Deflate => deflate_compress(plain),
    }
}

/// Compress bytes using a full tuning policy.
///
/// # Errors
/// Returns [`TransferError::Compression`] or [`TransferError::Unsupported`].
pub fn compress_with_policy(
    policy: TransferCompressionPolicy,
    plain: &[u8],
) -> Result<(TransferCompression, Vec<u8>), TransferError> {
    let alg = policy.choose(plain.len());
    match alg {
        TransferCompression::None => Ok((alg, plain.to_vec())),
        TransferCompression::Deflate => {
            deflate_compress_with_policy(policy, plain).map(|out| (alg, out))
        }
    }
}

/// Decompress bytes.
///
/// # Errors
/// Returns [`TransferError::Compression`] or [`TransferError::Unsupported`].
pub fn decompress(alg: TransferCompression, compressed: &[u8]) -> Result<Vec<u8>, TransferError> {
    match alg {
        TransferCompression::None => Ok(compressed.to_vec()),
        TransferCompression::Deflate => deflate_decompress(compressed),
    }
}

#[cfg(feature = "transfer-compression")]
fn deflate_compress(plain: &[u8]) -> Result<Vec<u8>, TransferError> {
    deflate_compress_with_policy(TransferCompressionPolicy::default(), plain)
}

#[cfg(feature = "transfer-compression")]
fn deflate_compress_with_policy(
    policy: TransferCompressionPolicy,
    plain: &[u8],
) -> Result<Vec<u8>, TransferError> {
    use std::io::Write;

    use flate2::write::DeflateEncoder;

    let mut encoder = DeflateEncoder::new(Vec::new(), policy.deflate_level());
    encoder
        .write_all(plain)
        .map_err(|e| TransferError::Compression(e.to_string()))?;
    encoder
        .finish()
        .map_err(|e| TransferError::Compression(e.to_string()))
}

#[cfg(feature = "transfer-compression")]
fn deflate_decompress(compressed: &[u8]) -> Result<Vec<u8>, TransferError> {
    use std::io::Read;

    use flate2::read::DeflateDecoder;

    let mut decoder = DeflateDecoder::new(compressed);
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .map_err(|e| TransferError::Compression(e.to_string()))?;
    Ok(out)
}

#[cfg(not(feature = "transfer-compression"))]
fn deflate_compress(_plain: &[u8]) -> Result<Vec<u8>, TransferError> {
    Err(TransferError::Unsupported(
        "deflate (compression feature off)",
    ))
}

#[cfg(not(feature = "transfer-compression"))]
fn deflate_compress_with_policy(
    _policy: TransferCompressionPolicy,
    _plain: &[u8],
) -> Result<Vec<u8>, TransferError> {
    Err(TransferError::Unsupported(
        "deflate (compression feature off)",
    ))
}

#[cfg(not(feature = "transfer-compression"))]
fn deflate_decompress(_compressed: &[u8]) -> Result<Vec<u8>, TransferError> {
    Err(TransferError::Unsupported(
        "deflate (compression feature off)",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_is_identity() {
        let data = b"abc";
        let c = compress(TransferCompression::None, data).unwrap();
        let d = decompress(TransferCompression::None, &c).unwrap();
        assert_eq!(d, data);
    }

    #[test]
    fn latency_first_skips_small_chunks() {
        let policy = TransferCompressionPolicy::latency_first();
        assert_eq!(policy.choose(1024), TransferCompression::None);
    }

    #[test]
    fn throughput_first_compresses_smaller_chunks() {
        let policy = TransferCompressionPolicy::throughput_first();
        assert_eq!(policy.choose(1024), policy.preferred);
    }

    #[cfg(feature = "transfer-compression")]
    #[test]
    fn deflate_round_trip() {
        let data = vec![b'x'; 4096];
        let c = compress(TransferCompression::Deflate, &data).unwrap();
        assert!(c.len() < data.len());
        let d = decompress(TransferCompression::Deflate, &c).unwrap();
        assert_eq!(d, data);
    }
}
