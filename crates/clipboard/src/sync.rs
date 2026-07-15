//! Clipboard sync wire message and the end-to-end pipeline.
//!
//! [`ClipboardUpdate`] is the body carried under
//! [`MessageKind::Clipboard`](nexkvm_protocol::MessageKind::Clipboard). The
//! [`ClipboardSync`] state machine composes the other modules into the two
//! directions of a sync:
//!
//! ```text
//! outbound:  snapshot ─▶ conflict(local) ─▶ encode ─▶ compress ─▶ seal ─▶ ClipboardUpdate
//! inbound:   ClipboardUpdate ─▶ open ─▶ decompress ─▶ decode ─▶ conflict(inbound) ─▶ snapshot?
//! ```
//!
//! Encryption wraps the *compressed* bytes (compress-then-encrypt) so the cipher
//! never sees exploitable plaintext redundancy and the transport sees only
//! sealed output. The orchestration layer feeds outbound updates onto the
//! [`MessageKind::Clipboard`](nexkvm_protocol::MessageKind::Clipboard) lane and
//! applies returned inbound snapshots to the platform clipboard.

use bytes::{Buf, BufMut, Bytes, BytesMut};
use nexkvm_core::identity::DeviceId;
use uuid::Uuid;

use crate::ClipboardError;
use crate::cipher::ClipboardCipher;
use crate::compression::{self, CompressionAlgorithm, CompressionPolicy};
use crate::conflict::{ConflictResolver, InboundDecision, LocalDecision, OriginStamp};
use crate::content::ClipboardSnapshot;

/// Fixed `ClipboardUpdate` header: origin(16) + seq(8) + at_millis(8) +
/// compression(1) + payload_len(4).
const UPDATE_HEADER_LEN: usize = 16 + 8 + 8 + 1 + 4;

/// Frame budget shared with the stream transports.
const TRANSPORT_FRAME_MAX: usize = 16 * 1024 * 1024;
/// Fixed `Envelope` header encoded by `nexkvm-network::wire`.
const WIRE_ENVELOPE_HEADER_LEN: usize = 2 + 2 + 8 + 2;
/// Version and message-kind binding prepended before transport-layer sealing.
const SECURE_BODY_HEADER_LEN: usize = 2 + 2 + 2;
/// Authentication tag appended by the current ChaCha20-Poly1305 session layer.
const SESSION_AEAD_TAG_LEN: usize = 16;
/// Largest sealed clipboard payload that still fits one secure transport frame.
const DEFAULT_MAX_PAYLOAD: usize = TRANSPORT_FRAME_MAX
    - WIRE_ENVELOPE_HEADER_LEN
    - SECURE_BODY_HEADER_LEN
    - SESSION_AEAD_TAG_LEN
    - UPDATE_HEADER_LEN;

/// A clipboard synchronization message as it travels on the wire.
///
/// `payload` is the sealed (and possibly compressed) encoding of a
/// [`ClipboardSnapshot`]; it is opaque until [`ClipboardSync::accept_inbound`]
/// opens it.
#[derive(Debug, Clone)]
pub struct ClipboardUpdate {
    /// Producing device.
    pub origin: DeviceId,
    /// Hybrid logical ordering sequence.
    pub seq: u64,
    /// Wall-clock millis at production.
    pub at_millis: u64,
    /// Compression applied to the pre-seal bytes.
    pub compression: CompressionAlgorithm,
    /// Sealed payload (ciphertext over the compressed snapshot encoding).
    pub payload: Bytes,
}

impl ClipboardUpdate {
    /// Encode to the binary envelope body.
    ///
    /// # Errors
    /// Returns [`ClipboardError::TooLarge`] if the payload exceeds `u32`.
    pub fn encode(&self) -> Result<Bytes, ClipboardError> {
        let payload_len =
            u32::try_from(self.payload.len()).map_err(|_| ClipboardError::TooLarge {
                size: self.payload.len(),
                limit: u32::MAX as usize,
            })?;
        let mut buf = BytesMut::with_capacity(UPDATE_HEADER_LEN + self.payload.len());
        buf.put_slice(self.origin.0.as_bytes());
        buf.put_u64(self.seq);
        buf.put_u64(self.at_millis);
        buf.put_u8(self.compression.as_u8());
        buf.put_u32(payload_len);
        buf.put_slice(&self.payload);
        Ok(buf.freeze())
    }

