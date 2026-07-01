//! Audio routing and synchronization model.
//!
//! Audio is a continuous stream, not event-bus traffic. This module defines the
//! sans-IO control plane for routing audio between trusted devices: negotiated
//! formats, low-latency frame metadata, follow-mouse routing, shared headset
//! mode, and device switching. Platform-specific capture/playback backends
//! (PipeWire, CoreAudio, WASAPI) implement [`AudioBackend`] behind this safe
//! boundary.
//!
//! The data path remains encrypted by the existing network/session layer. This
//! module never provides an insecure transport shortcut; it only decides *where*
//! audio should flow and how frames are described.

use std::collections::HashMap;

use async_trait::async_trait;
use bytes::{Buf, BufMut, Bytes, BytesMut};
use nexkvm_core::identity::DeviceId;
use serde::{Deserialize, Serialize};

use crate::AudioError;

const FRAME_HEADER_LEN: usize = 8 + 8 + 4 + 1 + 4;

/// Stable id for a local audio endpoint as reported by a platform backend.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AudioDeviceId(pub String);

impl AudioDeviceId {
    /// Construct an endpoint id.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

/// Role of an audio endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioDeviceRole {
    /// Captures system output or microphone/source audio.
    Capture,
    /// Plays audio to speakers/headphones/sink.
    Playback,
    /// Can both capture and play back (e.g. headset profile).
    Duplex,
}

/// One local audio endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioDevice {
    /// Platform-stable id.
    pub id: AudioDeviceId,
    /// User-facing label.
    pub label: String,
    /// Endpoint role.
    pub role: AudioDeviceRole,
    /// Whether this is the current platform default.
    pub is_default: bool,
}

impl AudioDevice {
    /// Construct a playback endpoint.
    #[must_use]
    pub fn playback(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: AudioDeviceId::new(id),
            label: label.into(),
            role: AudioDeviceRole::Playback,
            is_default: false,
        }
    }

    /// Construct a duplex headset endpoint.
    #[must_use]
    pub fn headset(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: AudioDeviceId::new(id),
            label: label.into(),
            role: AudioDeviceRole::Duplex,
            is_default: false,
        }
    }

    /// Mark as platform default.
    #[must_use]
    pub fn with_default(mut self, is_default: bool) -> Self {
        self.is_default = is_default;
        self
    }
}

/// PCM sample representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SampleFormat {
    /// Signed 16-bit little endian PCM.
    S16Le,
    /// 32-bit floating point PCM.
    F32Le,
}

/// Audio codec used on the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioCodec {
    /// Raw PCM, useful for tests/local loops.
    Pcm,
    /// Opus, preferred for low-latency LAN audio.
    Opus,
}

/// Negotiated audio format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioFormat {
    /// Sample rate in Hz.
    pub sample_rate_hz: u32,
    /// Channel count.
    pub channels: u8,
    /// PCM sample format before/after codec.
    pub sample_format: SampleFormat,
    /// Stream codec.
    pub codec: AudioCodec,
    /// Frame duration in milliseconds.
    pub frame_duration_ms: u16,
}

impl AudioFormat {
    /// Low-latency Opus stereo preset.
    #[must_use]
    pub const fn opus_stereo_48k() -> Self {
        Self {
            sample_rate_hz: 48_000,
            channels: 2,
            sample_format: SampleFormat::F32Le,
            codec: AudioCodec::Opus,
            frame_duration_ms: 10,
        }
    }

    /// Estimate samples per channel per frame.
    #[must_use]
    pub const fn samples_per_frame(self) -> u32 {
        (self.sample_rate_hz / 1000) * self.frame_duration_ms as u32
    }
}

impl Default for AudioFormat {
    fn default() -> Self {
        Self::opus_stereo_48k()
    }
}

/// Audio frame metadata and encoded payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioFrame {
    /// Monotonic sequence number for loss/jitter tracking.
    pub sequence: u64,
    /// Capture timestamp in microseconds from the sender's monotonic clock.
    pub capture_time_micros: u64,
    /// Number of decoded samples per channel represented by this frame.
    pub samples_per_channel: u32,
    /// Codec used for payload.
    pub codec: AudioCodec,
    /// Encoded frame payload.
    pub payload: Bytes,
}

