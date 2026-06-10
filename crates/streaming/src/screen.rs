//! Screen streaming capability negotiation and frame model.
//!
//! This module is deliberately sans-IO. Platform crates provide capture through
//! [`ScreenCaptureBackend`] using Screen Recording/Window Capture APIs on macOS,
//! PipeWire portals on Wayland, X11 capture as a fallback on Linux, and Desktop
//! Duplication/Graphics Capture on Windows. Hardware encoders implement
//! [`ScreenEncoderBackend`] behind feature flags. The network layer remains
//! responsible for authenticated, encrypted transport and replay protection.

use async_trait::async_trait;
use bytes::{Buf, BufMut, Bytes, BytesMut};
use nexkvm_core::identity::DeviceId;
use serde::{Deserialize, Serialize};

use crate::ScreenError;

const ENCODED_FRAME_HEADER_LEN: usize = 8 + 8 + 4 + 4 + 1 + 1 + 1 + 1 + 4;

/// Stable id for a display, window, or app capture source.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CaptureSourceId(pub String);

impl CaptureSourceId {
    /// Construct a capture source id.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

/// Visibility state used to protect window/app preview privacy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WindowVisibility {
    /// Source is visible and safe to preview.
    Visible,
    /// Source is minimized; only app metadata/thumbnail cache may be shown.
    Minimized,
    /// Source is hidden/occluded and should not be streamed without explicit OS support.
    Hidden,
}

/// Platform-neutral capture source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CaptureSource {
    /// Full display capture.
    Display {
        /// Stable display id.
        id: CaptureSourceId,
        /// User-facing label.
        label: String,
    },
    /// Single-window capture for window peeking.
    Window {
        /// Stable window id.
        id: CaptureSourceId,
        /// User-facing label.
        title: String,
        /// Owning application id/name when known.
        app_id: Option<String>,
        /// Whether the window is currently visible.
        visibility: WindowVisibility,
    },
    /// Application-scoped capture for instant app previews.
    Application {
        /// Stable app id.
        id: CaptureSourceId,
        /// User-facing app name.
        name: String,
    },
}

impl CaptureSource {
    /// Whether this source can be previewed without capturing an unavailable surface.
    #[must_use]
    pub const fn is_previewable(&self) -> bool {
        match self {
            Self::Display { .. } | Self::Application { .. } => true,
            Self::Window { visibility, .. } => matches!(visibility, WindowVisibility::Visible),
        }
    }
}

/// Encoded video codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ScreenCodec {
    /// Raw RGBA frames for tests/local fallback. Not suitable for remote mode.
    RawRgba,
    /// H.264/AVC.
    H264,
    /// H.265/HEVC.
    H265,
}

impl ScreenCodec {
    /// Whether this codec is intended for bandwidth-efficient remote streaming.
    #[must_use]
    pub const fn is_compressed(self) -> bool {
        matches!(self, Self::H264 | Self::H265)
    }
}

/// Encoder implementation family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HardwareEncoder {
    /// CPU software encoder fallback.
    Software,
    /// NVIDIA NVENC.
    Nvenc,
    /// Linux VAAPI.
    Vaapi,
    /// macOS/iOS VideoToolbox.
    VideoToolbox,
}

impl HardwareEncoder {
    /// Whether this encoder uses platform GPU/video hardware.
    #[must_use]
    pub const fn is_gpu_accelerated(self) -> bool {
        !matches!(self, Self::Software)
    }

    /// Whether the matching Cargo feature is currently enabled.
    #[must_use]
    pub const fn feature_enabled(self) -> bool {
        match self {
            Self::Software => true,
            Self::Nvenc => cfg!(feature = "screen-nvenc"),
            Self::Vaapi => cfg!(feature = "screen-vaapi"),
            Self::VideoToolbox => cfg!(feature = "screen-videotoolbox"),
        }
    }
}

/// Pixel format of unencoded capture frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PixelFormat {
    /// 8-bit RGBA.
    Rgba8,
    /// 8-bit BGRA, common for desktop capture APIs.
    Bgra8,
    /// NV12 YUV 4:2:0, common hardware encoder input.
    Nv12,
}

