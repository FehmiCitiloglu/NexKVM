//! Linux PipeWire screen capture through the ScreenCast portal.

use async_trait::async_trait;
use bytes::Bytes;
use nexkvm_streaming::{
    CaptureSource, CaptureSourceId, GpuMemoryKind, HardwareEncoder, PixelFormat,
    ScreenCaptureBackend, ScreenCodec, ScreenError, ScreenFeatureSet, ScreenFrame,
    ScreenPermissions, ScreenResolution, ScreenStreamCapabilities, ScreenStreamPlan,
};
use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use tokio::sync::Mutex;
use zbus::export::futures_util::StreamExt;
use zbus::{Connection, Proxy};
use zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

const PORTAL_DESTINATION: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const SCREENCAST_INTERFACE: &str = "org.freedesktop.portal.ScreenCast";
const REQUEST_INTERFACE: &str = "org.freedesktop.portal.Request";
const MONITOR_SOURCE: u32 = 1;
const WINDOW_SOURCE: u32 = 2;
const EMBEDDED_CURSOR_MODE: u32 = 2;

/// PipeWire remote fd opened by the ScreenCast portal.
#[derive(Debug)]
pub struct PipeWireRemoteFd {
    /// File descriptor for the portal-scoped PipeWire remote.
    pub fd: OwnedFd,
}

/// One stream returned by `ScreenCast.Start`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipeWireScreenCastStream {
    /// Compatibility PipeWire node id returned by the portal.
    pub node_id: u32,
    /// Optional stable portal stream id.
    pub id: Option<String>,
    /// Optional compositor-space position.
    pub position: Option<(i32, i32)>,
    /// Optional compositor-space size.
    pub size: Option<(i32, i32)>,
    /// ScreenCast source type bit.
    pub source_type: u32,
    /// Optional mapping id.
    pub mapping_id: Option<String>,
    /// Preferred PipeWire object serial for stream targeting.
    pub pipewire_serial: Option<u64>,
}

/// Active ScreenCast portal session with PipeWire stream metadata.
#[derive(Debug, Clone)]
pub struct PipeWireScreenCastSession {
    /// Portal session object path.
    pub session_handle: String,
    /// Streams selected by the portal.
    pub streams: Vec<PipeWireScreenCastStream>,
    /// Portal-scoped PipeWire remote fd, if opened by the transport.
    pub remote: Option<Arc<PipeWireRemoteFd>>,
}

/// PipeWire stream target selected from portal metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipeWireStreamTarget {
    /// Preferred target using PipeWire object serial from the portal.
    ObjectSerial(u64),
    /// Fallback target using the compatibility node id.
    NodeId(u32),
    /// Fallback target using portal stream id.
    PortalStreamId(String),
}

impl PipeWireStreamTarget {
    /// Value to place in PipeWire `target.object`.
    #[must_use]
    pub fn target_object(&self) -> String {
        match self {
            Self::ObjectSerial(serial) => serial.to_string(),
            Self::NodeId(node_id) => node_id.to_string(),
            Self::PortalStreamId(id) => id.clone(),
        }
    }
}

/// Frame request passed to the PipeWire reader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipeWireFrameRequest {
    /// PipeWire stream target.
    pub target: PipeWireStreamTarget,
    /// Requested output resolution.
    pub resolution: ScreenResolution,
    /// Requested memory backing.
    pub memory: GpuMemoryKind,
}

/// Raw frame returned by a PipeWire reader before platform-neutral validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipeWireRawFrame {
    /// Monotonic sequence number.
    pub sequence: u64,
    /// Capture timestamp in microseconds.
    pub capture_time_micros: u64,
    /// Frame resolution.
    pub resolution: ScreenResolution,
    /// Pixel format.
    pub pixel_format: PixelFormat,
    /// Memory backing.
    pub memory: GpuMemoryKind,
    /// Pixel bytes or brokered handle payload.
    pub payload: Bytes,
}

/// Negotiated PipeWire frame format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PipeWireFrameFormat {
    /// Frame resolution.
    pub resolution: ScreenResolution,
    /// PipeWire raw video format.
    pub video_format: PipeWireVideoFormat,
    /// Pixel format.
    pub pixel_format: PixelFormat,
    /// Bytes per line for mapped system-memory planes.
    pub stride: u32,
}

/// SPA raw video format ids consumed from `spa_video_info_raw`.
pub const SPA_VIDEO_FORMAT_RGBX: u32 = 5;
/// SPA raw video BGRx id.
pub const SPA_VIDEO_FORMAT_BGRX: u32 = 6;
/// SPA raw video RGBA id.
pub const SPA_VIDEO_FORMAT_RGBA: u32 = 9;
/// SPA raw video BGRA id.
pub const SPA_VIDEO_FORMAT_BGRA: u32 = 10;
/// SPA raw video NV12 id.
pub const SPA_VIDEO_FORMAT_NV12: u32 = 23;

#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
const SPA_TYPE_ID: u32 = 3;
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
const SPA_TYPE_INT: u32 = 4;
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
const SPA_TYPE_RECTANGLE: u32 = 10;
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
const SPA_TYPE_OBJECT: u32 = 15;
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
const SPA_TYPE_CHOICE: u32 = 19;
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
const SPA_TYPE_OBJECT_FORMAT: u32 = 0x40003;
pub const SPA_PARAM_FORMAT: u32 = 4;
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
const SPA_MEDIA_TYPE_VIDEO: u32 = 2;
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
const SPA_MEDIA_SUBTYPE_RAW: u32 = 1;
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
const SPA_FORMAT_MEDIA_TYPE: u32 = 1;
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
const SPA_FORMAT_MEDIA_SUBTYPE: u32 = 2;
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
const SPA_FORMAT_VIDEO_FORMAT: u32 = 0x20001;
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
const SPA_FORMAT_VIDEO_SIZE: u32 = 0x20003;

/// Parsed subset of `spa_video_info_raw` used by NexKVM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PipeWireSpaRawVideoInfo {
    /// Raw `spa_video_format` id.
    pub spa_format: u32,
    /// Width from `spa_video_info_raw.size.width`.
    pub width: u32,
    /// Height from `spa_video_info_raw.size.height`.
    pub height: u32,
    /// Optional line stride from parsed pod metadata.
    pub stride: Option<u32>,
}

impl PipeWireFrameFormat {
    /// Select the best supported raw format for a requested resolution.
    ///
    /// # Errors
    /// Returns [`ScreenError`] if none of the negotiated formats can be mapped
    /// into NexKVM's shared frame model.
    pub fn fixate(
        resolution: ScreenResolution,
        candidates: &[PipeWireVideoFormat],
    ) -> Result<Self, ScreenError> {
        let video_format = candidates
            .iter()
            .copied()
            .find(|format| {
                matches!(
                    format,
                    PipeWireVideoFormat::Bgra | PipeWireVideoFormat::Rgba
                )
            })
            .or_else(|| {
                candidates
                    .iter()
                    .copied()
                    .find(|format| *format == PipeWireVideoFormat::Nv12)
            })
            .ok_or(ScreenError::CapabilityMismatch(
                "no supported PipeWire raw video format",
            ))?;
        let pixel_format = video_format.pixel_format();
        let stride = video_format.stride_for_width(resolution.width)?;
        Ok(Self {
            resolution,
            video_format,
            pixel_format,
            stride,
        })
    }

    /// Convert parsed PipeWire SPA raw video info into a NexKVM frame format.
    ///
    /// # Errors
    /// Returns [`ScreenError`] when the SPA video format id is unsupported.
    pub fn from_spa_raw_info(info: PipeWireSpaRawVideoInfo) -> Result<Self, ScreenError> {
        let video_format = PipeWireVideoFormat::from_spa_format(info.spa_format).ok_or(
            ScreenError::CapabilityMismatch("unsupported PipeWire SPA raw video format"),
        )?;
        let resolution = ScreenResolution::new(info.width, info.height);
        let pixel_format = video_format.pixel_format();
        let stride = match info.stride {
            Some(stride) if stride > 0 => stride,
            _ => video_format.stride_for_width(info.width)?,
        };
        Ok(Self {
            resolution,
            video_format,
            pixel_format,
            stride,
        })
    }