impl AudioFrame {
    /// Encode frame to a compact binary stream payload.
    ///
    /// # Errors
    /// Returns [`AudioError::TooLarge`] if the payload exceeds `u32`.
    pub fn encode(&self) -> Result<Bytes, AudioError> {
        let payload_len = u32::try_from(self.payload.len()).map_err(|_| AudioError::TooLarge {
            size: self.payload.len(),
            limit: u32::MAX as usize,
        })?;
        let mut out = BytesMut::with_capacity(FRAME_HEADER_LEN + self.payload.len());
        out.put_u64(self.sequence);
        out.put_u64(self.capture_time_micros);
        out.put_u32(self.samples_per_channel);
        out.put_u8(codec_to_u8(self.codec));
        out.put_u32(payload_len);
        out.put_slice(&self.payload);
        Ok(out.freeze())
    }

    /// Decode frame from stream payload.
    ///
    /// # Errors
    /// Returns [`AudioError::Codec`] on malformed input.
    pub fn decode(mut bytes: Bytes) -> Result<Self, AudioError> {
        if bytes.remaining() < FRAME_HEADER_LEN {
            return Err(AudioError::Codec("truncated audio frame".into()));
        }
        let sequence = bytes.get_u64();
        let capture_time_micros = bytes.get_u64();
        let samples_per_channel = bytes.get_u32();
        let codec = codec_from_u8(bytes.get_u8())?;
        let payload_len = bytes.get_u32() as usize;
        if payload_len != bytes.remaining() {
            return Err(AudioError::Codec("audio payload length mismatch".into()));
        }
        Ok(Self {
            sequence,
            capture_time_micros,
            samples_per_channel,
            codec,
            payload: bytes,
        })
    }
}

/// User-selected audio routing behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioRouteMode {
    /// Audio follows the currently controlled device.
    FollowMouse,
    /// A headset attached to one device is shared with another.
    SharedHeadset,
    /// Keep audio pinned to a selected device.
    Fixed(DeviceId),
}

/// Active audio route decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioRoute {
    /// Source device whose system audio is captured.
    pub source: DeviceId,
    /// Device that should play the audio.
    pub sink: DeviceId,
    /// Whether microphone/headset return audio is expected.
    pub duplex: bool,
}

/// Per-device audio preferences.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioDeviceProfile {
    /// Preferred playback endpoint for this device.
    pub preferred_playback: Option<AudioDeviceId>,
    /// Preferred capture/headset endpoint for this device.
    pub preferred_capture: Option<AudioDeviceId>,
    /// Whether this device may receive follow-mouse audio.
    pub allow_follow_mouse: bool,
    /// Whether this device may participate in shared headset mode.
    pub allow_shared_headset: bool,
}

impl Default for AudioDeviceProfile {
    fn default() -> Self {
        Self {
            preferred_playback: None,
            preferred_capture: None,
            allow_follow_mouse: true,
            allow_shared_headset: true,
        }
    }
}

/// In-memory audio routing policy engine.
#[derive(Debug, Clone)]
pub struct AudioRouter {
    local_device: DeviceId,
    mode: AudioRouteMode,
    active_control: DeviceId,
    shared_headset_host: Option<DeviceId>,
    profiles: HashMap<DeviceId, AudioDeviceProfile>,
}

impl AudioRouter {
    /// Create a router rooted at the local device.
    #[must_use]
    pub fn new(local_device: DeviceId) -> Self {
        Self {
            local_device,
            mode: AudioRouteMode::FollowMouse,
            active_control: local_device,
            shared_headset_host: None,
            profiles: HashMap::new(),
        }
    }

    /// Set routing mode.
    pub fn set_mode(&mut self, mode: AudioRouteMode) {
        self.mode = mode;
    }