    /// Decode from a binary envelope body, validating all fields.
    ///
    /// # Errors
    /// Returns [`ClipboardError::Codec`] on truncation/invalid fields.
    pub fn decode(mut buf: Bytes) -> Result<Self, ClipboardError> {
        if buf.remaining() < UPDATE_HEADER_LEN {
            return Err(ClipboardError::Codec("truncated update header".into()));
        }
        let mut uuid = [0u8; 16];
        buf.copy_to_slice(&mut uuid);
        let origin = DeviceId(Uuid::from_bytes(uuid));
        let seq = buf.get_u64();
        let at_millis = buf.get_u64();
        let compression = CompressionAlgorithm::from_u8(buf.get_u8())?;
        let payload_len = buf.get_u32() as usize;
        if payload_len != buf.remaining() {
            return Err(ClipboardError::Codec("payload length mismatch".into()));
        }
        Ok(Self {
            origin,
            seq,
            at_millis,
            compression,
            payload: buf,
        })
    }
}

/// Per-link clipboard sync state machine.
///
/// Owns one [`ConflictResolver`], a [`CompressionPolicy`], and an injected
/// [`ClipboardCipher`]. It is sans-IO: callers drive it with observed local
/// snapshots and received updates, and route the results.
pub struct ClipboardSync {
    resolver: ConflictResolver,
    policy: CompressionPolicy,
    cipher: Box<dyn ClipboardCipher>,
    max_payload: usize,
}

impl std::fmt::Debug for ClipboardSync {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClipboardSync")
            .field("policy", &self.policy)
            .field("max_payload", &self.max_payload)
            .finish_non_exhaustive()
    }
}

impl ClipboardSync {
    /// Create a sync for `local_device`, sealing payloads with `cipher`.
    #[must_use]
    pub fn new(local_device: DeviceId, cipher: Box<dyn ClipboardCipher>) -> Self {
        Self {
            resolver: ConflictResolver::new(local_device),
            policy: CompressionPolicy::default(),
            cipher,
            max_payload: DEFAULT_MAX_PAYLOAD,
        }
    }

