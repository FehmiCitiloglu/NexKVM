//! Playback-side latency synchronization (jitter buffer).
//!
//! Follow-mouse audio rides a reliable/ordered transport, but real networks
//! still deliver frames with variable inter-arrival timing (jitter) and the
//! occasional reorder or loss. Feeding the platform sink directly from the wire
//! produces audible glitches. This module is the *sans-IO* control plane that
//! absorbs that variance: it buffers a small, bounded amount of audio (the
//! "playout delay"), reorders frames by sequence, conceals losses, and emits
//! frames in order at the sink's steady cadence.
//!
//! It owns no clock, no thread, and no OS calls — the platform playback driver
//! pushes arriving [`AudioFrame`]s and, once per frame interval, pops the next
//! frame to render. That keeps it deterministic and unit-testable. The
//! buffered depth *is* the latency knob: deeper absorbs more jitter at the cost
//! of latency, targeting the research budget of < ~50 ms on LAN.

use std::collections::BTreeMap;

use crate::AudioFrame;
use crate::audio::AudioFormat;

/// Tuning for the jitter buffer's playout delay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitterConfig {
    /// Duration of one frame in milliseconds (from the negotiated format).
    pub frame_duration_ms: u16,
    /// Target playout delay to prefill before starting playback.
    pub target_delay_ms: u16,
    /// Hard cap on buffered audio; excess fast-forwards to bound latency.
    pub max_delay_ms: u16,
}

impl JitterConfig {
    /// Low-latency LAN preset: 30 ms target, 120 ms cap.
    #[must_use]
    pub const fn lan_low_latency(frame_duration_ms: u16) -> Self {
        Self {
            frame_duration_ms,
            target_delay_ms: 30,
            max_delay_ms: 120,
        }
    }

    /// Build a config from a negotiated [`AudioFormat`].
    #[must_use]
    pub const fn from_format(format: AudioFormat) -> Self {
        Self::lan_low_latency(format.frame_duration_ms)
    }

    /// Frames to prefill before playout starts (at least one).
    #[must_use]
    pub const fn target_frames(self) -> usize {
        frames_for(self.target_delay_ms, self.frame_duration_ms)
    }

    /// Maximum frames to keep buffered before fast-forwarding.
    #[must_use]
    pub const fn max_frames(self) -> usize {
        let max = frames_for(self.max_delay_ms, self.frame_duration_ms);
        let target = self.target_frames();
        if max > target { max } else { target + 1 }
    }
}

impl Default for JitterConfig {
    fn default() -> Self {
        Self::lan_low_latency(AudioFormat::opus_stereo_48k().frame_duration_ms)
    }
}

const fn frames_for(duration_ms: u16, frame_duration_ms: u16) -> usize {
    if frame_duration_ms == 0 {
        return 1;
    }
    // Round up so we never under-buffer the requested delay.
    let frames = duration_ms.div_ceil(frame_duration_ms) as usize;
    if frames == 0 { 1 } else { frames }
}

/// Result of pushing a frame into the buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    /// Frame accepted and queued for playout.
    Buffered,
    /// Frame's sequence was already played out; dropped as too late.
    TooLate,
    /// A frame with this sequence is already buffered; ignored.
    Duplicate,
    /// Frame accepted, but an older buffered frame was dropped to bound latency.
    Overflowed,
}

/// Result of popping the next frame to render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JitterOutput {
    /// Still prefilling to the target delay; the sink should play silence.
    Prefill,
    /// Next in-order frame to render.
    Frame(AudioFrame),
    /// The expected frame is missing but later audio exists: conceal (e.g.
    /// silence/PLC) and advance. Reported as a loss.
    Gap,
    /// Buffer is empty mid-stream; playout pauses and re-prefills.
    Starved,
}