    /// Update the device currently under pointer/keyboard control.
    pub fn set_active_control(&mut self, device: DeviceId) {
        self.active_control = device;
    }

    /// Set per-device audio profile.
    pub fn set_profile(&mut self, device: DeviceId, profile: AudioDeviceProfile) {
        self.profiles.insert(device, profile);
    }

    /// Enable shared headset mode hosted by `device`.
    pub fn share_headset_from(&mut self, device: DeviceId) {
        self.shared_headset_host = Some(device);
        self.mode = AudioRouteMode::SharedHeadset;
    }

    /// Resolve the current route, if policy permits one.
    #[must_use]
    pub fn current_route(&self) -> Option<AudioRoute> {
        match self.mode {
            AudioRouteMode::FollowMouse => self.follow_mouse_route(),
            AudioRouteMode::SharedHeadset => self.shared_headset_route(),
            AudioRouteMode::Fixed(sink) => Some(AudioRoute {
                source: self.local_device,
                sink,
                duplex: false,
            }),
        }
    }

    fn follow_mouse_route(&self) -> Option<AudioRoute> {
        let sink = self.active_control;
        let profile = self.profiles.get(&sink).cloned().unwrap_or_default();
        profile.allow_follow_mouse.then_some(AudioRoute {
            source: self.local_device,
            sink,
            duplex: false,
        })
    }

    fn shared_headset_route(&self) -> Option<AudioRoute> {
        let sink = self.shared_headset_host?;
        let profile = self.profiles.get(&sink).cloned().unwrap_or_default();
        profile.allow_shared_headset.then_some(AudioRoute {
            source: self.local_device,
            sink,
            duplex: true,
        })
    }
}

/// Platform audio backend boundary.
///
/// Implementors must keep blocking platform loops (PipeWire main loop,
/// CoreAudio callbacks, WASAPI event loops) off Tokio worker threads and bridge
/// audio frames through bounded channels.
#[async_trait]
pub trait AudioBackend: Send + Sync {
    /// List known local endpoints.
    async fn devices(&self) -> Result<Vec<AudioDevice>, AudioError>;

    /// Switch the platform default playback endpoint.
    async fn switch_playback_device(&self, device: &AudioDeviceId) -> Result<(), AudioError>;

    /// Preferred stream format for this backend.
    fn preferred_format(&self) -> AudioFormat;
}

/// Platform audio stream boundary for frame capture and playback.
///
/// Implementors own the OS-specific stream pump (PipeWire streams, CoreAudio
/// callbacks, WASAPI clients) and expose one frame-at-a-time operations to the
/// routing/session layer.
#[async_trait]
pub trait AudioStreamBackend: Send + Sync {
    /// Capture one audio frame from `device`.
    async fn capture_audio_frame(&self, device: &AudioDeviceId) -> Result<AudioFrame, AudioError>;

    /// Play one audio frame to `device`.
    async fn play_audio_frame(
        &self,
        device: &AudioDeviceId,
        frame: AudioFrame,
    ) -> Result<(), AudioError>;
}

/// Capture one frame from `source` and deliver it to `sink`.
///
/// This is intentionally sans-I/O orchestration: platform implementations
/// provide capture/playback, while session code can use this as the smallest
/// testable routing unit.
///
/// # Errors
/// Returns any capture or playback error from the backend.
pub async fn route_audio_frame_once<B>(
    backend: &B,
    source: &AudioDeviceId,
    sink: &AudioDeviceId,
) -> Result<AudioFrame, AudioError>
where
    B: AudioStreamBackend + ?Sized,
{
    let frame = backend.capture_audio_frame(source).await?;
    backend.play_audio_frame(sink, frame.clone()).await?;
    Ok(frame)
}

fn codec_to_u8(codec: AudioCodec) -> u8 {
    match codec {
        AudioCodec::Pcm => 0,
        AudioCodec::Opus => 1,
    }
}

