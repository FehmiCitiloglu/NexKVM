//! QR-code pairing bootstrap.
//!
//! To pair, the initiator renders a [`PairingBootstrap`] as a compact `nexkvm://`
//! URI inside a QR code; the responder scans it to learn exactly where to
//! connect and which key to expect. The bootstrap carries:
//!
//! - the initiator's [`PublicKey`] (to pin on first use),
//! - a fresh [`nonce`](PairingBootstrap::nonce) that makes independently
//!   generated bootstrap payloads distinct,
//! - the dial address and display name for zero-config connection.
//!
//! # Security
//! The QR channel is the *out-of-band authenticator* that defeats MITM on the
//! first key exchange: an attacker on the network cannot forge the public key in
//! a QR the user physically scans. Callers must still compare the fingerprint
//! before importing trust. The current trust-import flow does not persist or
//! consume the nonce, so it must not be treated as a single-use replay token.
//! Rendering the QR image is a UI concern; this module only defines the payload
//! and its encoding, dependency-free.
//!
//! The wire form is `nexkvm://pair/v1/<hex>`, where `<hex>` is a small binary
//! record. Hex keeps the payload URL-safe and dependency-free; pairing payloads
//! are tiny, so the 2x size overhead is irrelevant for QR capacity.

use crate::CryptoError;
use crate::identity::PublicKey;

/// URI scheme + path prefix identifying a v1 pairing bootstrap.
const URI_PREFIX: &str = "nexkvm://pair/v1/";
const MAX_PAIRING_URI_BYTES: usize = 4 * 1024;
const MAX_DISPLAY_NAME_BYTES: usize = 255;
const MAX_PAIRING_ADDRESS_BYTES: usize = 1_024;
const ED25519_PUBLIC_KEY_BYTES: usize = 32;

/// Length of the single-use pairing nonce, in bytes.
pub const NONCE_LEN: usize = 32;

/// Everything a peer needs to start an authenticated pairing handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingBootstrap {
    /// Friendly name of the initiating device.
    pub display_name: String,
    /// Initiator's long-lived public key, to be pinned on success.
    pub public_key: PublicKey,
    /// Fresh, single-use value bound into the pairing confirmation.
    pub nonce: [u8; NONCE_LEN],
    /// Address the responder should dial (`ip:port`).
    pub addr: String,
}

impl PairingBootstrap {
    /// Construct a bootstrap.
    #[must_use]
    pub fn new(
        display_name: impl Into<String>,
        public_key: PublicKey,
        nonce: [u8; NONCE_LEN],
        addr: impl Into<String>,
    ) -> Self {
        Self {
            display_name: display_name.into(),
            public_key,
            nonce,
            addr: addr.into(),
        }
    }

    /// Encode to a scannable `nexkvm://pair/v1/<hex>` URI.
    ///
    /// # Errors
    /// Returns [`CryptoError::Pairing`] instead of truncating invalid or
    /// oversized fields.
    pub fn to_uri(&self) -> Result<String, CryptoError> {
        self.validate()?;
        let uri = format!("{URI_PREFIX}{}", hex_encode(&self.encode_binary()?));
        if uri.len() > MAX_PAIRING_URI_BYTES {
            return Err(CryptoError::Pairing("pairing uri is too large".into()));
        }
        Ok(uri)
    }

    /// Parse a bootstrap from its `nexkvm://pair/v1/<hex>` URI form.
    ///
    /// # Errors
    /// Returns [`CryptoError::Pairing`] if the scheme/prefix is wrong or the
    /// payload is malformed.
    pub fn from_uri(uri: &str) -> Result<Self, CryptoError> {
        if uri.len() > MAX_PAIRING_URI_BYTES {
            return Err(CryptoError::Pairing("pairing uri is too large".into()));
        }
        let hex = uri
            .strip_prefix(URI_PREFIX)
            .ok_or_else(|| CryptoError::Pairing("unrecognized pairing uri".into()))?;
        let bytes = hex_decode(hex)?;
        Self::decode_binary(&bytes)
    }

    /// Compact binary record:
    /// `name_len(u16) name | key_len(u16) key | nonce(32) | addr_len(u16) addr`,
    /// all big-endian. Mirrors the dependency-free codec style used elsewhere.
    fn encode_binary(&self) -> Result<Vec<u8>, CryptoError> {
        let name = self.display_name.as_bytes();
        let key = self.public_key.as_bytes();
        let addr = self.addr.as_bytes();
        let mut out =
            Vec::with_capacity(2 + name.len() + 2 + key.len() + NONCE_LEN + 2 + addr.len());
        put_field(&mut out, name)?;
        put_field(&mut out, key)?;
        out.extend_from_slice(&self.nonce);
        put_field(&mut out, addr)?;
        Ok(out)
    }

    fn decode_binary(bytes: &[u8]) -> Result<Self, CryptoError> {
        let mut cur = bytes;
        let name = take_field(&mut cur)?;
        let key = take_field(&mut cur)?;
        if cur.len() < NONCE_LEN {
            return Err(CryptoError::Pairing("truncated nonce".into()));
        }
        let (nonce_bytes, rest) = cur.split_at(NONCE_LEN);
        cur = rest;
        let addr = take_field(&mut cur)?;
        if !cur.is_empty() {
            return Err(CryptoError::Pairing("trailing pairing bytes".into()));
        }
        let display_name = String::from_utf8(name.to_vec())
            .map_err(|_| CryptoError::Pairing("invalid name utf8".into()))?;
        let addr = String::from_utf8(addr.to_vec())
            .map_err(|_| CryptoError::Pairing("invalid addr utf8".into()))?;
        let mut nonce = [0u8; NONCE_LEN];
        nonce.copy_from_slice(nonce_bytes);
        let bootstrap = Self {
            display_name,
            public_key: PublicKey(key.to_vec()),
            nonce,
            addr,
        };
        bootstrap.validate()?;
        Ok(bootstrap)
    }