/// Running quality counters for diagnostics and adaptive tuning.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JitterStats {
    /// Frames dropped because they arrived after their playout slot.
    pub late_drops: u64,
    /// Concealed gaps (lost frames skipped during playout).
    pub concealed: u64,
    /// Times the buffer emptied mid-stream and had to re-prefill.
    pub underruns: u64,
    /// Frames evicted to keep buffered latency under the cap.
    pub overflow_drops: u64,
    /// Peak number of frames held at once.
    pub peak_depth: usize,
}

/// Reordering, loss-concealing playout buffer for one audio stream.
///
/// Push frames as they arrive (any order); call [`pop`](Self::pop) once per
/// frame interval to drive the sink.
#[derive(Debug, Clone)]
pub struct AudioJitterBuffer {
    config: JitterConfig,
    buffer: BTreeMap<u64, AudioFrame>,
    next_seq: Option<u64>,
    playing: bool,
    stats: JitterStats,
}

impl AudioJitterBuffer {
    /// Create an empty buffer with the given config.
    #[must_use]
    pub fn new(config: JitterConfig) -> Self {
        Self {
            config,
            buffer: BTreeMap::new(),
            next_seq: None,
            playing: false,
            stats: JitterStats::default(),
        }
    }

    /// Queue an arriving frame, absorbing reorders and dropping stale/duplicate
    /// frames. Returns how the frame was handled.
    pub fn push(&mut self, frame: AudioFrame) -> PushOutcome {
        let seq = frame.sequence;
        if let Some(next) = self.next_seq
            && seq < next
        {
            self.stats.late_drops += 1;
            return PushOutcome::TooLate;
        }
        if self.buffer.contains_key(&seq) {
            return PushOutcome::Duplicate;
        }
        self.buffer.insert(seq, frame);
        if self.buffer.len() > self.stats.peak_depth {
            self.stats.peak_depth = self.buffer.len();
        }
        if self.buffer.len() > self.config.max_frames() {
            // Fast-forward: evict the oldest to keep latency bounded.
            if let Some((&oldest, _)) = self.buffer.iter().next() {
                self.buffer.remove(&oldest);
                self.next_seq = Some(oldest + 1);
                self.stats.overflow_drops += 1;
            }
            return PushOutcome::Overflowed;
        }
        PushOutcome::Buffered
    }

    /// Pop the next frame to render. Drives the steady playout cadence.
    pub fn pop(&mut self) -> JitterOutput {
        if !self.playing {
            if self.buffer.len() < self.config.target_frames() {
                return JitterOutput::Prefill;
            }
            self.playing = true;
            self.next_seq = self.buffer.keys().next().copied();
        }

        let Some(next) = self.next_seq else {
            self.playing = false;
            return JitterOutput::Starved;
        };

        if let Some(frame) = self.buffer.remove(&next) {
            self.next_seq = Some(next + 1);
            JitterOutput::Frame(frame)
        } else if self.buffer.is_empty() {
            self.playing = false;
            self.next_seq = None;
            self.stats.underruns += 1;
            JitterOutput::Starved
        } else {
            // Later frames exist, so `next` is genuinely lost: conceal and skip.
            self.next_seq = Some(next + 1);
            self.stats.concealed += 1;
            JitterOutput::Gap
        }
    }

    /// Currently buffered audio expressed as latency in milliseconds.
    #[must_use]
    pub fn buffered_latency_ms(&self) -> u32 {
        self.buffer.len() as u32 * u32::from(self.config.frame_duration_ms)
    }