    /// Expected mapped system-memory payload length for this format.
    #[must_use]
    pub fn system_payload_len(self) -> Option<usize> {
        let height = usize::try_from(self.resolution.height).ok()?;
        usize::try_from(self.stride).ok()?.checked_mul(height)
    }
}

/// PipeWire raw video formats NexKVM can consume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipeWireVideoFormat {
    /// PipeWire BGRA/BGRx style 32-bit pixels.
    Bgra,
    /// PipeWire RGBA/RGBx style 32-bit pixels.
    Rgba,
    /// NV12 YUV 4:2:0.
    Nv12,
}

impl PipeWireVideoFormat {
    fn from_spa_format(format: u32) -> Option<Self> {
        match format {
            SPA_VIDEO_FORMAT_BGRA | SPA_VIDEO_FORMAT_BGRX => Some(Self::Bgra),
            SPA_VIDEO_FORMAT_RGBA | SPA_VIDEO_FORMAT_RGBX => Some(Self::Rgba),
            SPA_VIDEO_FORMAT_NV12 => Some(Self::Nv12),
            _ => None,
        }
    }

    fn pixel_format(self) -> PixelFormat {
        match self {
            Self::Bgra => PixelFormat::Bgra8,
            Self::Rgba => PixelFormat::Rgba8,
            Self::Nv12 => PixelFormat::Nv12,
        }
    }

    fn stride_for_width(self, width: u32) -> Result<u32, ScreenError> {
        match self {
            Self::Bgra | Self::Rgba => width
                .checked_mul(4)
                .ok_or_else(|| ScreenError::Codec("PipeWire stride overflow".into())),
            Self::Nv12 => Ok(width),
        }
    }
}

/// Mapped PipeWire buffer plane.
#[derive(Debug, Clone, Copy)]
pub struct PipeWireMappedBuffer<'a> {
    /// Mapped bytes for the first video plane.
    pub bytes: &'a [u8],
    /// Valid data offset from the SPA chunk.
    pub chunk_offset: usize,
    /// Valid data size from the SPA chunk.
    pub chunk_size: usize,
    /// Optional DMA-BUF fd for non-mapped GPU-backed buffers.
    pub fd: Option<i64>,
}

impl PipeWireRawFrame {
    /// Validate and convert the raw PipeWire frame into the shared streaming type.
    ///
    /// # Errors
    /// Returns [`ScreenError`] if the payload is inconsistent with the advertised
    /// frame format.
    pub fn into_screen_frame(self) -> Result<ScreenFrame, ScreenError> {
        validate_pipewire_payload(
            self.resolution,
            self.pixel_format,
            self.memory,
            self.payload.len(),
        )?;
        Ok(ScreenFrame {
            sequence: self.sequence,
            capture_time_micros: self.capture_time_micros,
            resolution: self.resolution,
            pixel_format: self.pixel_format,
            memory: self.memory,
            payload: self.payload,
        })
    }
}

/// PipeWire frame reader.
#[async_trait]
pub trait PipeWireFrameReader: Send + Sync {
    /// Return the next frame for the selected portal stream.
    async fn next_frame(
        &self,
        remote: Arc<PipeWireRemoteFd>,
        request: PipeWireFrameRequest,
    ) -> Result<PipeWireRawFrame, ScreenError>;
}

/// Default reader used until a native Linux PipeWire stream pump is installed.
#[derive(Debug, Clone, Copy, Default)]
pub struct PendingPipeWireFrameReader;

#[async_trait]
impl PipeWireFrameReader for PendingPipeWireFrameReader {
    async fn next_frame(
        &self,
        _remote: Arc<PipeWireRemoteFd>,
        _request: PipeWireFrameRequest,
    ) -> Result<PipeWireRawFrame, ScreenError> {
        Err(ScreenError::Backend(
            "PipeWire frame decoding is not wired yet".into(),
        ))
    }
}

/// Native PipeWire reader entry point.
///
/// On Linux this opens the portal-scoped PipeWire remote with
/// `pw_context_connect_fd` before handing off to the stream pump. The stream
/// process/dequeue loop is still implemented behind the [`PipeWireFrameReader`]
/// boundary so tests can validate frame conversion without a live compositor.
#[derive(Debug, Clone, Copy, Default)]
pub struct NativePipeWireFrameReader;

#[async_trait]
impl PipeWireFrameReader for NativePipeWireFrameReader {
    async fn next_frame(
        &self,
        remote: Arc<PipeWireRemoteFd>,
        request: PipeWireFrameRequest,
    ) -> Result<PipeWireRawFrame, ScreenError> {
        native_pipewire_next_frame(remote, request).await
    }
}

/// ScreenCast portal transport.
#[async_trait]
pub trait XdgDesktopPortalScreenCastTransport: Send + Sync {
    /// Open a ScreenCast session and PipeWire remote.
    async fn open_screen_cast(&self) -> Result<PipeWireScreenCastSession, ScreenError>;
}

/// zbus-backed xdg-desktop-portal ScreenCast transport.
#[derive(Debug, Clone)]
pub struct ZbusXdgDesktopPortalScreenCastTransport {
    connection: Connection,
}

impl ZbusXdgDesktopPortalScreenCastTransport {
    /// Connect to the user session bus.
    ///
    /// # Errors
    /// Returns [`ScreenError`] if the D-Bus session bus is unavailable.
    pub async fn session() -> Result<Self, ScreenError> {
        let connection = Connection::session()
            .await
            .map_err(|error| ScreenError::Backend(format!("connect session bus: {error}")))?;
        Ok(Self { connection })
    }

    async fn proxy(&self) -> Result<Proxy<'_>, ScreenError> {
        Proxy::new(
            &self.connection,
            PORTAL_DESTINATION,
            PORTAL_PATH,
            SCREENCAST_INTERFACE,
        )
        .await
        .map_err(|error| ScreenError::Backend(format!("create ScreenCast proxy: {error}")))
    }

    async fn request_results(
        &self,
        handle: OwnedObjectPath,
        operation: &'static str,
    ) -> Result<HashMap<String, OwnedValue>, ScreenError> {
        let proxy = Proxy::new(
            &self.connection,
            PORTAL_DESTINATION,
            handle.as_str(),
            REQUEST_INTERFACE,
        )
        .await
        .map_err(|error| ScreenError::Backend(format!("{operation} request proxy: {error}")))?;
        let mut responses = proxy.receive_signal("Response").await.map_err(|error| {
            ScreenError::Backend(format!("{operation} response stream: {error}"))
        })?;
        let message = responses
            .next()
            .await
            .ok_or_else(|| ScreenError::Backend(format!("{operation} response stream ended")))?;
        let (code, results): (u32, HashMap<String, OwnedValue>) =
            message.body().deserialize().map_err(|error| {
                ScreenError::Backend(format!("{operation} response decode: {error}"))
            })?;
        if code == 0 {
            Ok(results)
        } else {
            Err(ScreenError::Backend(format!(
                "{operation} failed with portal response code {code}"
            )))
        }
    }

    fn session_handle(token: &str) -> String {
        format!("/org/freedesktop/portal/desktop/session/nexkvm/{token}")
    }
}