fn codec_from_u8(raw: u8) -> Result<AudioCodec, AudioError> {
    match raw {
        0 => Ok(AudioCodec::Pcm),
        1 => Ok(AudioCodec::Opus),
        _ => Err(AudioError::Codec(format!("unknown audio codec {raw}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_frame_round_trips() {
        let frame = AudioFrame {
            sequence: 42,
            capture_time_micros: 1_000,
            samples_per_channel: 480,
            codec: AudioCodec::Opus,
            payload: Bytes::from_static(b"opus-frame"),
        };
        let decoded = AudioFrame::decode(frame.encode().unwrap()).unwrap();
        assert_eq!(decoded, frame);
    }

    #[test]
    fn rejects_truncated_frame() {
        assert!(matches!(
            AudioFrame::decode(Bytes::from_static(b"short")),
            Err(AudioError::Codec(_))
        ));
    }

    #[test]
    fn follow_mouse_tracks_active_device() {
        let local = DeviceId::generate();
        let peer = DeviceId::generate();
        let mut router = AudioRouter::new(local);
        router.set_active_control(peer);
        let route = router.current_route().unwrap();
        assert_eq!(route.source, local);
        assert_eq!(route.sink, peer);
        assert!(!route.duplex);
    }

    #[test]
    fn follow_mouse_honors_profile_opt_out() {
        let local = DeviceId::generate();
        let peer = DeviceId::generate();
        let mut router = AudioRouter::new(local);
        router.set_active_control(peer);
        router.set_profile(
            peer,
            AudioDeviceProfile {
                allow_follow_mouse: false,
                ..AudioDeviceProfile::default()
            },
        );
        assert!(router.current_route().is_none());
    }

    #[test]
    fn shared_headset_is_duplex() {
        let local = DeviceId::generate();
        let headset_host = DeviceId::generate();
        let mut router = AudioRouter::new(local);
        router.share_headset_from(headset_host);
        let route = router.current_route().unwrap();
        assert_eq!(route.sink, headset_host);
        assert!(route.duplex);
    }

    #[test]
    fn fixed_route_pins_sink() {
        let local = DeviceId::generate();
        let sink = DeviceId::generate();
        let other = DeviceId::generate();
        let mut router = AudioRouter::new(local);
        router.set_mode(AudioRouteMode::Fixed(sink));
        router.set_active_control(other);
        assert_eq!(router.current_route().unwrap().sink, sink);
    }

    #[test]
    fn format_reports_samples_per_frame() {
        assert_eq!(AudioFormat::opus_stereo_48k().samples_per_frame(), 480);
    }

    #[tokio::test]
    async fn routes_one_captured_audio_frame_to_playback_endpoint() {
        use std::sync::{Arc, Mutex};

        #[derive(Debug, Clone)]
        struct LoopbackStream {
            captured: AudioFrame,
            played: Arc<Mutex<Vec<(AudioDeviceId, AudioFrame)>>>,
        }

        #[async_trait]
        impl AudioStreamBackend for LoopbackStream {
            async fn capture_audio_frame(
                &self,
                _device: &AudioDeviceId,
            ) -> Result<AudioFrame, AudioError> {
                Ok(self.captured.clone())
            }

            async fn play_audio_frame(
                &self,
                device: &AudioDeviceId,
                frame: AudioFrame,
            ) -> Result<(), AudioError> {
                self.played.lock().unwrap().push((device.clone(), frame));
                Ok(())
            }
        }

        let captured = AudioFrame {
            sequence: 7,
            capture_time_micros: 70,
            samples_per_channel: 480,
            codec: AudioCodec::Pcm,
            payload: Bytes::from_static(b"pcm"),
        };
        let played = Arc::new(Mutex::new(Vec::new()));
        let stream = LoopbackStream {
            captured: captured.clone(),
            played: played.clone(),
        };

        let routed = route_audio_frame_once(
            &stream,
            &AudioDeviceId::new("capture"),
            &AudioDeviceId::new("playback"),
        )
        .await
        .unwrap();

        assert_eq!(routed, captured);
        assert_eq!(
            played.lock().unwrap().as_slice(),
            &[(AudioDeviceId::new("playback"), captured)]
        );
    }
}