    /// Number of frames currently buffered.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.buffer.len()
    }

    /// Whether playout has started (past the initial prefill).
    #[must_use]
    pub fn is_playing(&self) -> bool {
        self.playing
    }

    /// Snapshot of quality counters.
    #[must_use]
    pub fn stats(&self) -> JitterStats {
        self.stats
    }

    /// Active configuration.
    #[must_use]
    pub fn config(&self) -> JitterConfig {
        self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::AudioCodec;
    use bytes::Bytes;

    fn frame(sequence: u64) -> AudioFrame {
        AudioFrame {
            sequence,
            capture_time_micros: sequence * 10_000,
            samples_per_channel: 480,
            codec: AudioCodec::Opus,
            payload: Bytes::from(format!("f{sequence}")),
        }
    }

    fn cfg() -> JitterConfig {
        // 10 ms frames, 30 ms target → prefill 3 frames.
        JitterConfig::lan_low_latency(10)
    }

    #[test]
    fn prefills_before_playing() {
        let mut jb = AudioJitterBuffer::new(cfg());
        assert_eq!(jb.config().target_frames(), 3);
        assert_eq!(jb.push(frame(0)), PushOutcome::Buffered);
        assert_eq!(jb.pop(), JitterOutput::Prefill);
        jb.push(frame(1));
        jb.push(frame(2));
        // Target reached → starts playing in order.
        assert_eq!(jb.pop(), JitterOutput::Frame(frame(0)));
        assert!(jb.is_playing());
        assert_eq!(jb.pop(), JitterOutput::Frame(frame(1)));
    }

    #[test]
    fn reorders_out_of_order_arrivals() {
        let mut jb = AudioJitterBuffer::new(cfg());
        jb.push(frame(2));
        jb.push(frame(0));
        jb.push(frame(1));
        assert_eq!(jb.pop(), JitterOutput::Frame(frame(0)));
        assert_eq!(jb.pop(), JitterOutput::Frame(frame(1)));
        assert_eq!(jb.pop(), JitterOutput::Frame(frame(2)));
    }

    #[test]
    fn conceals_lost_frame() {
        let mut jb = AudioJitterBuffer::new(cfg());
        jb.push(frame(0));
        jb.push(frame(1));
        // frame 2 lost
        jb.push(frame(3));
        assert_eq!(jb.pop(), JitterOutput::Frame(frame(0)));
        assert_eq!(jb.pop(), JitterOutput::Frame(frame(1)));
        assert_eq!(jb.pop(), JitterOutput::Gap);
        assert_eq!(jb.pop(), JitterOutput::Frame(frame(3)));
        assert_eq!(jb.stats().concealed, 1);
    }

    #[test]
    fn drops_late_frame_after_playout() {
        let mut jb = AudioJitterBuffer::new(cfg());
        jb.push(frame(0));
        jb.push(frame(1));
        jb.push(frame(2));
        assert_eq!(jb.pop(), JitterOutput::Frame(frame(0)));
        // frame 0 shows up again, too late.
        assert_eq!(jb.push(frame(0)), PushOutcome::TooLate);
        assert_eq!(jb.stats().late_drops, 1);
    }

    #[test]
    fn rejects_duplicate() {
        let mut jb = AudioJitterBuffer::new(cfg());
        assert_eq!(jb.push(frame(5)), PushOutcome::Buffered);
        assert_eq!(jb.push(frame(5)), PushOutcome::Duplicate);
    }

    #[test]
    fn fast_forwards_on_overflow() {
        // max 120 ms / 10 ms = 12 frames cap.
        let mut jb = AudioJitterBuffer::new(cfg());
        for seq in 0..=jb.config().max_frames() as u64 {
            jb.push(frame(seq));
        }
        assert!(jb.depth() <= jb.config().max_frames());
        assert_eq!(jb.stats().overflow_drops, 1);
    }

    #[test]
    fn starves_then_reprefills() {
        let mut jb = AudioJitterBuffer::new(cfg());
        jb.push(frame(0));
        jb.push(frame(1));
        jb.push(frame(2));
        jb.pop();
        jb.pop();
        jb.pop();
        // Drained mid-stream → starve, playout pauses.
        assert_eq!(jb.pop(), JitterOutput::Starved);
        assert!(!jb.is_playing());
        assert_eq!(jb.stats().underruns, 1);
    }

    #[test]
    fn reports_buffered_latency() {
        let mut jb = AudioJitterBuffer::new(cfg());
        jb.push(frame(0));
        jb.push(frame(1));
        assert_eq!(jb.buffered_latency_ms(), 20);
    }
}