#[async_trait]
impl XdgDesktopPortalScreenCastTransport for ZbusXdgDesktopPortalScreenCastTransport {
    async fn open_screen_cast(&self) -> Result<PipeWireScreenCastSession, ScreenError> {
        let proxy = self.proxy().await?;
        let token = portal_token("screencast");
        let expected_session_handle = Self::session_handle(&token);
        let mut options = portal_options();
        options.insert("session_handle_token", Value::from(token.as_str()));
        let handle: OwnedObjectPath = proxy
            .call("CreateSession", &(options))
            .await
            .map_err(screen_portal_call_error("ScreenCast.CreateSession"))?;
        let mut results = self
            .request_results(handle, "ScreenCast.CreateSession")
            .await?;
        let session_handle = optional_portal_result(&mut results, "session_handle")?
            .unwrap_or(expected_session_handle);
        let session_path = ObjectPath::try_from(session_handle.as_str())
            .map_err(|error| ScreenError::Backend(format!("screencast session path: {error}")))?;

        let mut options = portal_options();
        options.insert("types", Value::from(MONITOR_SOURCE | WINDOW_SOURCE));
        options.insert("multiple", Value::from(false));
        options.insert("cursor_mode", Value::from(EMBEDDED_CURSOR_MODE));
        let handle: OwnedObjectPath = proxy
            .call("SelectSources", &(&session_path, options))
            .await
            .map_err(screen_portal_call_error("ScreenCast.SelectSources"))?;
        let _ = self
            .request_results(handle, "ScreenCast.SelectSources")
            .await?;

        let options = portal_options();
        let handle: OwnedObjectPath = proxy
            .call("Start", &(&session_path, "", options))
            .await
            .map_err(screen_portal_call_error("ScreenCast.Start"))?;
        let mut results = self.request_results(handle, "ScreenCast.Start").await?;
        let raw_streams: Vec<(u32, HashMap<String, OwnedValue>)> =
            take_portal_result(&mut results, "streams")?;
        let streams = raw_streams
            .into_iter()
            .map(|(node_id, mut props)| parse_stream(node_id, &mut props))
            .collect::<Result<Vec<_>, _>>()?;

        let options = portal_options();
        let fd: zvariant::OwnedFd = proxy
            .call("OpenPipeWireRemote", &(&session_path, options))
            .await
            .map_err(screen_portal_call_error("ScreenCast.OpenPipeWireRemote"))?;

        Ok(PipeWireScreenCastSession {
            session_handle,
            streams,
            remote: Some(Arc::new(PipeWireRemoteFd {
                fd: OwnedFd::from(fd),
            })),
        })
    }
}

/// Linux ScreenCaptureBackend backed by xdg-desktop-portal ScreenCast.
#[derive(Debug)]
pub struct LinuxPipeWireScreenCapture<T, R = PendingPipeWireFrameReader> {
    transport: T,
    frame_reader: R,
    session: Mutex<Option<PipeWireScreenCastSession>>,
}

impl<T> LinuxPipeWireScreenCapture<T, PendingPipeWireFrameReader>
where
    T: XdgDesktopPortalScreenCastTransport,
{
    /// Create a PipeWire screen capture backend over `transport`.
    #[must_use]
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            frame_reader: PendingPipeWireFrameReader,
            session: Mutex::new(None),
        }
    }
}

impl<T, R> LinuxPipeWireScreenCapture<T, R>
where
    T: XdgDesktopPortalScreenCastTransport,
    R: PipeWireFrameReader,
{
    /// Create a PipeWire screen capture backend with an explicit frame reader.
    #[must_use]
    pub fn with_frame_reader(transport: T, frame_reader: R) -> Self {
        Self {
            transport,
            frame_reader,
            session: Mutex::new(None),
        }
    }

    /// Borrow the transport for tests/diagnostics.
    #[must_use]
    pub const fn transport(&self) -> &T {
        &self.transport
    }

    /// Borrow the frame reader for tests/diagnostics.
    #[must_use]
    pub const fn frame_reader(&self) -> &R {
        &self.frame_reader
    }

    async fn ensure_session(&self) -> Result<PipeWireScreenCastSession, ScreenError> {
        let mut session = self.session.lock().await;
        if session.is_none() {
            *session = Some(self.transport.open_screen_cast().await?);
        }
        session
            .clone()
            .ok_or_else(|| ScreenError::Backend("ScreenCast session was not initialized".into()))
    }
}

#[async_trait]
impl<T, R> ScreenCaptureBackend for LinuxPipeWireScreenCapture<T, R>
where
    T: XdgDesktopPortalScreenCastTransport,
    R: PipeWireFrameReader,
{
    fn capabilities(&self) -> ScreenStreamCapabilities {
        ScreenStreamCapabilities {
            permissions: ScreenPermissions {
                display_capture: true,
                window_capture: true,
                app_capture: false,
                permission_pending: true,
            },
            memory_kinds: vec![GpuMemoryKind::DmaBuf, GpuMemoryKind::System],
            codecs: vec![ScreenCodec::RawRgba, ScreenCodec::H264],
            encoders: vec![HardwareEncoder::Software],
            max_resolution: ScreenResolution::new(3840, 2160),
            max_fps: 60,
            features: ScreenFeatureSet {
                mini_remote_preview: true,
                window_peeking: true,
                instant_app_preview: false,
            },
        }
    }

    async fn request_permissions(&self) -> Result<ScreenPermissions, ScreenError> {
        let _ = self.ensure_session().await?;
        Ok(ScreenPermissions {
            display_capture: true,
            window_capture: true,
            app_capture: false,
            permission_pending: false,
        })
    }

    async fn list_sources(&self) -> Result<Vec<CaptureSource>, ScreenError> {
        let session = self.ensure_session().await?;
        Ok(session.streams.iter().map(stream_source).collect())
    }

    async fn capture_frame(&self, plan: &ScreenStreamPlan) -> Result<ScreenFrame, ScreenError> {
        let session = self.ensure_session().await?;
        let remote = session
            .remote
            .clone()
            .ok_or_else(|| ScreenError::Backend("PipeWire remote fd is not open".into()))?;
        let stream = select_stream_for_source(&session.streams, &plan.source)?;
        let request = PipeWireFrameRequest {
            target: stream_target(stream),
            resolution: plan.resolution,
            memory: plan.memory,
        };
        self.frame_reader
            .next_frame(remote, request)
            .await?
            .into_screen_frame()
    }
}

fn stream_source(stream: &PipeWireScreenCastStream) -> CaptureSource {
    let id = stream
        .pipewire_serial
        .map(|serial| format!("pipewire-serial:{serial}"))
        .or_else(|| stream.id.as_ref().map(|id| format!("portal-stream:{id}")))
        .unwrap_or_else(|| format!("pipewire-node:{}", stream.node_id));
    let label_id = stream
        .id
        .as_deref()
        .map_or_else(|| stream.node_id.to_string(), str::to_string);
    let size = stream
        .size
        .map(|(width, height)| format!("{width}x{height}"))
        .unwrap_or_else(|| "unknown-size".into());
    let position = stream
        .position
        .map(|(x, y)| format!(" at {x},{y}"))
        .unwrap_or_default();

    CaptureSource::Display {
        id: CaptureSourceId::new(id),
        label: format!("PipeWire monitor {label_id} ({size}{position})"),
    }
}

fn parse_stream(
    node_id: u32,
    props: &mut HashMap<String, OwnedValue>,
) -> Result<PipeWireScreenCastStream, ScreenError> {
    Ok(PipeWireScreenCastStream {
        node_id,
        id: optional_portal_result(props, "id")?,
        position: optional_portal_result(props, "position")?,
        size: optional_portal_result(props, "size")?,
        source_type: optional_portal_result(props, "source_type")?.unwrap_or(MONITOR_SOURCE),
        mapping_id: optional_portal_result(props, "mapping_id")?,
        pipewire_serial: optional_portal_result(props, "pipewire-serial")?,
    })
}

fn select_stream_for_source<'a>(
    streams: &'a [PipeWireScreenCastStream],
    source: &CaptureSource,
) -> Result<&'a PipeWireScreenCastStream, ScreenError> {
    let source_id = capture_source_id(source);
    streams
        .iter()
        .find(|stream| stream_matches_source_id(stream, source_id))
        .or_else(|| streams.first())
        .ok_or_else(|| ScreenError::SourceUnavailable("PipeWire portal returned no streams".into()))
}

fn capture_source_id(source: &CaptureSource) -> &str {
    match source {
        CaptureSource::Display { id, .. }
        | CaptureSource::Window { id, .. }
        | CaptureSource::Application { id, .. } => id.0.as_str(),
    }
}

fn stream_matches_source_id(stream: &PipeWireScreenCastStream, source_id: &str) -> bool {
    if let Some(serial) = stream.pipewire_serial
        && source_id == format!("pipewire-serial:{serial}")
    {
        return true;
    }
    if let Some(id) = &stream.id
        && source_id == format!("portal-stream:{id}")
    {
        return true;
    }
    source_id == format!("pipewire-node:{}", stream.node_id)
}

fn stream_target(stream: &PipeWireScreenCastStream) -> PipeWireStreamTarget {
    stream
        .pipewire_serial
        .map(PipeWireStreamTarget::ObjectSerial)
        .or_else(|| stream.id.clone().map(PipeWireStreamTarget::PortalStreamId))
        .unwrap_or(PipeWireStreamTarget::NodeId(stream.node_id))
}