/// Memory backing for capture frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GpuMemoryKind {
    /// CPU/system memory.
    System,
    /// Linux DMA-BUF handle exported by compositor/capture stack.
    DmaBuf,
    /// macOS IOSurface/CoreVideo buffer.
    IoSurface,
    /// Windows D3D texture handle.
    D3D11Texture,
}

impl GpuMemoryKind {
    /// Whether this memory can support a zero-copy GPU encode path.
    #[must_use]
    pub const fn is_gpu(self) -> bool {
        !matches!(self, Self::System)
    }
}

/// Frame dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ScreenResolution {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl ScreenResolution {
    /// Construct a resolution.
    #[must_use]
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    /// 1080p preset.
    #[must_use]
    pub const fn full_hd() -> Self {
        Self::new(1920, 1080)
    }

    /// 720p preset.
    #[must_use]
    pub const fn hd() -> Self {
        Self::new(1280, 720)
    }

    /// Small preview preset.
    #[must_use]
    pub const fn preview() -> Self {
        Self::new(480, 270)
    }

    /// Pixel count.
    #[must_use]
    pub const fn pixels(self) -> u64 {
        self.width as u64 * self.height as u64
    }

    fn min_by_pixels(self, other: Self) -> Self {
        if self.pixels() <= other.pixels() {
            self
        } else {
            other
        }
    }
}

/// Optional UX affordances supported by a capture backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenFeatureSet {
    /// Low bitrate live remote thumbnails.
    pub mini_remote_preview: bool,
    /// Single-window preview without full display streaming.
    pub window_peeking: bool,
    /// App-scoped previews for fast app switching.
    pub instant_app_preview: bool,
}

impl ScreenFeatureSet {
    /// No advanced preview features.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            mini_remote_preview: false,
            window_peeking: false,
            instant_app_preview: false,
        }
    }

    /// All planned screen UX features.
    #[must_use]
    pub const fn all() -> Self {
        Self {
            mini_remote_preview: true,
            window_peeking: true,
            instant_app_preview: true,
        }
    }

    fn supports(self, intent: ScreenStreamIntent) -> bool {
        match intent {
            ScreenStreamIntent::InteractiveRemote => true,
            ScreenStreamIntent::MiniRemotePreview => self.mini_remote_preview,
            ScreenStreamIntent::WindowPeek => self.window_peeking,
            ScreenStreamIntent::InstantAppPreview => self.instant_app_preview,
        }
    }
}

/// Current OS permission state for display/window capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenPermissions {
    /// Full display capture permission is granted.
    pub display_capture: bool,
    /// Window-specific capture permission/API is granted.
    pub window_capture: bool,
    /// App-scoped capture/listing permission is granted.
    pub app_capture: bool,
    /// OS prompt or portal request is still pending.
    pub permission_pending: bool,
}

impl ScreenPermissions {
    /// Conservative baseline.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            display_capture: false,
            window_capture: false,
            app_capture: false,
            permission_pending: false,
        }
    }

    /// Fully granted test/development state.
    #[must_use]
    pub const fn granted() -> Self {
        Self {
            display_capture: true,
            window_capture: true,
            app_capture: true,
            permission_pending: false,
        }
    }

    fn permits(self, source: &CaptureSource) -> bool {
        match source {
            CaptureSource::Display { .. } => self.display_capture,
            CaptureSource::Window { .. } => self.window_capture,
            CaptureSource::Application { .. } => self.app_capture,
        }
    }
}

/// Capture/encode capabilities for one device/backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenStreamCapabilities {
    /// Capture permissions available right now.
    pub permissions: ScreenPermissions,
    /// Supported capture memory kinds.
    pub memory_kinds: Vec<GpuMemoryKind>,
    /// Codecs available to this endpoint.
    pub codecs: Vec<ScreenCodec>,
    /// Encoder families available to this endpoint.
    pub encoders: Vec<HardwareEncoder>,
    /// Max outgoing capture resolution.
    pub max_resolution: ScreenResolution,
    /// Max outgoing capture FPS.
    pub max_fps: u16,
    /// Supported preview/peek/app affordances.
    pub features: ScreenFeatureSet,
}

