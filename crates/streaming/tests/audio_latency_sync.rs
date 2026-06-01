//! End-to-end latency sync for follow-mouse audio: frames that arrive jittered,
//! reordered, and with a loss are smoothed into an in-order playout stream
//! through the public API.

use bytes::Bytes;
use coklu_streaming::{AudioCodec, AudioFrame, AudioJitterBuffer, JitterConfig, JitterOutput};

fn frame(sequence: u64) -> AudioFrame {
    AudioFrame {
        sequence,
        capture_time_micros: sequence * 10_000,
        samples_per_channel: 480,
        codec: AudioCodec::Opus,
        payload: Bytes::from(format!("frame-{sequence}")),
    }
}

/// Simulate a jittered/reordered arrival schedule and a single dropped frame,
/// then drive playout and assert the rendered order is monotonic with exactly
/// one concealed gap.
#[test]
fn jittered_arrivals_play_out_in_order() {
    // 10 ms frames, default 30 ms target → prefill 3 frames before playout.
    let mut jb = AudioJitterBuffer::new(JitterConfig::lan_low_latency(10));

    // Frames 0..8 produced, frame 4 is lost, and arrival order is shuffled.
    let arrival_order = [2u64, 0, 1, 5, 3, 7, 6, 8];
    let mut rendered: Vec<u64> = Vec::new();
    let mut gaps = 0u64;

    // Interleave pushes and pops the way a real playback loop would: each "tick"
    // delivers one arriving frame, then renders one slot.
    for &seq in &arrival_order {
        jb.push(frame(seq));
        match jb.pop() {
            JitterOutput::Frame(f) => rendered.push(f.sequence),
            JitterOutput::Gap => gaps += 1,
            JitterOutput::Prefill | JitterOutput::Starved => {}
        }
    }

    // Drain whatever remains buffered.
    loop {
        match jb.pop() {
            JitterOutput::Frame(f) => rendered.push(f.sequence),
            JitterOutput::Gap => gaps += 1,
            JitterOutput::Prefill => continue,
            JitterOutput::Starved => break,
        }
    }

    // Rendered sequence must be strictly increasing (no reorders leaked through).
    assert!(
        rendered.windows(2).all(|w| w[0] < w[1]),
        "playout not in order: {rendered:?}"
    );
    // Every produced, non-lost frame was rendered exactly once.
    assert_eq!(rendered, vec![0, 1, 2, 3, 5, 6, 7, 8]);
    // The single lost frame (4) was concealed.
    assert_eq!(gaps, 1);
    assert_eq!(jb.stats().concealed, 1);
}
