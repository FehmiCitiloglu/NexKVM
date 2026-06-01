//! Length-prefixed framing for stream transports (TCP, QUIC streams).
//!
//! Wire layout: a 4-byte big-endian unsigned length prefix followed by exactly
//! that many payload bytes. This codec is transport-agnostic and synchronous —
//! the async I/O lives in the `network` crate, which feeds bytes into
//! [`FrameCodec::decode`] as they arrive. Datagram transports (QUIC datagrams,
//! WebRTC data channels) carry one payload per datagram and skip framing.

use bytes::{Buf, BufMut, Bytes, BytesMut};

use crate::ProtocolError;

/// Maximum accepted frame payload length (16 MiB).
///
/// Guards against a malicious peer advertising a huge length to exhaust memory.
/// Large transfers (files, media) must be chunked below this bound.
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

const LEN_PREFIX: usize = 4;

/// Stateless encoder/decoder for length-prefixed frames.
#[derive(Debug, Default, Clone, Copy)]
pub struct FrameCodec;

impl FrameCodec {
    /// Append a framed `payload` to `dst`.
    ///
    /// # Errors
    /// Returns [`ProtocolError::FrameTooLarge`] if `payload` exceeds
    /// [`MAX_FRAME_LEN`].
    pub fn encode(self, payload: &[u8], dst: &mut BytesMut) -> Result<(), ProtocolError> {
        if payload.len() > MAX_FRAME_LEN {
            return Err(ProtocolError::FrameTooLarge {
                len: payload.len(),
                max: MAX_FRAME_LEN,
            });
        }
        dst.reserve(LEN_PREFIX + payload.len());
        dst.put_u32(payload.len() as u32);
        dst.put_slice(payload);
        Ok(())
    }

    /// Try to decode a single frame from the front of `src`.
    ///
    /// On success the frame bytes are split off (zero-copy) and returned, and
    /// `src` advances past the consumed bytes. If a full frame is not yet
    /// available, returns `Ok(None)` and leaves `src` untouched so the caller
    /// can read more from the transport.
    ///
    /// # Errors
    /// Returns [`ProtocolError::FrameTooLarge`] if the advertised length is out
    /// of bounds.
    pub fn decode(self, src: &mut BytesMut) -> Result<Option<Bytes>, ProtocolError> {
        if src.len() < LEN_PREFIX {
            return Ok(None);
        }

        // Peek the length prefix without consuming it yet.
        let len = u32::from_be_bytes([src[0], src[1], src[2], src[3]]) as usize;
        if len > MAX_FRAME_LEN {
            return Err(ProtocolError::FrameTooLarge {
                len,
                max: MAX_FRAME_LEN,
            });
        }

        if src.len() < LEN_PREFIX + len {
            return Ok(None);
        }

        src.advance(LEN_PREFIX);
        Ok(Some(src.split_to(len).freeze()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_frame() {
        let codec = FrameCodec;
        let mut buf = BytesMut::new();
        codec.encode(b"hello", &mut buf).unwrap();
        let frame = codec.decode(&mut buf).unwrap().expect("full frame");
        assert_eq!(&frame[..], b"hello");
        assert!(buf.is_empty());
    }

    #[test]
    fn returns_none_until_complete() {
        let codec = FrameCodec;
        let mut buf = BytesMut::new();
        buf.put_u32(4);
        assert!(codec.decode(&mut buf).unwrap().is_none());
        buf.put_slice(b"ab");
        assert!(codec.decode(&mut buf).unwrap().is_none());
        buf.put_slice(b"cd");
        let frame = codec.decode(&mut buf).unwrap().expect("complete");
        assert_eq!(&frame[..], b"abcd");
    }

    #[test]
    fn rejects_oversized_length() {
        let codec = FrameCodec;
        let mut buf = BytesMut::new();
        buf.put_u32((MAX_FRAME_LEN + 1) as u32);
        assert!(matches!(
            codec.decode(&mut buf),
            Err(ProtocolError::FrameTooLarge { .. })
        ));
    }
}