impl ScreenStreamCapabilities {
    /// Conservative no-capture baseline.
    #[must_use]
    pub fn none() -> Self {
        Self {
            permissions: ScreenPermissions::none(),
            memory_kinds: vec![GpuMemoryKind::System],
            codecs: Vec::new(),
            encoders: Vec::new(),
            max_resolution: ScreenResolution::preview(),
            max_fps: 0,
            features: ScreenFeatureSet::none(),
        }
    }

    /// Typical local LAN baseline for tests and early integrations.
    #[must_use]
    pub fn software_h264() -> Self {
        Self {
            permissions: ScreenPermissions::granted(),
            memory_kinds: vec![GpuMemoryKind::System],
            codecs: vec![ScreenCodec::H264, ScreenCodec::RawRgba],
            encoders: vec![HardwareEncoder::Software],
            max_resolution: ScreenResolution::full_hd(),
            max_fps: 60,
            features: ScreenFeatureSet::all(),
        }
    }

    /// Whether a memory kind is supported.
    #[must_use]
    pub fn supports_memory(&self, memory: GpuMemoryKind) -> bool {
        self.memory_kinds.contains(&memory)
    }

    /// Whether a codec is supported.
    #[must_use]
    pub fn supports_codec(&self, codec: ScreenCodec) -> bool {
        self.codecs.contains(&codec)
    }

    /// Whether an encoder is supported and compiled in.
    #[must_use]
    pub fn supports_encoder(&self, encoder: HardwareEncoder) -> bool {
        self.encoders.contains(&encoder) && encoder.feature_enabled()
    }
}

/// User-facing stream intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScreenStreamIntent {
    /// Tiny always-on device thumbnail.
    MiniRemotePreview,
    /// Peek a selected window.
    WindowPeek,
    /// Preview an app before switching focus to it.
    InstantAppPreview,
    /// Full interactive remote-control stream.
    InteractiveRemote,
}

/// Quality/latency preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScreenQualityPreset {
    /// Small preview, low bitrate.
    Preview,
    /// Low latency interactive stream.
    Interactive,
    /// High quality full-screen stream.
    Quality,
}

impl ScreenQualityPreset {
    fn defaults(self) -> (ScreenResolution, u16, u32) {
        match self {
            Self::Preview => (ScreenResolution::preview(), 15, 700),
            Self::Interactive => (ScreenResolution::hd(), 60, 8_000),
            Self::Quality => (ScreenResolution::full_hd(), 60, 16_000),
        }
    }
}

/// Request to plan a screen stream between trusted devices.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenStreamRequest {
    /// Sending/capturing device.
    pub from: DeviceId,
    /// Receiving device.
    pub to: DeviceId,
    /// Capture source.
    pub source: CaptureSource,
    /// UX intent.
    pub intent: ScreenStreamIntent,
    /// Quality preset.
    pub quality: ScreenQualityPreset,
    /// Preferred codecs in order.
    pub preferred_codecs: Vec<ScreenCodec>,
    /// Whether GPU acceleration should be preferred.
    pub prefer_gpu: bool,
    /// Whether software encoder fallback is acceptable.
    pub allow_software_fallback: bool,
    /// Require GPU memory to remain zero-copy into the encoder.
    pub require_zero_copy: bool,
}

impl ScreenStreamRequest {
    /// Build a mini remote preview request.
    #[must_use]
    pub fn mini_preview(from: DeviceId, to: DeviceId, source: CaptureSource) -> Self {
        Self {
            from,
            to,
            source,
            intent: ScreenStreamIntent::MiniRemotePreview,
            quality: ScreenQualityPreset::Preview,
            preferred_codecs: vec![ScreenCodec::H265, ScreenCodec::H264],
            prefer_gpu: true,
            allow_software_fallback: true,
            require_zero_copy: false,
        }
    }

    /// Build an interactive remote-control request.
    #[must_use]
    pub fn interactive(from: DeviceId, to: DeviceId, source: CaptureSource) -> Self {
        Self {
            from,
            to,
            source,
            intent: ScreenStreamIntent::InteractiveRemote,
            quality: ScreenQualityPreset::Interactive,
            preferred_codecs: vec![ScreenCodec::H264, ScreenCodec::H265],
            prefer_gpu: true,
            allow_software_fallback: true,
            require_zero_copy: false,
        }
    }
}