    /// Override the compression policy.
    #[must_use]
    pub fn with_policy(mut self, policy: CompressionPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Override the maximum accepted/produced sealed and decoded payload size.
    #[must_use]
    pub fn with_max_payload(mut self, max_payload: usize) -> Self {
        self.max_payload = max_payload;
        self
    }

    /// Build an outbound update for a locally observed clipboard `snapshot`.
    ///
    /// Returns `Ok(None)` when the change is an echo of the held selection and
    /// must not be broadcast (loop prevention).
    ///
    /// # Errors
    /// Returns [`ClipboardError`] on encode/compress/seal failure or if the
    /// sealed payload exceeds the configured maximum.
    pub fn prepare_outbound(
        &mut self,
        snapshot: &ClipboardSnapshot,
        now_millis: u64,
    ) -> Result<Option<ClipboardUpdate>, ClipboardError> {
        if snapshot.is_concealed() {
            return Ok(None);
        }
        let fingerprint = snapshot.fingerprint();
        if self.resolver.holds(fingerprint) {
            return Ok(None);
        }

        let encoded = snapshot.encode()?;
        if encoded.len() > self.max_payload {
            return Err(ClipboardError::TooLarge {
                size: encoded.len(),
                limit: self.max_payload,
            });
        }
        let algorithm = self.policy.choose(snapshot);
        let compressed = compression::compress(algorithm, &encoded)?;
        let payload = self.cipher.seal(&compressed)?;

        if payload.len() > self.max_payload {
            return Err(ClipboardError::TooLarge {
                size: payload.len(),
                limit: self.max_payload,
            });
        }

        let stamp = match self.resolver.on_local_change(fingerprint, now_millis) {
            LocalDecision::Suppress => return Ok(None),
            LocalDecision::Broadcast(stamp) => stamp,
            LocalDecision::ClockExhausted => return Err(ClipboardError::ClockExhausted),
        };

        Ok(Some(ClipboardUpdate {
            origin: stamp.origin,
            seq: stamp.seq,
            at_millis: stamp.at_millis,
            compression: algorithm,
            payload: Bytes::from(payload),
        }))
    }

    /// Process an inbound update, returning the snapshot to apply locally, or
    /// `None` if it was stale or an echo.
    ///
    /// # Errors
    /// Returns [`ClipboardError`] on oversize payloads or open/decompress/decode
    /// failure (e.g. a forged or corrupt message).
    pub fn accept_inbound(
        &mut self,
        update: ClipboardUpdate,
    ) -> Result<Option<ClipboardSnapshot>, ClipboardError> {
        if update.payload.len() > self.max_payload {
            return Err(ClipboardError::TooLarge {
                size: update.payload.len(),
                limit: self.max_payload,
            });
        }

        let compressed = self.cipher.open(&update.payload)?;
        let encoded =
            compression::decompress_bounded(update.compression, &compressed, self.max_payload)?;
        let snapshot = ClipboardSnapshot::decode(Bytes::from(encoded))?;

        let stamp = OriginStamp {
            origin: update.origin,
            seq: update.seq,
            at_millis: update.at_millis,
        };
        match self.resolver.on_inbound(stamp, snapshot.fingerprint()) {
            InboundDecision::Apply => Ok(Some(snapshot)),
            InboundDecision::IgnoreStale | InboundDecision::IgnoreEcho => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cipher::PlaintextCipher;
    use crate::content::ClipboardContent;

    fn sync(dev: DeviceId) -> ClipboardSync {
        ClipboardSync::new(dev, Box::new(PlaintextCipher))
    }

    #[test]
    fn update_wire_round_trips() {
        let update = ClipboardUpdate {
            origin: DeviceId::generate(),
            seq: 9,
            at_millis: 1234,
            compression: CompressionAlgorithm::None,
            payload: Bytes::from_static(b"sealed"),
        };
        let encoded = update.encode().unwrap();
        let decoded = ClipboardUpdate::decode(encoded).unwrap();
        assert_eq!(decoded.origin, update.origin);
        assert_eq!(decoded.seq, update.seq);
        assert_eq!(decoded.at_millis, update.at_millis);
        assert_eq!(decoded.compression, update.compression);
        assert_eq!(decoded.payload, update.payload);
    }

    #[test]
    fn decode_rejects_truncated_update() {
        let buf = Bytes::from_static(&[0u8; 8]);
        assert!(matches!(
            ClipboardUpdate::decode(buf),
            Err(ClipboardError::Codec(_))
        ));
    }

    #[test]
    fn default_payload_cap_fits_secure_transport_frame() {
        const FRAME_MAX: usize = 16 * 1024 * 1024;
        const ENVELOPE_HEADER: usize = 2 + 2 + 8 + 2;
        const SECURE_BODY_HEADER: usize = 2 + 2 + 2;
        const SESSION_AEAD_TAG: usize = 16;
        let configured_max = sync(DeviceId::generate()).max_payload;

        assert!(
            ENVELOPE_HEADER
                + SECURE_BODY_HEADER
                + SESSION_AEAD_TAG
                + UPDATE_HEADER_LEN
                + configured_max
                <= FRAME_MAX
        );
    }

    #[test]
    fn oversized_outbound_does_not_advance_sequence() {
        let mut a = sync(DeviceId::generate()).with_max_payload(64);
        let oversized = ClipboardSnapshot::from_text("x".repeat(128));

        assert!(matches!(
            a.prepare_outbound(&oversized, 1),
            Err(ClipboardError::TooLarge { limit: 64, .. })
        ));

        let update = a
            .prepare_outbound(&ClipboardSnapshot::from_text("ok"), 1)
            .unwrap()
            .expect("small clipboard update");
        assert_eq!(update.seq, 1, "rejected payload must not consume a stamp");
    }

    #[cfg(feature = "compression")]
    #[test]
    fn inbound_decompression_is_bounded_by_max_payload() {
        let snapshot = ClipboardSnapshot::from_text("z".repeat(4096));
        let encoded = snapshot.encode().unwrap();
        let compressed = compression::compress(CompressionAlgorithm::Deflate, &encoded).unwrap();
        assert!(compressed.len() < 256, "fixture must pass wire-size check");

        let update = ClipboardUpdate {
            origin: DeviceId::generate(),
            seq: 1,
            at_millis: 1,
            compression: CompressionAlgorithm::Deflate,
            payload: Bytes::from(compressed),
        };
        let mut receiver = sync(DeviceId::generate()).with_max_payload(256);

        assert!(matches!(
            receiver.accept_inbound(update),
            Err(ClipboardError::TooLarge {
                size: 257,
                limit: 256
            })
        ));
    }

    #[test]
    fn full_pipeline_between_two_devices() {
        let dev_a = DeviceId::generate();
        let dev_b = DeviceId::generate();
        let mut a = sync(dev_a);
        let mut b = sync(dev_b);

        // A copies rich content.
        let snap = ClipboardSnapshot::new(vec![
            ClipboardContent::text("hello"),
            ClipboardContent::html("<b>hello</b>"),
        ]);
        let update = a.prepare_outbound(&snap, 100).unwrap().expect("broadcast");

        // Travels over the wire and back.
        let wire = update.encode().unwrap();
        let received = ClipboardUpdate::decode(wire).unwrap();

        // B applies it.
        let applied = b.accept_inbound(received).unwrap().expect("apply");
        assert_eq!(applied, snap);
    }

    #[test]
    fn echo_does_not_loop_back() {
        let dev_a = DeviceId::generate();
        let dev_b = DeviceId::generate();
        let mut a = sync(dev_a);
        let mut b = sync(dev_b);

        let snap = ClipboardSnapshot::from_text("ping");
        let update = a.prepare_outbound(&snap, 1).unwrap().unwrap();
        let applied = b.accept_inbound(update).unwrap().unwrap();

        // B's watcher now observes the applied content; it must not rebroadcast.
        assert!(b.prepare_outbound(&applied, 2).unwrap().is_none());
    }

    #[test]
    fn duplicate_local_copy_is_suppressed() {
        let mut a = sync(DeviceId::generate());
        let snap = ClipboardSnapshot::from_text("same");
        assert!(a.prepare_outbound(&snap, 1).unwrap().is_some());
        assert!(a.prepare_outbound(&snap, 2).unwrap().is_none());
    }

    #[test]
    fn concealed_clipboard_content_is_never_prepared_for_the_network() {
        let mut sync = sync(DeviceId::generate());
        let concealed = ClipboardSnapshot::new(vec![ClipboardContent {
            mime: "org.nspasteboard.ConcealedType".into(),
            data: Bytes::from_static(b"password"),
        }]);

        assert!(sync.prepare_outbound(&concealed, 1).unwrap().is_none());
        let ordinary = ClipboardSnapshot::from_text("safe");
        let update = sync
            .prepare_outbound(&ordinary, 1)
            .unwrap()
            .expect("concealed content must not advance the outbound clock");
        assert_eq!(update.seq, 1);
    }

    #[test]
    fn stale_inbound_is_ignored() {
        let mut b = sync(DeviceId::generate());
        let peer = DeviceId::generate();

        let newer = ClipboardUpdate {
            origin: peer,
            seq: 10,
            at_millis: 5,
            compression: CompressionAlgorithm::None,
            payload: encode_plain(&ClipboardSnapshot::from_text("new")),
        };
        let older = ClipboardUpdate {
            origin: peer,
            seq: 2,
            at_millis: 5,
            compression: CompressionAlgorithm::None,
            payload: encode_plain(&ClipboardSnapshot::from_text("old")),
        };
        assert!(b.accept_inbound(newer).unwrap().is_some());
        assert!(b.accept_inbound(older).unwrap().is_none());
    }

    #[cfg(feature = "compression")]
    #[test]
    fn large_text_payload_is_compressed() {
        let mut a = sync(DeviceId::generate());
        let big = "x".repeat(4096);
        let snap = ClipboardSnapshot::from_text(big);
        let update = a.prepare_outbound(&snap, 1).unwrap().unwrap();
        assert_eq!(update.compression, CompressionAlgorithm::Deflate);
        assert!(update.payload.len() < 4096);
    }

    /// Helper: build a plaintext (PlaintextCipher) sealed, uncompressed payload.
    fn encode_plain(snapshot: &ClipboardSnapshot) -> Bytes {
        snapshot.encode().unwrap()
    }
}
