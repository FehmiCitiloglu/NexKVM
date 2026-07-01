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
    use super::{PipeWireFrameRequest, PipeWireRawFrame, PipeWireRemoteFd, ScreenError};
    use std::ffi::{c_char, c_int, c_void};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::ptr;
    use std::sync::Arc;

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
    }

    pub(super) fn connect_remote_and_read_frame(
        remote: Arc<PipeWireRemoteFd>,
        request: PipeWireFrameRequest,
    ) -> Result<PipeWireRawFrame, ScreenError> {
        let _target_object = request.target.target_object();
        let fd = duplicate_fd(&remote)?;
        let _connection = PipeWireConnection::connect_fd(fd)?;
        Err(ScreenError::Backend(
            "PipeWire stream process/dequeue frame pump is not wired yet".into(),
        ))
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
}