/// Negotiated screen stream plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenStreamPlan {
    /// Sender device.
    pub from: DeviceId,
    /// Receiver device.
    pub to: DeviceId,
    /// Capture source.
    pub source: CaptureSource,
    /// Stream intent.
    pub intent: ScreenStreamIntent,
    /// Selected codec.
    pub codec: ScreenCodec,
    /// Selected encoder family.
    pub encoder: HardwareEncoder,
    /// Capture memory kind.
    pub memory: GpuMemoryKind,
    /// Target resolution.
    pub resolution: ScreenResolution,
    /// Target frame rate.
    pub fps: u16,
    /// Target video bitrate in kilobits per second.
    pub bitrate_kbps: u32,
    /// Whether this plan can avoid CPU readback before encode.
    pub zero_copy: bool,
    /// Transport must be authenticated/encrypted by the network/session layer.
    pub requires_encrypted_transport: bool,
}

/// Negotiate a screen stream plan from sender and receiver capabilities.
///
/// # Errors
/// Returns [`ScreenError`] when permissions, UX feature support, codec, encoder,
/// or zero-copy requirements cannot be satisfied.
pub fn negotiate_screen_stream(
    sender: &ScreenStreamCapabilities,
    receiver: &ScreenStreamCapabilities,
    request: ScreenStreamRequest,
) -> Result<ScreenStreamPlan, ScreenError> {
    if !sender.permissions.permits(&request.source) {
        return Err(ScreenError::PermissionDenied(match request.source {
            CaptureSource::Display { .. } => "display capture permission required",
            CaptureSource::Window { .. } => "window capture permission required",
            CaptureSource::Application { .. } => "application capture permission required",
        }));
    }

    if !request.source.is_previewable() {
        return Err(ScreenError::SourceUnavailable(
            "capture source is not currently visible".into(),
        ));
    }

    if !sender.features.supports(request.intent) {
        return Err(ScreenError::CapabilityMismatch(
            "sender does not support requested screen intent",
        ));
    }

    let codec = select_codec(sender, receiver, &request)?;
    let encoder = select_encoder(sender, &request)?;
    let memory = select_memory(sender, encoder, request.require_zero_copy)?;
    let (preset_resolution, preset_fps, bitrate_kbps) = request.quality.defaults();
    let resolution = preset_resolution.min_by_pixels(sender.max_resolution);
    let fps = preset_fps.min(sender.max_fps);
    if fps == 0 {
        return Err(ScreenError::CapabilityMismatch(
            "sender cannot produce frames",
        ));
    }

    Ok(ScreenStreamPlan {
        from: request.from,
        to: request.to,
        source: request.source,
        intent: request.intent,
        codec,
        encoder,
        memory,
        resolution,
        fps,
        bitrate_kbps,
        zero_copy: memory.is_gpu() && encoder.is_gpu_accelerated(),
        requires_encrypted_transport: true,
    })
}

fn select_codec(
    sender: &ScreenStreamCapabilities,
    receiver: &ScreenStreamCapabilities,
    request: &ScreenStreamRequest,
) -> Result<ScreenCodec, ScreenError> {
    let preferred = if request.preferred_codecs.is_empty() {
        &[ScreenCodec::H264, ScreenCodec::H265, ScreenCodec::RawRgba][..]
    } else {
        request.preferred_codecs.as_slice()
    };

    preferred
        .iter()
        .copied()
        .find(|codec| sender.supports_codec(*codec) && receiver.supports_codec(*codec))
        .ok_or(ScreenError::CapabilityMismatch(
            "no mutually supported screen codec",
        ))
}

fn select_encoder(
    sender: &ScreenStreamCapabilities,
    request: &ScreenStreamRequest,
) -> Result<HardwareEncoder, ScreenError> {
    let hardware_order = [
        HardwareEncoder::Nvenc,
        HardwareEncoder::Vaapi,
        HardwareEncoder::VideoToolbox,
    ];

    if request.prefer_gpu
        && let Some(encoder) = hardware_order
            .into_iter()
            .find(|encoder| sender.supports_encoder(*encoder))
    {
        return Ok(encoder);
    }

    if request.allow_software_fallback && sender.supports_encoder(HardwareEncoder::Software) {
        return Ok(HardwareEncoder::Software);
    }

    Err(ScreenError::CapabilityMismatch(
        "no supported screen encoder available",
    ))
}