    fn validate(&self) -> Result<(), CryptoError> {
        if self.display_name.is_empty()
            || self.display_name.len() > MAX_DISPLAY_NAME_BYTES
            || self.display_name.chars().any(char::is_control)
        {
            return Err(CryptoError::Pairing("invalid display name".into()));
        }
        if self.public_key.as_bytes().len() != ED25519_PUBLIC_KEY_BYTES {
            return Err(CryptoError::Pairing(
                "invalid Ed25519 public key length".into(),
            ));
        }
        if self.addr.is_empty()
            || self.addr.len() > MAX_PAIRING_ADDRESS_BYTES
            || self.addr.trim() != self.addr
            || self.addr.chars().any(char::is_control)
        {
            return Err(CryptoError::Pairing("invalid pairing address".into()));
        }
        Ok(())
    }
}

fn put_field(out: &mut Vec<u8>, field: &[u8]) -> Result<(), CryptoError> {
    let len = u16::try_from(field.len())
        .map_err(|_| CryptoError::Pairing("pairing field is too large".into()))?;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(field);
    Ok(())
}

fn take_field<'a>(cur: &mut &'a [u8]) -> Result<&'a [u8], CryptoError> {
    if cur.len() < 2 {
        return Err(CryptoError::Pairing("truncated length prefix".into()));
    }
    let (len_bytes, rest) = cur.split_at(2);
    let len = u16::from_be_bytes([len_bytes[0], len_bytes[1]]) as usize;
    if rest.len() < len {
        return Err(CryptoError::Pairing("truncated field".into()));
    }
    let (field, tail) = rest.split_at(len);
    *cur = tail;
    Ok(field)
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('0'));
        s.push(char::from_digit((b & 0x0f) as u32, 16).unwrap_or('0'));
    }
    s
}

fn hex_decode(s: &str) -> Result<Vec<u8>, CryptoError> {
    if !s.len().is_multiple_of(2) {
        return Err(CryptoError::Pairing("odd-length hex".into()));
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let hi = hex_val(pair[0])?;
        let lo = hex_val(pair[1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn hex_val(c: u8) -> Result<u8, CryptoError> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(CryptoError::Pairing("invalid hex digit".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> PairingBootstrap {
        PairingBootstrap::new(
            "Alien's MacBook",
            PublicKey(vec![1; ED25519_PUBLIC_KEY_BYTES]),
            [7u8; NONCE_LEN],
            "192.168.1.20:47654",
        )
    }

    #[test]
    fn uri_round_trips() {
        let boot = sample();
        let uri = boot.to_uri().unwrap();
        assert!(uri.starts_with("nexkvm://pair/v1/"));
        let parsed = PairingBootstrap::from_uri(&uri).unwrap();
        assert_eq!(parsed, boot);
    }

    #[test]
    fn rejects_wrong_scheme() {
        let err = PairingBootstrap::from_uri("https://evil/pair").unwrap_err();
        assert!(matches!(err, CryptoError::Pairing(_)));
    }

    #[test]
    fn rejects_truncated_payload() {
        let boot = sample();
        let uri = boot.to_uri().unwrap();
        // Drop the last hex byte-pair to corrupt the record.
        let truncated = &uri[..uri.len() - 4];
        assert!(PairingBootstrap::from_uri(truncated).is_err());
    }

    #[test]
    fn rejects_non_hex_payload() {
        assert!(PairingBootstrap::from_uri("nexkvm://pair/v1/zzzz").is_err());
    }

    #[test]
    fn rejects_oversized_uri_before_decoding_payload_fields() {
        let uri = format!("{URI_PREFIX}{}", "00".repeat(MAX_PAIRING_URI_BYTES));
        assert!(PairingBootstrap::from_uri(&uri).is_err());
    }

    #[test]
    fn rejects_terminal_controls_and_non_ed25519_keys() {
        let unsafe_name = PairingBootstrap::new(
            "trusted\nspoofed",
            PublicKey(vec![1; ED25519_PUBLIC_KEY_BYTES]),
            [0; NONCE_LEN],
            "192.168.1.20:47654",
        );
        assert!(unsafe_name.to_uri().is_err());

        let short_key = PairingBootstrap::new(
            "trusted",
            PublicKey(vec![1; ED25519_PUBLIC_KEY_BYTES - 1]),
            [0; NONCE_LEN],
            "192.168.1.20:47654",
        );
        assert!(short_key.to_uri().is_err());
    }

    #[test]
    fn uri_encoding_rejects_invalid_or_oversized_fields_without_truncation() {
        let oversized = PairingBootstrap::new(
            "n".repeat(MAX_DISPLAY_NAME_BYTES + 1),
            PublicKey(vec![1; ED25519_PUBLIC_KEY_BYTES]),
            [7; NONCE_LEN],
            "192.168.1.20:47654",
        );
        let control = PairingBootstrap::new(
            "trusted\nspoofed",
            PublicKey(vec![1; ED25519_PUBLIC_KEY_BYTES]),
            [7; NONCE_LEN],
            "192.168.1.20:47654",
        );

        assert!(oversized.to_uri().is_err());
        assert!(control.to_uri().is_err());
    }
}
