//! [`Envelope`] wire (de)serialization.
//!
//! A compact, dependency-free binary layout — no serde binary codec is pulled
//! in. The fixed header is followed by the opaque body, so decoding can hand the
//! body back as a zero-copy [`Bytes`] slice of the original buffer.
//!
//! ```text
//! +--------+--------+----------+--------+------------------+
//! | major  | minor  |    id    |  kind  |       body       |
//! | u16 BE | u16 BE |  u64 BE  | u16 BE |   len - HEADER   |
//! +--------+--------+----------+--------+------------------+
//! ```
//!
//! Over a **stream** transport (TCP) the encoded envelope is additionally
//! length-prefixed by [`coklu_protocol::FrameCodec`]. Over a **datagram**
//! transport (QUIC datagrams) the framing is the datagram boundary itself, so
//! the raw encoding here is used directly.

use bytes::{Buf, BufMut, Bytes, BytesMut};
use coklu_protocol::{Envelope, MessageId, MessageKind, ProtocolError, ProtocolVersion};

/// Size of the fixed envelope header in bytes.
pub const HEADER_LEN: usize = 2 + 2 + 8 + 2;

/// Encode `env` (header + body) into `dst`.
pub fn encode_envelope(env: &Envelope, dst: &mut BytesMut) {
    dst.reserve(HEADER_LEN + env.body.len());
    dst.put_u16(env.version.major);
    dst.put_u16(env.version.minor);
    dst.put_u64(env.id.0);
    dst.put_u16(env.kind as u16);
    dst.put_slice(&env.body);
}

/// Decode an [`Envelope`] from a complete `payload` (one frame / one datagram).
///
/// The body is sliced zero-copy from `payload`.
///
/// # Errors
/// - [`ProtocolError::Incomplete`] if `payload` is shorter than the header.
/// - [`ProtocolError::UnknownKind`] if the kind discriminant is unrecognized.
pub fn decode_envelope(mut payload: Bytes) -> Result<Envelope, ProtocolError> {
    if payload.len() < HEADER_LEN {
        return Err(ProtocolError::Incomplete {
            needed: HEADER_LEN - payload.len(),
        });
    }

    let major = payload.get_u16();
    let minor = payload.get_u16();
    let id = payload.get_u64();
    let raw_kind = payload.get_u16();
    let kind = MessageKind::from_u16(raw_kind).ok_or(ProtocolError::UnknownKind(raw_kind))?;

    // After the `get_*` calls, `payload` has advanced past the header; the
    // remainder is the body, retained as a zero-copy slice.
    Ok(Envelope {
        version: ProtocolVersion { major, minor },
        id: MessageId(id),
        kind,
        body: payload,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use coklu_protocol::PROTOCOL_VERSION;

    fn sample() -> Envelope {
        Envelope::new(
            PROTOCOL_VERSION,
            MessageId(42),
            MessageKind::Input,
            Bytes::from_static(b"payload-bytes"),
        )
    }

    #[test]
    fn round_trips() {
        let env = sample();
        let mut buf = BytesMut::new();
        encode_envelope(&env, &mut buf);
        let decoded = decode_envelope(buf.freeze()).unwrap();
        assert_eq!(decoded.id, env.id);
        assert_eq!(decoded.kind, env.kind);
        assert_eq!(decoded.version, env.version);
        assert_eq!(decoded.body, env.body);
    }

    #[test]
    fn rejects_short_payload() {
        let buf = Bytes::from_static(&[0u8; 4]);
        assert!(matches!(
            decode_envelope(buf),
            Err(ProtocolError::Incomplete { .. })
        ));
    }

    #[test]
    fn rejects_unknown_kind() {
        let mut buf = BytesMut::new();
        buf.put_u16(1);
        buf.put_u16(0);
        buf.put_u64(1);
        buf.put_u16(9999); // not a valid MessageKind
        assert!(matches!(
            decode_envelope(buf.freeze()),
            Err(ProtocolError::UnknownKind(9999))
        ));
    }
}