fn select_memory(
    sender: &ScreenStreamCapabilities,
    encoder: HardwareEncoder,
    require_zero_copy: bool,
) -> Result<GpuMemoryKind, ScreenError> {
    if encoder.is_gpu_accelerated()
        && let Some(memory) = sender.memory_kinds.iter().copied().find(|m| m.is_gpu())
    {
        return Ok(memory);
    }

    if sender.supports_memory(GpuMemoryKind::System) && !require_zero_copy {
        return Ok(GpuMemoryKind::System);
    }

    Err(ScreenError::CapabilityMismatch(
        "zero-copy GPU capture path is unavailable",
    ))
}

/// Raw captured frame before encode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenFrame {
    /// Monotonic sequence number.
    pub sequence: u64,
    /// Capture timestamp in microseconds from sender monotonic clock.
    pub capture_time_micros: u64,
    /// Frame resolution.
    pub resolution: ScreenResolution,
    /// Pixel format.
    pub pixel_format: PixelFormat,
    /// Memory backing.
    pub memory: GpuMemoryKind,
    /// Pixel bytes or brokered handle bytes, depending on memory kind.
    pub payload: Bytes,
}

/// Frame dependency shape for decoder scheduling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FrameDependency {
    /// Independent frame.
    Key,
    /// Delta frame referencing previous frames.
    Delta,
}

/// Encoded video frame type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScreenFrameType {
    /// Intra frame.
    I,
    /// Predicted frame.
    P,
    /// Bi-directional predicted frame.
    B,
}

/// Encoded frame payload for the screen stream lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedScreenFrame {
    /// Monotonic sequence number.
    pub sequence: u64,
    /// Capture timestamp in microseconds from sender monotonic clock.
    pub capture_time_micros: u64,
    /// Frame resolution.
    pub resolution: ScreenResolution,
    /// Codec used for payload.
    pub codec: ScreenCodec,
    /// Encoder family used.
    pub encoder: HardwareEncoder,
    /// Decoder dependency.
    pub dependency: FrameDependency,
    /// Frame type.
    pub frame_type: ScreenFrameType,
    /// Encoded frame payload.
    pub payload: Bytes,
}

impl EncodedScreenFrame {
    /// Encode frame metadata + payload to stream bytes.
    ///
    /// # Errors
    /// Returns [`ScreenError::TooLarge`] if the payload exceeds `u32`.
    pub fn encode(&self) -> Result<Bytes, ScreenError> {
        let payload_len = u32::try_from(self.payload.len()).map_err(|_| ScreenError::TooLarge {
            size: self.payload.len(),
            limit: u32::MAX as usize,
        })?;
        let mut out = BytesMut::with_capacity(ENCODED_FRAME_HEADER_LEN + self.payload.len());
        out.put_u64(self.sequence);
        out.put_u64(self.capture_time_micros);
        out.put_u32(self.resolution.width);
        out.put_u32(self.resolution.height);
        out.put_u8(codec_to_u8(self.codec));
        out.put_u8(encoder_to_u8(self.encoder));
        out.put_u8(dependency_to_u8(self.dependency));
        out.put_u8(frame_type_to_u8(self.frame_type));
        out.put_u32(payload_len);
        out.put_slice(&self.payload);
        Ok(out.freeze())
    }

    /// Decode stream bytes into an encoded screen frame.
    ///
    /// # Errors
    /// Returns [`ScreenError::Codec`] on malformed input.
    pub fn decode(mut bytes: Bytes) -> Result<Self, ScreenError> {
        if bytes.remaining() < ENCODED_FRAME_HEADER_LEN {
            return Err(ScreenError::Codec("truncated screen frame".into()));
        }
        let sequence = bytes.get_u64();
        let capture_time_micros = bytes.get_u64();
        let width = bytes.get_u32();
        let height = bytes.get_u32();
        let codec = codec_from_u8(bytes.get_u8())?;
        let encoder = encoder_from_u8(bytes.get_u8())?;
        let dependency = dependency_from_u8(bytes.get_u8())?;
        let frame_type = frame_type_from_u8(bytes.get_u8())?;
        let payload_len = bytes.get_u32() as usize;
        if payload_len != bytes.remaining() {
            return Err(ScreenError::Codec("screen payload length mismatch".into()));
        }
        Ok(Self {
            sequence,
            capture_time_micros,
            resolution: ScreenResolution::new(width, height),
            codec,
            encoder,
            dependency,
            frame_type,
            payload: bytes,
        })
    }
}

