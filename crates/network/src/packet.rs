//! Zero-copy packet helpers.
//!
//! Transport backends should pass received datagrams/frames as [`bytes::Bytes`]
//! so envelope bodies can be sliced without copying. This module wraps that
//! pattern in a small API that keeps ownership explicit and validates packets at
//! the protocol boundary.

use bytes::{Bytes, BytesMut};
use nexkvm_protocol::{Envelope, ProtocolError};

use crate::wire::{decode_envelope, encode_envelope};

/// A complete protocol packet backed by reference-counted bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZeroCopyPacket {
    bytes: Bytes,
}

impl ZeroCopyPacket {
    /// Encode an envelope into a packet.
    #[must_use]
    pub fn from_envelope(envelope: &Envelope) -> Self {
        let mut bytes = BytesMut::with_capacity(crate::wire::HEADER_LEN + envelope.body.len());
        encode_envelope(envelope, &mut bytes);
        Self {
            bytes: bytes.freeze(),
        }
    }

    /// Wrap bytes received from a transport.
    #[must_use]
    pub fn from_bytes(bytes: Bytes) -> Self {
        Self { bytes }
    }

    /// Decode the packet. Envelope body is a zero-copy slice of this packet.
    ///
    /// # Errors
    /// Returns [`ProtocolError`] for malformed packet headers.
    pub fn decode(&self) -> Result<Envelope, ProtocolError> {
        decode_envelope(self.bytes.clone())
    }

    /// Borrow packet bytes for transport writes.
    #[must_use]
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }

    /// Packet length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether the packet is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

/// A batch of zero-copy packets ready for one transport flush.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PacketBatch {
    packets: Vec<ZeroCopyPacket>,
    bytes: usize,
}

impl PacketBatch {
    /// Create an empty batch.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one packet.
    pub fn push(&mut self, packet: ZeroCopyPacket) {
        self.bytes = self.bytes.saturating_add(packet.len());
        self.packets.push(packet);
    }

    /// Packets in send order.
    #[must_use]
    pub fn packets(&self) -> &[ZeroCopyPacket] {
        &self.packets
    }

    /// Total byte length.
    #[must_use]
    pub const fn byte_len(&self) -> usize {
        self.bytes
    }

    /// Number of packets.
    #[must_use]
    pub fn len(&self) -> usize {
        self.packets.len()
    }

    /// Whether the batch is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.packets.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use nexkvm_protocol::{MessageId, MessageKind, PROTOCOL_VERSION};

    #[test]
    fn packet_decodes_body_zero_copy() {
        let env = Envelope::new(
            PROTOCOL_VERSION,
            MessageId(7),
            MessageKind::Input,
            Bytes::from_static(b"motion"),
        );
        let packet = ZeroCopyPacket::from_envelope(&env);
        let decoded = packet.decode().unwrap();
        assert_eq!(decoded.id, env.id);
        assert_eq!(decoded.body, env.body);
    }

    #[test]
    fn batch_tracks_total_bytes() {
        let first = ZeroCopyPacket::from_bytes(Bytes::from_static(b"abc"));
        let second = ZeroCopyPacket::from_bytes(Bytes::from_static(b"de"));
        let mut batch = PacketBatch::new();
        batch.push(first);
        batch.push(second);
        assert_eq!(batch.len(), 2);
        assert_eq!(batch.byte_len(), 5);
    }
}