fn validate_pipewire_payload(
    resolution: ScreenResolution,
    pixel_format: PixelFormat,
    memory: GpuMemoryKind,
    payload_len: usize,
) -> Result<(), ScreenError> {
    if memory == GpuMemoryKind::DmaBuf {
        if payload_len == 0 {
            return Err(ScreenError::Codec("empty PipeWire DMA-BUF payload".into()));
        }
        return Ok(());
    }

    let bytes_per_pixel = match pixel_format {
        PixelFormat::Rgba8 | PixelFormat::Bgra8 => 4,
        PixelFormat::Nv12 => 0,
    };
    let expected_len = if pixel_format == PixelFormat::Nv12 {
        usize::try_from(resolution.width)
            .ok()
            .and_then(|width| {
                usize::try_from(resolution.height)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .and_then(|pixels| pixels.checked_mul(3))
            .map(|bytes| bytes / 2)
    } else {
        usize::try_from(resolution.width)
            .ok()
            .and_then(|width| {
                usize::try_from(resolution.height)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .and_then(|pixels| pixels.checked_mul(bytes_per_pixel))
    }
    .ok_or_else(|| ScreenError::Codec("PipeWire frame dimensions overflow".into()))?;

    if payload_len < expected_len {
        return Err(ScreenError::Codec(format!(
            "PipeWire payload too short: {payload_len} bytes for {expected_len} byte frame"
        )));
    }
    Ok(())
}

#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
fn parse_spa_format_pod_bytes(bytes: &[u8]) -> Option<PipeWireSpaRawVideoInfo> {
    let pod = SpaPodView::new(bytes)?;
    if pod.type_ != SPA_TYPE_OBJECT || pod.body.len() < 8 {
        return None;
    }
    let object_type = read_u32(pod.body, 0)?;
    let object_id = read_u32(pod.body, 4)?;
    if object_type != SPA_TYPE_OBJECT_FORMAT || object_id != SPA_PARAM_FORMAT {
        return None;
    }

    let mut offset = 8usize;
    let mut media_type = None;
    let mut media_subtype = None;
    let mut spa_format = None;
    let mut size = None;

    while offset.checked_add(16)? <= pod.body.len() {
        let key = read_u32(pod.body, offset)?;
        let value_size = usize::try_from(read_u32(pod.body, offset + 8)?).ok()?;
        let value_type = read_u32(pod.body, offset + 12)?;
        let value_start = offset + 16;
        let value_end = value_start.checked_add(value_size)?;
        if value_end > pod.body.len() {
            return None;
        }
        let value = &pod.body[(offset + 8)..value_end];

        match key {
            SPA_FORMAT_MEDIA_TYPE => media_type = parse_spa_u32_value(value_type, value),
            SPA_FORMAT_MEDIA_SUBTYPE => media_subtype = parse_spa_u32_value(value_type, value),
            SPA_FORMAT_VIDEO_FORMAT => spa_format = parse_spa_u32_value(value_type, value),
            SPA_FORMAT_VIDEO_SIZE => size = parse_spa_rectangle_value(value_type, value),
            _ => {}
        }

        offset = value_start.checked_add(round_up_8(value_size))?;
    }

    if media_type != Some(SPA_MEDIA_TYPE_VIDEO) || media_subtype != Some(SPA_MEDIA_SUBTYPE_RAW) {
        return None;
    }
    let (width, height) = size?;
    Some(PipeWireSpaRawVideoInfo {
        spa_format: spa_format?,
        width,
        height,
        stride: None,
    })
}

#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
fn parse_spa_u32_value(type_: u32, value: &[u8]) -> Option<u32> {
    match type_ {
        SPA_TYPE_ID | SPA_TYPE_INT => read_u32(value, 8),
        SPA_TYPE_CHOICE => parse_spa_choice_first_u32(value),
        _ => None,
    }
}

#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
fn parse_spa_rectangle_value(type_: u32, value: &[u8]) -> Option<(u32, u32)> {
    match type_ {
        SPA_TYPE_RECTANGLE => Some((read_u32(value, 8)?, read_u32(value, 12)?)),
        SPA_TYPE_CHOICE => parse_spa_choice_first_rectangle(value),
        _ => None,
    }
}

#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
fn parse_spa_choice_first_u32(value: &[u8]) -> Option<u32> {
    if value.len() < 24 {
        return None;
    }
    let child_size = usize::try_from(read_u32(value, 16)?).ok()?;
    let child_type = read_u32(value, 20)?;
    if child_size < 4 || !matches!(child_type, SPA_TYPE_ID | SPA_TYPE_INT) {
        return None;
    }
    read_u32(value, 24)
}

#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
fn parse_spa_choice_first_rectangle(value: &[u8]) -> Option<(u32, u32)> {
    if value.len() < 32 {
        return None;
    }
    let child_size = usize::try_from(read_u32(value, 16)?).ok()?;
    let child_type = read_u32(value, 20)?;
    if child_size < 8 || child_type != SPA_TYPE_RECTANGLE {
        return None;
    }
    Some((read_u32(value, 24)?, read_u32(value, 28)?))
}

#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
struct SpaPodView<'a> {
    type_: u32,
    body: &'a [u8],
}

#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
impl<'a> SpaPodView<'a> {
    fn new(bytes: &'a [u8]) -> Option<Self> {
        if bytes.len() < 8 {
            return None;
        }
        let size = usize::try_from(read_u32(bytes, 0)?).ok()?;
        let type_ = read_u32(bytes, 4)?;
        let end = 8usize.checked_add(size)?;
        if end > bytes.len() {
            return None;
        }
        Some(Self {
            type_,
            body: &bytes[8..end],
        })
    }
}

#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    let chunk = bytes.get(offset..end)?;
    Some(u32::from_le_bytes(chunk.try_into().ok()?))
}

#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
fn round_up_8(value: usize) -> usize {
    (value + 7) & !7
}

#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
fn pipewire_frame_from_mapped_buffer(
    buffer: PipeWireMappedBuffer<'_>,
    request: PipeWireFrameRequest,
    format: PipeWireFrameFormat,
    sequence: u64,
    capture_time_micros: u64,
) -> Result<PipeWireRawFrame, ScreenError> {
    if request.memory == GpuMemoryKind::DmaBuf {
        let fd = buffer.fd.ok_or_else(|| {
            ScreenError::Backend("PipeWire DMA-BUF buffer did not include an fd".into())
        })?;
        let mut payload = Vec::with_capacity(24);
        payload.extend_from_slice(&fd.to_le_bytes());
        payload.extend_from_slice(&(buffer.chunk_offset as u64).to_le_bytes());
        payload.extend_from_slice(&(buffer.chunk_size as u64).to_le_bytes());
        return Ok(PipeWireRawFrame {
            sequence,
            capture_time_micros,
            resolution: format.resolution,
            pixel_format: format.pixel_format,
            memory: GpuMemoryKind::DmaBuf,
            payload: Bytes::from(payload),
        });
    }

    let end = buffer
        .chunk_offset
        .checked_add(buffer.chunk_size)
        .ok_or_else(|| ScreenError::Codec("PipeWire chunk range overflow".into()))?;
    let bytes = buffer
        .bytes
        .get(buffer.chunk_offset..end)
        .ok_or_else(|| ScreenError::Codec("PipeWire chunk range is outside mapped data".into()))?;
    Ok(PipeWireRawFrame {
        sequence,
        capture_time_micros,
        resolution: format.resolution,
        pixel_format: format.pixel_format,
        memory: GpuMemoryKind::System,
        payload: Bytes::copy_from_slice(bytes),
    })
}

#[cfg(target_os = "linux")]
async fn native_pipewire_next_frame(
    remote: Arc<PipeWireRemoteFd>,
    request: PipeWireFrameRequest,
) -> Result<PipeWireRawFrame, ScreenError> {
    tokio::task::spawn_blocking(move || {
        native_pipewire::connect_remote_and_read_frame(remote, request)
    })
    .await
    .map_err(|error| ScreenError::Backend(format!("PipeWire reader task join: {error}")))?
}

#[cfg(not(target_os = "linux"))]
async fn native_pipewire_next_frame(
    _remote: Arc<PipeWireRemoteFd>,
    _request: PipeWireFrameRequest,
) -> Result<PipeWireRawFrame, ScreenError> {
    Err(ScreenError::Backend(
        "native PipeWire frame reader requires Linux".into(),
    ))
}

#[cfg(target_os = "linux")]
mod native_pipewire {
    use super::{
        PipeWireFrameFormat, PipeWireFrameRequest, PipeWireMappedBuffer, PipeWireRawFrame,
        PipeWireRemoteFd, PipeWireVideoFormat, SPA_PARAM_FORMAT, ScreenError,
        parse_spa_format_pod_bytes, pipewire_frame_from_mapped_buffer,
    };
    use std::ffi::CString;
    use std::ffi::{c_char, c_int, c_void};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::ptr;
    use std::sync::Arc;
    use std::sync::mpsc::{SyncSender, TryRecvError, sync_channel};
    use std::time::{Duration, Instant};

    const PW_DIRECTION_INPUT: u32 = 0;
    const PW_ID_ANY: u32 = u32::MAX;
    const PW_STREAM_FLAG_AUTOCONNECT: u32 = 1 << 0;
    const PW_STREAM_FLAG_MAP_BUFFERS: u32 = 1 << 2;
    const STREAM_WAIT_TIMEOUT: Duration = Duration::from_secs(5);

    #[repr(C)]
    struct SpaDict {
        _private: [u8; 0],
    }

    #[repr(C)]
    struct PwMainLoop {
        _private: [u8; 0],
    }

    #[repr(C)]
    struct PwLoop {
        _private: [u8; 0],
    }

    #[repr(C)]
    struct PwContext {
        _private: [u8; 0],
    }

    #[repr(C)]
    struct PwCore {
        _private: [u8; 0],
    }

    #[repr(C)]
    struct PwProperties {
        _private: [u8; 0],
    }

    #[repr(C)]
    struct PwStream {
        _private: [u8; 0],
    }

    #[repr(C)]
    struct SpaPod {
        size: u32,
        type_: u32,
    }

    #[repr(C)]
    struct SpaList {
        next: *mut SpaList,
        prev: *mut SpaList,
    }

    #[repr(C)]
    struct SpaCallbacks {
        funcs: *const c_void,
        data: *mut c_void,
    }

    #[repr(C)]
    struct SpaHook {
        link: SpaList,
        cb: SpaCallbacks,
        removed: Option<extern "C" fn(*mut SpaHook)>,
        priv_: *mut c_void,
    }

    impl Default for SpaHook {
        fn default() -> Self {
            Self {
                link: SpaList {
                    next: ptr::null_mut(),
                    prev: ptr::null_mut(),
                },
                cb: SpaCallbacks {
                    funcs: ptr::null(),
                    data: ptr::null_mut(),
                },
                removed: None,
                priv_: ptr::null_mut(),
            }
        }
    }

    #[repr(C)]
    struct PwStreamEvents {
        version: u32,
        destroy: Option<extern "C" fn(*mut c_void)>,
        state_changed: Option<extern "C" fn(*mut c_void, u32, u32, *const c_char)>,
        control_info: Option<extern "C" fn(*mut c_void, u32, *const c_void)>,
        io_changed: Option<extern "C" fn(*mut c_void, u32, *mut c_void, u32)>,
        param_changed: Option<extern "C" fn(*mut c_void, u32, *const SpaPod)>,
        add_buffer: Option<extern "C" fn(*mut c_void, *mut PwBuffer)>,
        remove_buffer: Option<extern "C" fn(*mut c_void, *mut PwBuffer)>,
        process: Option<extern "C" fn(*mut c_void)>,
        drained: Option<extern "C" fn(*mut c_void)>,
        command: Option<extern "C" fn(*mut c_void, *const c_void)>,
        trigger_done: Option<extern "C" fn(*mut c_void)>,
    }

    #[repr(C)]
    struct PwBuffer {
        buffer: *mut SpaBuffer,
        user_data: *mut c_void,
        size: u64,
        requested: u64,
        time: u64,
    }

    #[repr(C)]
    struct SpaBuffer {
        n_metas: u32,
        metas: *mut c_void,
        n_datas: u32,
        datas: *mut SpaData,
    }

    #[repr(C)]
    struct SpaData {
        type_: u32,
        flags: u32,
        fd: i64,
        mapoffset: u32,
        maxsize: u32,
        data: *mut c_void,
        chunk: *mut SpaChunk,
    }

    #[repr(C)]
    struct SpaChunk {
        offset: u32,
        size: u32,
        stride: i32,
        flags: i32,
    }

    struct NativeStreamData {
        stream: *mut PwStream,
        request: PipeWireFrameRequest,
        format: PipeWireFrameFormat,
        format_negotiated: bool,
        sequence: u64,
        sender: SyncSender<Result<PipeWireRawFrame, String>>,
    }

    unsafe extern "C" {
        fn dup(oldfd: c_int) -> c_int;
    }

    #[link(name = "pipewire-0.3")]
    unsafe extern "C" {
        fn pw_init(argc: *mut c_int, argv: *mut *mut *mut c_char);
        fn pw_main_loop_new(props: *const SpaDict) -> *mut PwMainLoop;
        fn pw_main_loop_destroy(loop_: *mut PwMainLoop);
        fn pw_main_loop_get_loop(loop_: *mut PwMainLoop) -> *mut PwLoop;
        fn pw_context_new(
            main_loop: *mut PwLoop,
            props: *mut c_void,
            user_data_size: usize,
        ) -> *mut PwContext;
        fn pw_context_destroy(context: *mut PwContext);
        fn pw_context_connect_fd(
            context: *mut PwContext,
            fd: c_int,
            properties: *mut c_void,
            user_data_size: usize,
        ) -> *mut PwCore;
        fn pw_core_disconnect(core: *mut PwCore);
        fn pw_loop_iterate(loop_: *mut PwLoop, timeout: c_int) -> c_int;
        fn pw_properties_new_string(args: *const c_char) -> *mut PwProperties;
        fn pw_properties_set(
            properties: *mut PwProperties,
            key: *const c_char,
            value: *const c_char,
        ) -> c_int;
        fn pw_properties_free(properties: *mut PwProperties);
        fn pw_stream_new(
            core: *mut PwCore,
            name: *const c_char,
            props: *mut PwProperties,
        ) -> *mut PwStream;
        fn pw_stream_destroy(stream: *mut PwStream);
        fn pw_stream_add_listener(
            stream: *mut PwStream,
            listener: *mut SpaHook,
            events: *const PwStreamEvents,
            data: *mut c_void,
        );
        fn pw_stream_connect(
            stream: *mut PwStream,
            direction: u32,
            target_id: u32,
            flags: u32,
            params: *mut *const SpaPod,
            n_params: u32,
        ) -> c_int;
        fn pw_stream_dequeue_buffer(stream: *mut PwStream) -> *mut PwBuffer;
        fn pw_stream_queue_buffer(stream: *mut PwStream, buffer: *mut PwBuffer) -> c_int;
    }

    pub(super) fn connect_remote_and_read_frame(
        remote: Arc<PipeWireRemoteFd>,
        request: PipeWireFrameRequest,
    ) -> Result<PipeWireRawFrame, ScreenError> {
        let fd = duplicate_fd(&remote)?;
        let connection = PipeWireConnection::connect_fd(fd)?;
        let (sender, receiver) = sync_channel(1);
        let stream = PipeWireStream::connect(&connection, request, sender)?;
        let started = Instant::now();

        loop {
            match receiver.try_recv() {
                Ok(Ok(frame)) => return Ok(frame),
                Ok(Err(error)) => return Err(ScreenError::Backend(error)),
                Err(TryRecvError::Disconnected) => {
                    return Err(ScreenError::Backend(
                        "PipeWire stream reader disconnected".into(),
                    ));
                }
                Err(TryRecvError::Empty) => {}
            }
            if started.elapsed() >= STREAM_WAIT_TIMEOUT {
                return Err(ScreenError::Backend(
                    "timed out waiting for PipeWire frame".into(),
                ));
            }
            stream.iterate(100)?;
        }
    }

    fn duplicate_fd(remote: &PipeWireRemoteFd) -> Result<OwnedFd, ScreenError> {
        let raw = unsafe { dup(remote.fd.as_raw_fd()) };
        if raw < 0 {
            return Err(ScreenError::Backend(format!(
                "duplicate PipeWire portal fd: {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(unsafe { OwnedFd::from_raw_fd(raw) })
    }

    struct PipeWireConnection {
        core: *mut PwCore,
        context: *mut PwContext,
        main_loop: *mut PwMainLoop,
        loop_: *mut PwLoop,
    }

    impl PipeWireConnection {
        fn connect_fd(fd: OwnedFd) -> Result<Self, ScreenError> {
            unsafe {
                pw_init(ptr::null_mut(), ptr::null_mut());
                let main_loop = pw_main_loop_new(ptr::null());
                if main_loop.is_null() {
                    return Err(ScreenError::Backend("create PipeWire main loop".into()));
                }
                let loop_ptr = pw_main_loop_get_loop(main_loop);
                let context = pw_context_new(loop_ptr, ptr::null_mut(), 0);
                if context.is_null() {
                    pw_main_loop_destroy(main_loop);
                    return Err(ScreenError::Backend("create PipeWire context".into()));
                }
                let core = pw_context_connect_fd(context, fd.as_raw_fd(), ptr::null_mut(), 0);
                if core.is_null() {
                    pw_context_destroy(context);
                    pw_main_loop_destroy(main_loop);
                    return Err(ScreenError::Backend(
                        "connect PipeWire context to portal fd".into(),
                    ));
                }
                std::mem::forget(fd);
                Ok(Self {
                    core,
                    context,
                    main_loop,
                    loop_: loop_ptr,
                })
            }
        }
    }

    impl Drop for PipeWireConnection {
        fn drop(&mut self) {
            unsafe {
                pw_core_disconnect(self.core);
                pw_context_destroy(self.context);
                pw_main_loop_destroy(self.main_loop);
            }
        }
    }

    struct PipeWireStream {
        stream: *mut PwStream,
        listener: SpaHook,
        events: PwStreamEvents,
        data: Box<NativeStreamData>,
        loop_: *mut PwLoop,
    }

    impl PipeWireStream {
        fn connect(
            connection: &PipeWireConnection,
            request: PipeWireFrameRequest,
            sender: SyncSender<Result<PipeWireRawFrame, String>>,
        ) -> Result<Self, ScreenError> {
            let props = stream_properties(&request)?;
            let name = CString::new("nexkvm-screencast")
                .map_err(|error| ScreenError::Backend(format!("PipeWire stream name: {error}")))?;
            let stream = unsafe { pw_stream_new(connection.core, name.as_ptr(), props) };
            if stream.is_null() {
                unsafe {
                    pw_properties_free(props);
                }
                return Err(ScreenError::Backend("create PipeWire stream".into()));
            }

            let format = PipeWireFrameFormat::fixate(
                request.resolution,
                &[
                    PipeWireVideoFormat::Bgra,
                    PipeWireVideoFormat::Rgba,
                    PipeWireVideoFormat::Nv12,
                ],
            )?;
            let mut data = Box::new(NativeStreamData {
                stream,
                format,
                format_negotiated: false,
                request,
                sequence: 1,
                sender,
            });
            let mut listener = SpaHook::default();
            let events = PwStreamEvents {
                version: 0,
                destroy: None,
                state_changed: None,
                control_info: None,
                io_changed: None,
                param_changed: Some(on_stream_param_changed),
                add_buffer: None,
                remove_buffer: None,
                process: Some(on_stream_process),
                drained: None,
                command: None,
                trigger_done: None,
            };
            unsafe {
                pw_stream_add_listener(
                    stream,
                    &mut listener,
                    &events,
                    (&mut *data as *mut NativeStreamData).cast(),
                );
            }
            let flags = PW_STREAM_FLAG_AUTOCONNECT | PW_STREAM_FLAG_MAP_BUFFERS;
            let res = unsafe {
                pw_stream_connect(
                    stream,
                    PW_DIRECTION_INPUT,
                    PW_ID_ANY,
                    flags,
                    ptr::null_mut(),
                    0,
                )
            };
            if res < 0 {
                unsafe {
                    pw_stream_destroy(stream);
                }
                return Err(ScreenError::Backend(format!(
                    "connect PipeWire stream: {res}"
                )));
            }

            Ok(Self {
                stream,
                listener,
                events,
                data,
                loop_: connection.loop_,
            })
        }

        fn iterate(&self, timeout_ms: i32) -> Result<(), ScreenError> {
            let res = unsafe { pw_loop_iterate(self.loop_, timeout_ms) };
            if res < 0 {
                return Err(ScreenError::Backend(format!(
                    "iterate PipeWire loop: {res}"
                )));
            }
            Ok(())
        }
    }

    impl Drop for PipeWireStream {
        fn drop(&mut self) {
            let _ = &self.listener;
            let _ = &self.events;
            let _ = &self.data;
            unsafe {
                pw_stream_destroy(self.stream);
            }
        }
    }

    fn stream_properties(request: &PipeWireFrameRequest) -> Result<*mut PwProperties, ScreenError> {
        let empty = CString::new("{}")
            .map_err(|error| ScreenError::Backend(format!("PipeWire props json: {error}")))?;
        let props = unsafe { pw_properties_new_string(empty.as_ptr()) };
        if props.is_null() {
            return Err(ScreenError::Backend(
                "create PipeWire stream properties".into(),
            ));
        }

        set_property(props, "application.name", "NexKVM")?;
        set_property(props, "media.type", "Video")?;
        set_property(props, "media.category", "Capture")?;
        set_property(props, "media.role", "Screen")?;
        set_property(props, "target.object", &request.target.target_object())?;
        Ok(props)
    }

    fn set_property(props: *mut PwProperties, key: &str, value: &str) -> Result<(), ScreenError> {
        let key = CString::new(key)
            .map_err(|error| ScreenError::Backend(format!("PipeWire property key: {error}")))?;
        let value = CString::new(value)
            .map_err(|error| ScreenError::Backend(format!("PipeWire property value: {error}")))?;
        let res = unsafe { pw_properties_set(props, key.as_ptr(), value.as_ptr()) };
        if res < 0 {
            return Err(ScreenError::Backend(format!(
                "set PipeWire property {}: {res}",
                key.to_string_lossy()
            )));
        }
        Ok(())
    }

    extern "C" fn on_stream_process(data: *mut c_void) {
        let data = unsafe { &mut *data.cast::<NativeStreamData>() };
        match unsafe { dequeue_stream_frame(data) } {
            Ok(Some(frame)) => {
                let _ = data.sender.try_send(Ok(frame));
            }
            Ok(None) => {}
            Err(error) => {
                let _ = data.sender.try_send(Err(error));
            }
        }
    }

    extern "C" fn on_stream_param_changed(data: *mut c_void, id: u32, param: *const SpaPod) {
        if param.is_null() || id != SPA_PARAM_FORMAT {
            return;
        }
        let data = unsafe { &mut *data.cast::<NativeStreamData>() };
        let pod_len = unsafe { ((*param).size as usize).saturating_add(8) };
        let pod = unsafe { std::slice::from_raw_parts(param.cast::<u8>(), pod_len) };
        let format = parse_spa_format_pod_bytes(pod)
            .map(PipeWireFrameFormat::from_spa_raw_info)
            .unwrap_or_else(|| {
                PipeWireFrameFormat::fixate(
                    data.request.resolution,
                    &[
                        PipeWireVideoFormat::Bgra,
                        PipeWireVideoFormat::Rgba,
                        PipeWireVideoFormat::Nv12,
                    ],
                )
            });
        match format {
            Ok(format) => {
                data.format = format;
                data.format_negotiated = true;
            }
            Err(error) => {
                let _ = data.sender.try_send(Err(error.to_string()));
            }
        }
    }

    unsafe fn dequeue_stream_frame(
        data: &mut NativeStreamData,
    ) -> Result<Option<PipeWireRawFrame>, String> {
        let buffer = unsafe { pw_stream_dequeue_buffer(data.stream) };
        if buffer.is_null() {
            return Ok(None);
        }

        let frame = extract_frame_from_pw_buffer(data, buffer);
        unsafe {
            let _ = pw_stream_queue_buffer(data.stream, buffer);
        }
        frame.map(Some)
    }

    fn extract_frame_from_pw_buffer(
        data: &mut NativeStreamData,
        buffer: *mut PwBuffer,
    ) -> Result<PipeWireRawFrame, String> {
        let spa = unsafe { (*buffer).buffer };
        if spa.is_null() {
            return Err("PipeWire buffer did not include a SPA buffer".into());
        }
        let spa = unsafe { &*spa };
        if spa.n_datas == 0 || spa.datas.is_null() {
            return Err("PipeWire SPA buffer had no data planes".into());
        }

        let plane = unsafe { &*spa.datas };
        let chunk = if plane.chunk.is_null() {
            return Err("PipeWire SPA data plane had no chunk".into());
        } else {
            unsafe { &*plane.chunk }
        };
        let maxsize = usize::try_from(plane.maxsize)
            .map_err(|_| "PipeWire data plane maxsize overflow".to_string())?;
        let chunk_offset = if maxsize == 0 {
            0
        } else {
            usize::try_from(chunk.offset)
                .map_err(|_| "PipeWire chunk offset overflow".to_string())?
                % maxsize
        };
        let chunk_size = usize::try_from(chunk.size)
            .map_err(|_| "PipeWire chunk size overflow".to_string())?
            .min(maxsize.saturating_sub(chunk_offset));

        let bytes = if plane.data.is_null() {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(plane.data.cast::<u8>(), maxsize) }
        };
        let frame = pipewire_frame_from_mapped_buffer(
            PipeWireMappedBuffer {
                bytes,
                chunk_offset,
                chunk_size,
                fd: (plane.fd >= 0).then_some(plane.fd),
            },
            data.request.clone(),
            data.format,
            data.sequence,
            unsafe { (*buffer).time / 1_000 },
        )
        .map_err(|error| error.to_string())?;
        data.sequence = data.sequence.saturating_add(1);
        Ok(frame)
    }
}

fn portal_options<'a>() -> HashMap<&'a str, Value<'a>> {
    HashMap::new()
}

fn take_portal_result<T>(
    results: &mut HashMap<String, OwnedValue>,
    key: &'static str,
) -> Result<T, ScreenError>
where
    T: TryFrom<OwnedValue>,
    T::Error: std::fmt::Display,
{
    let value = results
        .remove(key)
        .ok_or_else(|| ScreenError::Backend(format!("portal response missing `{key}`")))?;
    value
        .try_into()
        .map_err(|error| ScreenError::Backend(format!("portal response `{key}`: {error}")))
}

fn optional_portal_result<T>(
    results: &mut HashMap<String, OwnedValue>,
    key: &'static str,
) -> Result<Option<T>, ScreenError>
where
    T: TryFrom<OwnedValue>,
    T::Error: std::fmt::Display,
{
    results
        .remove(key)
        .map(|value| {
            value
                .try_into()
                .map_err(|error| ScreenError::Backend(format!("portal response `{key}`: {error}")))
        })
        .transpose()
}

fn portal_token(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);
    let next = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}_{next}")
}

fn screen_portal_call_error(
    operation: &'static str,
) -> impl FnOnce(zbus::Error) -> ScreenError + Send + Sync + 'static {
    move |error| ScreenError::Backend(format!("{operation}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use nexkvm_streaming::{
        CaptureSource, CaptureSourceId, GpuMemoryKind, HardwareEncoder, PixelFormat, ScreenCodec,
        ScreenFrame, ScreenResolution, ScreenStreamCapabilities, ScreenStreamIntent,
        ScreenStreamPlan,
    };
    use std::fs::File;
    use std::os::fd::OwnedFd;
    use std::sync::Mutex;

    #[derive(Debug, Default)]
    struct RecordingScreenCastTransport {
        opened: Mutex<usize>,
    }

    #[async_trait]
    impl XdgDesktopPortalScreenCastTransport for RecordingScreenCastTransport {
        async fn open_screen_cast(
            &self,
        ) -> Result<PipeWireScreenCastSession, nexkvm_streaming::ScreenError> {
            *self.opened.lock().expect("poisoned") += 1;
            Ok(PipeWireScreenCastSession {
                session_handle: "/org/freedesktop/portal/desktop/session/nexkvm/screencast".into(),
                streams: vec![PipeWireScreenCastStream {
                    node_id: 42,
                    id: Some("monitor-1".into()),
                    position: Some((0, 0)),
                    size: Some((1920, 1080)),
                    source_type: 1,
                    mapping_id: Some("display-main".into()),
                    pipewire_serial: Some(9001),
                }],
                remote: Some(Arc::new(PipeWireRemoteFd {
                    fd: OwnedFd::from(File::open("/dev/null").expect("open /dev/null")),
                })),
            })
        }
    }

    #[derive(Debug)]
    struct QueuedPipeWireFrameReader {
        frame: Mutex<Option<PipeWireRawFrame>>,
        requests: Mutex<Vec<PipeWireFrameRequest>>,
    }

    impl QueuedPipeWireFrameReader {
        fn new(frame: PipeWireRawFrame) -> Self {
            Self {
                frame: Mutex::new(Some(frame)),
                requests: Mutex::new(Vec::new()),
            }
        }

        fn requests(&self) -> Vec<PipeWireFrameRequest> {
            self.requests.lock().expect("poisoned").clone()
        }
    }

    #[async_trait]
    impl PipeWireFrameReader for QueuedPipeWireFrameReader {
        async fn next_frame(
            &self,
            _remote: Arc<PipeWireRemoteFd>,
            request: PipeWireFrameRequest,
        ) -> Result<PipeWireRawFrame, ScreenError> {
            self.requests.lock().expect("poisoned").push(request);
            self.frame
                .lock()
                .expect("poisoned")
                .take()
                .ok_or_else(|| ScreenError::Backend("no queued frame".into()))
        }
    }

    fn spa_format_pod_fixture(spa_format: u32, width: u32, height: u32) -> Vec<u8> {
        let mut body = Vec::new();
        push_u32(&mut body, SPA_TYPE_OBJECT_FORMAT);
        push_u32(&mut body, SPA_PARAM_FORMAT);
        push_spa_id_prop(&mut body, SPA_FORMAT_MEDIA_TYPE, SPA_MEDIA_TYPE_VIDEO);
        push_spa_id_prop(&mut body, SPA_FORMAT_MEDIA_SUBTYPE, SPA_MEDIA_SUBTYPE_RAW);
        push_spa_id_prop(&mut body, SPA_FORMAT_VIDEO_FORMAT, spa_format);
        push_spa_rectangle_prop(&mut body, SPA_FORMAT_VIDEO_SIZE, width, height);

        let mut pod = Vec::new();
        push_u32(&mut pod, u32::try_from(body.len()).expect("body len"));
        push_u32(&mut pod, SPA_TYPE_OBJECT);
        pod.extend_from_slice(&body);
        pod
    }

    fn push_spa_id_prop(out: &mut Vec<u8>, key: u32, value: u32) {
        push_u32(out, key);
        push_u32(out, 0);
        push_u32(out, 4);
        push_u32(out, SPA_TYPE_ID);
        push_u32(out, value);
        pad_8(out);
    }

    fn push_spa_rectangle_prop(out: &mut Vec<u8>, key: u32, width: u32, height: u32) {
        push_u32(out, key);
        push_u32(out, 0);
        push_u32(out, 8);
        push_u32(out, SPA_TYPE_RECTANGLE);
        push_u32(out, width);
        push_u32(out, height);
        pad_8(out);
    }

    fn push_u32(out: &mut Vec<u8>, value: u32) {
        out.extend_from_slice(&value.to_le_bytes());
    }

    fn pad_8(out: &mut Vec<u8>) {
        while out.len() % 8 != 0 {
            out.push(0);
        }
    }

    #[tokio::test]
    async fn pipewire_backend_lists_portal_streams_as_display_sources() {
        let backend = LinuxPipeWireScreenCapture::new(RecordingScreenCastTransport::default());
        let permissions = backend.request_permissions().await.expect("permissions");
        assert!(permissions.display_capture);
        assert!(permissions.window_capture);

        let sources = backend.list_sources().await.expect("sources");
        assert_eq!(
            sources,
            vec![CaptureSource::Display {
                id: CaptureSourceId::new("pipewire-serial:9001"),
                label: "PipeWire monitor monitor-1 (1920x1080 at 0,0)".into(),
            }]
        );
        assert_eq!(*backend.transport().opened.lock().expect("poisoned"), 1);
    }

    #[tokio::test]
    async fn pipewire_backend_reports_frame_decode_pending() {
        let backend = LinuxPipeWireScreenCapture::new(RecordingScreenCastTransport::default());
        backend.request_permissions().await.expect("permissions");
        let plan = ScreenStreamPlan {
            from: nexkvm_core::DeviceId::generate(),
            to: nexkvm_core::DeviceId::generate(),
            source: CaptureSource::Display {
                id: CaptureSourceId::new("pipewire-serial:9001"),
                label: "PipeWire monitor monitor-1".into(),
            },
            intent: ScreenStreamIntent::InteractiveRemote,
            codec: ScreenCodec::RawRgba,
            encoder: HardwareEncoder::Software,
            memory: GpuMemoryKind::DmaBuf,
            resolution: ScreenResolution::new(1920, 1080),
            fps: 60,
            bitrate_kbps: 8_000,
            zero_copy: true,
            requires_encrypted_transport: true,
        };

        let error = backend
            .capture_frame(&plan)
            .await
            .expect_err("frame decode pending");
        assert!(
            matches!(error, nexkvm_streaming::ScreenError::Backend(message) if message.contains("PipeWire frame decoding"))
        );

        let _shape = ScreenFrame {
            sequence: 1,
            capture_time_micros: 0,
            resolution: ScreenResolution::new(1, 1),
            pixel_format: PixelFormat::Bgra8,
            memory: GpuMemoryKind::DmaBuf,
            payload: bytes::Bytes::new(),
        };
    }

    #[test]
    fn pipewire_backend_capabilities_advertise_dmabuf_capture() {
        let caps =
            LinuxPipeWireScreenCapture::new(RecordingScreenCastTransport::default()).capabilities();
        assert_eq!(
            caps.memory_kinds,
            vec![GpuMemoryKind::DmaBuf, GpuMemoryKind::System]
        );
        assert!(caps.codecs.contains(&ScreenCodec::RawRgba));
        assert!(caps.encoders.contains(&HardwareEncoder::Software));
        assert_eq!(caps.max_fps, 60);

        let _caps: ScreenStreamCapabilities = caps;
    }

    #[tokio::test]
    async fn pipewire_backend_captures_frame_from_selected_pipewire_serial() {
        let reader = QueuedPipeWireFrameReader::new(PipeWireRawFrame {
            sequence: 7,
            capture_time_micros: 1234,
            resolution: ScreenResolution::new(2, 1),
            pixel_format: PixelFormat::Bgra8,
            memory: GpuMemoryKind::System,
            payload: bytes::Bytes::from_static(&[1, 2, 3, 4, 5, 6, 7, 8]),
        });
        let backend = LinuxPipeWireScreenCapture::with_frame_reader(
            RecordingScreenCastTransport::default(),
            reader,
        );
        backend.request_permissions().await.expect("permissions");

        let frame = backend
            .capture_frame(&ScreenStreamPlan {
                from: nexkvm_core::DeviceId::generate(),
                to: nexkvm_core::DeviceId::generate(),
                source: CaptureSource::Display {
                    id: CaptureSourceId::new("pipewire-serial:9001"),
                    label: "PipeWire monitor monitor-1".into(),
                },
                intent: ScreenStreamIntent::InteractiveRemote,
                codec: ScreenCodec::RawRgba,
                encoder: HardwareEncoder::Software,
                memory: GpuMemoryKind::System,
                resolution: ScreenResolution::new(2, 1),
                fps: 60,
                bitrate_kbps: 8_000,
                zero_copy: false,
                requires_encrypted_transport: true,
            })
            .await
            .expect("frame");

        assert_eq!(frame.sequence, 7);
        assert_eq!(frame.resolution, ScreenResolution::new(2, 1));
        assert_eq!(frame.pixel_format, PixelFormat::Bgra8);
        assert_eq!(frame.memory, GpuMemoryKind::System);
        assert_eq!(
            frame.payload,
            bytes::Bytes::from_static(&[1, 2, 3, 4, 5, 6, 7, 8])
        );
        assert_eq!(
            backend.frame_reader().requests(),
            vec![PipeWireFrameRequest {
                target: PipeWireStreamTarget::ObjectSerial(9001),
                resolution: ScreenResolution::new(2, 1),
                memory: GpuMemoryKind::System,
            }]
        );
    }

    #[test]
    fn pipewire_raw_frame_rejects_short_system_rgba_payloads() {
        let error = PipeWireRawFrame {
            sequence: 1,
            capture_time_micros: 0,
            resolution: ScreenResolution::new(2, 2),
            pixel_format: PixelFormat::Bgra8,
            memory: GpuMemoryKind::System,
            payload: bytes::Bytes::from_static(&[1, 2, 3]),
        }
        .into_screen_frame()
        .expect_err("short payload rejected");

        assert!(
            matches!(error, nexkvm_streaming::ScreenError::Codec(message) if message.contains("payload"))
        );
    }

    #[test]
    fn pipewire_mapped_buffer_extracts_chunk_with_offset_and_size() {
        let frame = pipewire_frame_from_mapped_buffer(
            PipeWireMappedBuffer {
                bytes: &[9, 8, 1, 2, 3, 4, 7],
                chunk_offset: 2,
                chunk_size: 4,
                fd: None,
            },
            PipeWireFrameRequest {
                target: PipeWireStreamTarget::ObjectSerial(9001),
                resolution: ScreenResolution::new(1, 1),
                memory: GpuMemoryKind::System,
            },
            PipeWireFrameFormat {
                resolution: ScreenResolution::new(1, 1),
                video_format: PipeWireVideoFormat::Bgra,
                pixel_format: PixelFormat::Bgra8,
                stride: 4,
            },
            12,
            3456,
        )
        .expect("mapped frame");

        assert_eq!(frame.sequence, 12);
        assert_eq!(frame.capture_time_micros, 3456);
        assert_eq!(frame.resolution, ScreenResolution::new(1, 1));
        assert_eq!(frame.pixel_format, PixelFormat::Bgra8);
        assert_eq!(frame.memory, GpuMemoryKind::System);
        assert_eq!(frame.payload, bytes::Bytes::from_static(&[1, 2, 3, 4]));
    }

    #[test]
    fn pipewire_frame_format_fixates_bgra_stride_from_request() {
        let format = PipeWireFrameFormat::fixate(
            ScreenResolution::new(7, 5),
            &[PipeWireVideoFormat::Nv12, PipeWireVideoFormat::Bgra],
        )
        .expect("format");

        assert_eq!(format.resolution, ScreenResolution::new(7, 5));
        assert_eq!(format.video_format, PipeWireVideoFormat::Bgra);
        assert_eq!(format.pixel_format, PixelFormat::Bgra8);
        assert_eq!(format.stride, 28);
        assert_eq!(format.system_payload_len(), Some(140));
    }

    #[test]
    fn pipewire_frame_format_maps_parsed_spa_raw_info() {
        let format = PipeWireFrameFormat::from_spa_raw_info(PipeWireSpaRawVideoInfo {
            spa_format: SPA_VIDEO_FORMAT_RGBA,
            width: 1280,
            height: 720,
            stride: Some(1280 * 4),
        })
        .expect("format");

        assert_eq!(format.resolution, ScreenResolution::new(1280, 720));
        assert_eq!(format.video_format, PipeWireVideoFormat::Rgba);
        assert_eq!(format.pixel_format, PixelFormat::Rgba8);
        assert_eq!(format.stride, 5120);
    }

    #[test]
    fn pipewire_spa_format_pod_parser_extracts_raw_video_info() {
        let pod = spa_format_pod_fixture(SPA_VIDEO_FORMAT_BGRA, 1920, 1080);
        let info = parse_spa_format_pod_bytes(&pod).expect("raw video info");

        assert_eq!(
            info,
            PipeWireSpaRawVideoInfo {
                spa_format: SPA_VIDEO_FORMAT_BGRA,
                width: 1920,
                height: 1080,
                stride: None,
            }
        );
    }
}