/// Platform capture backend implemented by `platform-*` crates.
#[async_trait]
pub trait ScreenCaptureBackend: Send + Sync {
    /// Current screen capture capabilities for this session.
    fn capabilities(&self) -> ScreenStreamCapabilities;

    /// Request OS capture permissions. May show a system prompt or portal.
    ///
    /// # Errors
    /// Returns [`ScreenError`] when the OS cannot grant the requested access.
    async fn request_permissions(&self) -> Result<ScreenPermissions, ScreenError>;

    /// List capture sources available to this user/session.
    ///
    /// # Errors
    /// Returns [`ScreenError`] on backend enumeration failures.
    async fn list_sources(&self) -> Result<Vec<CaptureSource>, ScreenError>;

    /// Capture one frame for the negotiated plan.
    ///
    /// # Errors
    /// Returns [`ScreenError`] on capture failure or source loss.
    async fn capture_frame(&self, plan: &ScreenStreamPlan) -> Result<ScreenFrame, ScreenError>;
}

/// Encoder backend implemented by software or hardware encoder adapters.
#[async_trait]
pub trait ScreenEncoderBackend: Send + Sync {
    /// Encoder family.
    fn encoder(&self) -> HardwareEncoder;

    /// Codecs supported by this backend.
    fn codecs(&self) -> &[ScreenCodec];

    /// Encode one captured frame. Implementations must keep heavy encode work
    /// off the async runtime, using a dedicated queue/thread pool as needed.
    ///
    /// # Errors
    /// Returns [`ScreenError`] on encode failure.
    async fn encode_frame(
        &self,
        plan: &ScreenStreamPlan,
        frame: ScreenFrame,
    ) -> Result<EncodedScreenFrame, ScreenError>;
}

fn codec_to_u8(codec: ScreenCodec) -> u8 {
    match codec {
        ScreenCodec::RawRgba => 0,
        ScreenCodec::H264 => 1,
        ScreenCodec::H265 => 2,
    }
}

fn codec_from_u8(value: u8) -> Result<ScreenCodec, ScreenError> {
    match value {
        0 => Ok(ScreenCodec::RawRgba),
        1 => Ok(ScreenCodec::H264),
        2 => Ok(ScreenCodec::H265),
        _ => Err(ScreenError::Codec("unknown screen codec".into())),
    }
}

fn encoder_to_u8(encoder: HardwareEncoder) -> u8 {
    match encoder {
        HardwareEncoder::Software => 0,
        HardwareEncoder::Nvenc => 1,
        HardwareEncoder::Vaapi => 2,
        HardwareEncoder::VideoToolbox => 3,
    }
}

fn encoder_from_u8(value: u8) -> Result<HardwareEncoder, ScreenError> {
    match value {
        0 => Ok(HardwareEncoder::Software),
        1 => Ok(HardwareEncoder::Nvenc),
        2 => Ok(HardwareEncoder::Vaapi),
        3 => Ok(HardwareEncoder::VideoToolbox),
        _ => Err(ScreenError::Codec("unknown screen encoder".into())),
    }
}

fn dependency_to_u8(dependency: FrameDependency) -> u8 {
    match dependency {
        FrameDependency::Key => 0,
        FrameDependency::Delta => 1,
    }
}

fn dependency_from_u8(value: u8) -> Result<FrameDependency, ScreenError> {
    match value {
        0 => Ok(FrameDependency::Key),
        1 => Ok(FrameDependency::Delta),
        _ => Err(ScreenError::Codec("unknown frame dependency".into())),
    }
}

fn frame_type_to_u8(frame_type: ScreenFrameType) -> u8 {
    match frame_type {
        ScreenFrameType::I => 0,
        ScreenFrameType::P => 1,
        ScreenFrameType::B => 2,
    }
}

