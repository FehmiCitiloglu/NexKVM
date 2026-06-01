#![no_main]

use bytes::{Bytes, BytesMut};
use coklu_network::wire::decode_envelope;
use coklu_protocol::FrameCodec;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = decode_envelope(Bytes::copy_from_slice(data));

    let mut framed = BytesMut::from(data);
    let codec = FrameCodec;
    for _ in 0..4 {
        match codec.decode(&mut framed) {
            Ok(Some(frame)) => {
                let _ = decode_envelope(frame);
            }
            Ok(None) | Err(_) => break,
        }
    }
});