fn frame_type_from_u8(value: u8) -> Result<ScreenFrameType, ScreenError> {
    match value {
        0 => Ok(ScreenFrameType::I),
        1 => Ok(ScreenFrameType::P),
        2 => Ok(ScreenFrameType::B),
        _ => Err(ScreenError::Codec("unknown screen frame type".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display_source() -> CaptureSource {
        CaptureSource::Display {
            id: CaptureSourceId::new("display-1"),
            label: "Display 1".into(),
        }
    }

    fn devices() -> (DeviceId, DeviceId) {
        (DeviceId::generate(), DeviceId::generate())
    }

    #[test]
    fn mini_preview_negotiates_software_h264() {
        let (from, to) = devices();
        let sender = ScreenStreamCapabilities::software_h264();
        let receiver = ScreenStreamCapabilities::software_h264();
        let plan = negotiate_screen_stream(
            &sender,
            &receiver,
            ScreenStreamRequest::mini_preview(from, to, display_source()),
        )
        .unwrap();

        assert_eq!(plan.codec, ScreenCodec::H264);
        assert_eq!(plan.encoder, HardwareEncoder::Software);
        assert_eq!(plan.resolution, ScreenResolution::preview());
        assert_eq!(plan.fps, 15);
        assert!(plan.requires_encrypted_transport);
    }

    #[test]
    fn denied_permission_blocks_capture_plan() {
        let (from, to) = devices();
        let mut sender = ScreenStreamCapabilities::software_h264();
        sender.permissions.display_capture = false;
        let receiver = ScreenStreamCapabilities::software_h264();

        let err = negotiate_screen_stream(
            &sender,
            &receiver,
            ScreenStreamRequest::interactive(from, to, display_source()),
        )
        .unwrap_err();

        assert!(matches!(err, ScreenError::PermissionDenied(_)));
    }

    #[test]
    fn hidden_window_is_not_previewable() {
        let (from, to) = devices();
        let source = CaptureSource::Window {
            id: CaptureSourceId::new("window-1"),
            title: "Private".into(),
            app_id: Some("com.example.private".into()),
            visibility: WindowVisibility::Hidden,
        };

        let err = negotiate_screen_stream(
            &ScreenStreamCapabilities::software_h264(),
            &ScreenStreamCapabilities::software_h264(),
            ScreenStreamRequest {
                from,
                to,
                source,
                intent: ScreenStreamIntent::WindowPeek,
                quality: ScreenQualityPreset::Preview,
                preferred_codecs: vec![ScreenCodec::H264],
                prefer_gpu: true,
                allow_software_fallback: true,
                require_zero_copy: false,
            },
        )
        .unwrap_err();

        assert!(matches!(err, ScreenError::SourceUnavailable(_)));
    }

    #[test]
    fn zero_copy_requirement_rejects_system_memory() {
        let (from, to) = devices();
        let sender = ScreenStreamCapabilities::software_h264();
        let receiver = ScreenStreamCapabilities::software_h264();

        let err = negotiate_screen_stream(
            &sender,
            &receiver,
            ScreenStreamRequest {
                from,
                to,
                source: display_source(),
                intent: ScreenStreamIntent::InteractiveRemote,
                quality: ScreenQualityPreset::Interactive,
                preferred_codecs: vec![ScreenCodec::H264],
                prefer_gpu: true,
                allow_software_fallback: true,
                require_zero_copy: true,
            },
        )
        .unwrap_err();

        assert!(matches!(err, ScreenError::CapabilityMismatch(_)));
    }

    #[test]
    fn encoded_frame_round_trips() {
        let frame = EncodedScreenFrame {
            sequence: 7,
            capture_time_micros: 42,
            resolution: ScreenResolution::preview(),
            codec: ScreenCodec::H264,
            encoder: HardwareEncoder::Software,
            dependency: FrameDependency::Key,
            frame_type: ScreenFrameType::I,
            payload: Bytes::from_static(b"frame"),
        };

        let decoded = EncodedScreenFrame::decode(frame.encode().unwrap()).unwrap();
        assert_eq!(decoded, frame);
    }
}
