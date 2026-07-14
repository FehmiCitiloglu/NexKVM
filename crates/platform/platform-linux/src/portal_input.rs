//! Wayland portal-mediated input capture and injection boundary.
//!
//! Native Wayland sessions do not allow global hooks or synthetic input through
//! raw process APIs. This module keeps Linux input integration behind a
//! compositor-mediated portal client: RemoteDesktop grants injection and
//! InputCapture grants captured events. The concrete D-Bus/libei client can
//! implement [`WaylandPortalInputClient`] without changing the daemon-facing
//! [`InputCapture`] and [`InputInjector`] contracts.

use async_trait::async_trait;
use nexkvm_input::{InjectionCommand, InputCapture, InputError, InputEvent, InputInjector};
use std::collections::HashMap;
use std::os::fd::OwnedFd;
#[cfg(target_os = "linux")]
use std::os::{fd::AsFd, unix::net::UnixStream};
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::thread;
use zbus::export::futures_util::StreamExt;
use zbus::{Connection, Proxy};
use zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

use crate::PortalAvailability;

const PORTAL_DESTINATION: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const REMOTE_DESKTOP_INTERFACE: &str = "org.freedesktop.portal.RemoteDesktop";
const INPUT_CAPTURE_INTERFACE: &str = "org.freedesktop.portal.InputCapture";
const REQUEST_INTERFACE: &str = "org.freedesktop.portal.Request";
const POINTER_AND_KEYBOARD: u32 = 1 | 2;

/// Input permissions granted by the Wayland portal session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PortalInputGrant {
    /// RemoteDesktop portal permission for compositor-mediated input injection.
    pub remote_desktop: bool,
    /// InputCapture portal permission for compositor-mediated capture.
    pub input_capture: bool,
}

impl PortalInputGrant {
    const REQUIRED: Self = Self {
        remote_desktop: true,
        input_capture: true,
    };

    const fn satisfies(self, required: Self) -> bool {
        (!required.remote_desktop || self.remote_desktop)
            && (!required.input_capture || self.input_capture)
    }
}

/// Client for the Linux Wayland input portals.
#[async_trait]
pub trait WaylandPortalInputClient: Send + Sync {
    /// Request a portal input session with the required capabilities.
    ///
    /// # Errors
    /// Returns [`InputError`] when the compositor denies or cannot provide the
    /// requested portal grants.
    async fn request_input_session(
        &self,
        required: PortalInputGrant,
    ) -> Result<PortalInputGrant, InputError>;

    /// Send one compositor-mediated injection command through the portal.
    ///
    /// # Errors
    /// Returns [`InputError`] if the portal session rejects the command.
    async fn inject(&self, command: InjectionCommand) -> Result<(), InputError>;

    /// Read the next event emitted by the InputCapture portal.
    ///
    /// # Errors
    /// Returns [`InputError`] if the capture stream stops or is unavailable.
    async fn next_event(&self) -> Result<InputEvent, InputError>;
}

/// Decoder for events transported over the portal-provided EIS connection.
#[async_trait]
pub trait PortalEisEventDecoder: Send + Sync {
    /// Decode the next input event from `connection`.
    ///
    /// # Errors
    /// Returns [`InputError`] when the EIS stream stops or emits an unsupported
    /// event.
    async fn next_event(&self, connection: &PortalEisConnection) -> Result<InputEvent, InputError>;
}

/// EIS event decoder backed by the pure-Rust `reis` libei implementation.
#[cfg(target_os = "linux")]
pub struct ReisPortalEisEventDecoder {
    streams: tokio::sync::Mutex<
        HashMap<String, tokio::sync::mpsc::UnboundedReceiver<Result<InputEvent, InputError>>>,
    >,
}

/// EIS event decoder backed by the pure-Rust `reis` libei implementation.
#[cfg(not(target_os = "linux"))]
pub struct ReisPortalEisEventDecoder;

#[cfg(target_os = "linux")]
impl Default for ReisPortalEisEventDecoder {
    fn default() -> Self {
        Self {
            streams: tokio::sync::Mutex::new(HashMap::new()),
        }
    }
}

#[cfg(not(target_os = "linux"))]
impl Default for ReisPortalEisEventDecoder {
    fn default() -> Self {
        Self
    }
}

#[cfg(target_os = "linux")]
impl std::fmt::Debug for ReisPortalEisEventDecoder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReisPortalEisEventDecoder")
            .finish_non_exhaustive()
    }
}

#[cfg(not(target_os = "linux"))]
impl std::fmt::Debug for ReisPortalEisEventDecoder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReisPortalEisEventDecoder")
            .finish_non_exhaustive()
    }
}

#[cfg(target_os = "linux")]
impl ReisPortalEisEventDecoder {
    fn start_stream(
        connection: &PortalEisConnection,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<Result<InputEvent, InputError>>, InputError>
    {
        let fd = connection
            .fd
            .as_ref()
            .ok_or_else(|| InputError::Backend("portal EIS fd is not available".into()))?
            .fd
            .as_fd()
            .try_clone_to_owned()
            .map_err(|error| InputError::Backend(format!("clone portal EIS fd: {error}")))?;
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        thread::Builder::new()
            .name(format!("nexkvm-reis-eis-{}", connection.handle))
            .spawn(move || run_reis_eis_worker(fd, sender))
            .map_err(|error| InputError::Backend(format!("start reis EIS worker: {error}")))?;
        Ok(receiver)
    }
}

#[cfg(target_os = "linux")]
fn run_reis_eis_worker(
    fd: OwnedFd,
    sender: tokio::sync::mpsc::UnboundedSender<Result<InputEvent, InputError>>,
) {
    let socket = UnixStream::from(fd);
    let context = match reis::ei::Context::new(socket) {
        Ok(context) => context,
        Err(error) => {
            let _ = sender.send(Err(InputError::Backend(format!(
                "create reis EI context: {error}"
            ))));
            return;
        }
    };
    let (_connection, events) =
        match context.handshake_blocking("nexkvm", reis::ei::handshake::ContextType::Receiver) {
            Ok(stream) => stream,
            Err(error) => {
                let _ = sender.send(Err(InputError::Backend(format!(
                    "reis EI handshake: {error}"
                ))));
                return;
            }
        };
    for event in events {
        let event = match event {
            Ok(event) => event,
            Err(error) => {
                let _ = sender.send(Err(InputError::Backend(format!(
                    "decode portal EIS event: {error}"
                ))));
                return;
            }
        };
        match reis_event_to_input(event) {
            Ok(Some(input)) if sender.send(Ok(input)).is_err() => return,
            Ok(Some(_)) | Ok(None) => {}
            Err(error) => {
                let _ = sender.send(Err(error));
                return;
            }
        }
    }
    let _ = sender.send(Err(InputError::Backend("portal EIS stream ended".into())));
}

#[cfg(target_os = "linux")]
#[async_trait]
impl PortalEisEventDecoder for ReisPortalEisEventDecoder {
    async fn next_event(&self, connection: &PortalEisConnection) -> Result<InputEvent, InputError> {
        let mut streams = self.streams.lock().await;
        if !streams.contains_key(&connection.handle) {
            let stream = Self::start_stream(connection)?;
            streams.insert(connection.handle.clone(), stream);
        }
        let stream = streams
            .get_mut(&connection.handle)
            .ok_or_else(|| InputError::Backend("portal EIS stream was not initialized".into()))?;
        stream
            .recv()
            .await
            .ok_or_else(|| InputError::Backend("portal EIS worker stopped".into()))?
    }
}

#[cfg(not(target_os = "linux"))]
#[async_trait]
impl PortalEisEventDecoder for ReisPortalEisEventDecoder {
    async fn next_event(&self, connection: &PortalEisConnection) -> Result<InputEvent, InputError> {
        if connection.fd.is_none() {
            return Err(InputError::Backend("portal EIS fd is not available".into()));
        }
        Err(InputError::Backend(
            "reis portal EIS decoder is only available on Linux targets".into(),
        ))
    }
}

/// RemoteDesktop/InputCapture method surface used by the concrete client.
#[async_trait]
pub trait XdgDesktopPortalInputTransport: Send + Sync {
    /// Create and start a RemoteDesktop session for pointer/keyboard injection.
    async fn open_remote_desktop(&self) -> Result<String, InputError>;

    /// Create and start an InputCapture session, returning an EIS descriptor
    /// marker when the compositor grants capture.
    async fn open_input_capture(&self) -> Result<PortalEisConnection, InputError>;

    /// Send one RemoteDesktop Notify* method call.
    async fn notify(
        &self,
        session_handle: &str,
        method: PortalNotifyMethod,
    ) -> Result<(), InputError>;

    /// Retrieve current InputCapture zones for a session.
    async fn get_zones(&self, session_handle: &str) -> Result<PortalZoneSet, InputError>;

    /// Configure pointer barriers for the current zone set.
    async fn set_pointer_barriers(
        &self,
        session_handle: &str,
        barriers: &[PortalPointerBarrier],
        zone_set: u32,
    ) -> Result<Vec<u32>, InputError>;

    /// Enable capture after barriers have been configured.
    async fn enable(&self, session_handle: &str) -> Result<(), InputError>;
}

/// libei/EIS connection exported by xdg-desktop-portal.
#[derive(Debug, Clone)]
pub struct PortalEisConnection {
    /// InputCapture portal session object path.
    pub session_handle: String,
    /// Stable label for the connection. The zbus transport owns the actual fd.
    pub handle: String,
    /// File descriptor connected to the compositor EIS endpoint, when supplied
    /// by the concrete transport.
    pub fd: Option<Arc<PortalEisFd>>,
}

impl PortalEisConnection {
    /// Whether this connection carries an EIS fd for a decoder backend.
    #[must_use]
    pub fn has_fd(&self) -> bool {
        self.fd.is_some()
    }
}

impl PartialEq for PortalEisConnection {
    fn eq(&self, other: &Self) -> bool {
        self.session_handle == other.session_handle
            && self.handle == other.handle
            && self.has_fd() == other.has_fd()
    }
}

impl Eq for PortalEisConnection {}

/// Owned EIS fd returned by the concrete D-Bus transport.
#[derive(Debug)]
pub struct PortalEisFd {
    /// File descriptor connected to the compositor EIS endpoint.
    pub fd: OwnedFd,
}

/// InputCapture zone returned by the portal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortalInputZone {
    /// Zone width.
    pub width: u32,
    /// Zone height.
    pub height: u32,
    /// Zone x offset.
    pub x: i32,
    /// Zone y offset.
    pub y: i32,
}

/// Current portal zone set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortalZoneSet {
    /// Zone-set id required by `SetPointerBarriers`.
    pub id: u32,
    /// Available zones.
    pub zones: Vec<PortalInputZone>,
}

/// Pointer barrier used to trigger InputCapture activation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortalPointerBarrier {
    /// Non-zero barrier id.
    pub id: u32,
    /// First x coordinate.
    pub x1: i32,
    /// First y coordinate.
    pub y1: i32,
    /// Second x coordinate.
    pub x2: i32,
    /// Second y coordinate.
    pub y2: i32,
}

/// xdg-desktop-portal D-Bus transport for RemoteDesktop/InputCapture.
#[derive(Debug, Clone)]
pub struct ZbusXdgDesktopPortalInputTransport {
    connection: Connection,
}

impl ZbusXdgDesktopPortalInputTransport {
    /// Connect to the user session bus.
    ///
    /// # Errors
    /// Returns [`InputError`] if the D-Bus session bus is unavailable.
    pub async fn session() -> Result<Self, InputError> {
        let connection = Connection::session()
            .await
            .map_err(|error| InputError::Backend(format!("connect session bus: {error}")))?;
        Ok(Self { connection })
    }

    async fn proxy(&self, interface: &'static str) -> Result<Proxy<'_>, InputError> {
        Proxy::new(&self.connection, PORTAL_DESTINATION, PORTAL_PATH, interface)
            .await
            .map_err(|error| InputError::Backend(format!("create portal proxy: {error}")))
    }

    async fn request_results(
        &self,
        handle: OwnedObjectPath,
        operation: &'static str,
    ) -> Result<HashMap<String, OwnedValue>, InputError> {
        let proxy = Proxy::new(
            &self.connection,
            PORTAL_DESTINATION,
            handle.as_str(),
            REQUEST_INTERFACE,
        )
        .await
        .map_err(|error| InputError::Backend(format!("{operation} request proxy: {error}")))?;
        let mut responses = proxy.receive_signal("Response").await.map_err(|error| {
            InputError::Backend(format!("{operation} response stream: {error}"))
        })?;
        let message = responses
            .next()
            .await
            .ok_or_else(|| InputError::Backend(format!("{operation} response stream ended")))?;
        let (code, results): (u32, HashMap<String, OwnedValue>) =
            message.body().deserialize().map_err(|error| {
                InputError::Backend(format!("{operation} response decode: {error}"))
            })?;
        if code == 0 {
            Ok(results)
        } else {
            Err(InputError::Backend(format!(
                "{operation} failed with portal response code {code}"
            )))
        }
    }

    fn session_handle(token: &str) -> String {
        format!("/org/freedesktop/portal/desktop/session/nexkvm/{token}")
    }
}

#[async_trait]
impl XdgDesktopPortalInputTransport for ZbusXdgDesktopPortalInputTransport {
    async fn open_remote_desktop(&self) -> Result<String, InputError> {
        let proxy = self.proxy(REMOTE_DESKTOP_INTERFACE).await?;
        let token = portal_token("remote");
        let session_handle = Self::session_handle(&token);
        let mut options = portal_options();
        options.insert("session_handle_token", Value::from(token.as_str()));

        let _: OwnedObjectPath = proxy
            .call("CreateSession", &(options))
            .await
            .map_err(portal_call_error("RemoteDesktop.CreateSession"))?;

        let session_path = ObjectPath::try_from(session_handle.as_str())
            .map_err(|error| InputError::Backend(format!("session object path: {error}")))?;
        let mut options = portal_options();
        options.insert("types", Value::from(POINTER_AND_KEYBOARD));
        let _: OwnedObjectPath = proxy
            .call("SelectDevices", &(&session_path, options))
            .await
            .map_err(portal_call_error("RemoteDesktop.SelectDevices"))?;

        let options = portal_options();
        let _: OwnedObjectPath = proxy
            .call("Start", &(&session_path, "", options))
            .await
            .map_err(portal_call_error("RemoteDesktop.Start"))?;

        Ok(session_handle)
    }

    async fn open_input_capture(&self) -> Result<PortalEisConnection, InputError> {
        let proxy = self.proxy(INPUT_CAPTURE_INTERFACE).await?;
        let token = portal_token("capture");
        let session_handle = Self::session_handle(&token);
        let mut options = portal_options();
        options.insert("session_handle_token", Value::from(token.as_str()));
        let _: HashMap<String, zvariant::OwnedValue> = proxy
            .call("CreateSession2", &("", options))
            .await
            .map_err(portal_call_error("InputCapture.CreateSession2"))?;

        let session_path = ObjectPath::try_from(session_handle.as_str()).map_err(|error| {
            InputError::Backend(format!("capture session object path: {error}"))
        })?;
        let mut options = portal_options();
        options.insert("capabilities", Value::from(POINTER_AND_KEYBOARD));
        let _: OwnedObjectPath = proxy
            .call("Start", &(&session_path, "", options))
            .await
            .map_err(portal_call_error("InputCapture.Start"))?;

        let options = portal_options();
        let fd: zvariant::OwnedFd = proxy
            .call("ConnectToEIS", &(&session_path, options))
            .await
            .map_err(portal_call_error("InputCapture.ConnectToEIS"))?;
        let fd = OwnedFd::from(fd);

        Ok(PortalEisConnection {
            session_handle: session_handle.clone(),
            handle: session_handle,
            fd: Some(Arc::new(PortalEisFd { fd })),
        })
    }

    async fn notify(
        &self,
        session_handle: &str,
        method: PortalNotifyMethod,
    ) -> Result<(), InputError> {
        let proxy = self.proxy(REMOTE_DESKTOP_INTERFACE).await?;
        let session_path = ObjectPath::try_from(session_handle)
            .map_err(|error| InputError::Backend(format!("notify session object path: {error}")))?;
        let options = portal_options();
        match method {
            PortalNotifyMethod::PointerMotionAbsolute { stream, x, y } => {
                proxy
                    .call::<_, _, ()>(
                        "NotifyPointerMotionAbsolute",
                        &(&session_path, options, stream, x, y),
                    )
                    .await
            }
            PortalNotifyMethod::PointerMotion { dx, dy } => {
                proxy
                    .call::<_, _, ()>("NotifyPointerMotion", &(&session_path, options, dx, dy))
                    .await
            }
            PortalNotifyMethod::PointerButton { button, state } => {
                proxy
                    .call::<_, _, ()>(
                        "NotifyPointerButton",
                        &(&session_path, options, button, state),
                    )
                    .await
            }
            PortalNotifyMethod::PointerAxis { dx, dy } => {
                proxy
                    .call::<_, _, ()>("NotifyPointerAxis", &(&session_path, options, dx, dy))
                    .await
            }
            PortalNotifyMethod::KeyboardKeycode { keycode, state } => {
                proxy
                    .call::<_, _, ()>(
                        "NotifyKeyboardKeycode",
                        &(&session_path, options, keycode, state),
                    )
                    .await
            }
        }
        .map_err(portal_call_error("RemoteDesktop.Notify"))?;
        Ok(())
    }

    async fn get_zones(&self, session_handle: &str) -> Result<PortalZoneSet, InputError> {
        let proxy = self.proxy(INPUT_CAPTURE_INTERFACE).await?;
        let session_path = ObjectPath::try_from(session_handle)
            .map_err(|error| InputError::Backend(format!("zones session object path: {error}")))?;
        let options = portal_options();
        let handle: OwnedObjectPath = proxy
            .call("GetZones", &(&session_path, options))
            .await
            .map_err(portal_call_error("InputCapture.GetZones"))?;
        let mut results = self
            .request_results(handle, "InputCapture.GetZones")
            .await?;

        let raw_zones: Vec<(u32, u32, i32, i32)> = take_portal_result(&mut results, "zones")?;
        let zone_set = take_portal_result(&mut results, "zone_set")?;
        Ok(PortalZoneSet {
            id: zone_set,
            zones: raw_zones
                .into_iter()
                .map(|(width, height, x, y)| PortalInputZone {
                    width,
                    height,
                    x,
                    y,
                })
                .collect(),
        })
    }

    async fn set_pointer_barriers(
        &self,
        session_handle: &str,
        barriers: &[PortalPointerBarrier],
        zone_set: u32,
    ) -> Result<Vec<u32>, InputError> {
        let proxy = self.proxy(INPUT_CAPTURE_INTERFACE).await?;
        let session_path = ObjectPath::try_from(session_handle).map_err(|error| {
            InputError::Backend(format!("barriers session object path: {error}"))
        })?;
        let options = portal_options();
        let barriers = portal_barriers(barriers);
        let handle: OwnedObjectPath = proxy
            .call(
                "SetPointerBarriers",
                &(&session_path, options, barriers, zone_set),
            )
            .await
            .map_err(portal_call_error("InputCapture.SetPointerBarriers"))?;
        let mut results = self
            .request_results(handle, "InputCapture.SetPointerBarriers")
            .await?;

        optional_portal_result(&mut results, "failed_barriers")
    }

    async fn enable(&self, session_handle: &str) -> Result<(), InputError> {
        let proxy = self.proxy(INPUT_CAPTURE_INTERFACE).await?;
        let session_path = ObjectPath::try_from(session_handle)
            .map_err(|error| InputError::Backend(format!("enable session object path: {error}")))?;
        let options = portal_options();
        proxy
            .call::<_, _, ()>("Enable", &(&session_path, options))
            .await
            .map_err(portal_call_error("InputCapture.Enable"))?;
        Ok(())
    }
}

/// RemoteDesktop Notify* method call.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PortalNotifyMethod {
    /// `NotifyPointerMotionAbsolute`.
    PointerMotionAbsolute {
        /// Stream id. Zero is used for input-only sessions.
        stream: u32,
        /// Logical x coordinate.
        x: f64,
        /// Logical y coordinate.
        y: f64,
    },
    /// `NotifyPointerMotion`.
    PointerMotion {
        /// Relative x movement.
        dx: f64,
        /// Relative y movement.
        dy: f64,
    },
    /// `NotifyPointerButton`.
    PointerButton {
        /// Linux evdev button code.
        button: i32,
        /// 1 for pressed, 0 for released.
        state: u32,
    },
    /// `NotifyPointerAxis`.
    PointerAxis {
        /// Horizontal scroll delta.
        dx: f64,
        /// Vertical scroll delta.
        dy: f64,
    },
    /// `NotifyKeyboardKeycode`.
    KeyboardKeycode {
        /// Linux keycode.
        keycode: i32,
        /// 1 for pressed, 0 for released.
        state: u32,
    },
}

/// Concrete portal client over an xdg-desktop-portal transport.
#[derive(Clone)]
pub struct XdgDesktopPortalInputClient<T> {
    transport: Arc<T>,
    remote_desktop_session: Arc<tokio::sync::Mutex<Option<String>>>,
    input_capture_session: Arc<tokio::sync::Mutex<Option<String>>>,
    eis_connection: Arc<tokio::sync::Mutex<Option<PortalEisConnection>>>,
    eis_decoder: Option<Arc<dyn PortalEisEventDecoder>>,
}

impl<T> std::fmt::Debug for XdgDesktopPortalInputClient<T>
where
    T: std::fmt::Debug,
{
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("XdgDesktopPortalInputClient")
            .field("transport", &self.transport)
            .field("remote_desktop_session", &self.remote_desktop_session)
            .field("input_capture_session", &self.input_capture_session)
            .field("eis_connection", &self.eis_connection)
            .field("eis_decoder_configured", &self.eis_decoder.is_some())
            .finish()
    }
}

impl<T> XdgDesktopPortalInputClient<T>
where
    T: XdgDesktopPortalInputTransport,
{
    /// Create a client over a portal transport.
    #[must_use]
    pub fn new(transport: T) -> Self {
        Self {
            transport: Arc::new(transport),
            remote_desktop_session: Arc::new(tokio::sync::Mutex::new(None)),
            input_capture_session: Arc::new(tokio::sync::Mutex::new(None)),
            eis_connection: Arc::new(tokio::sync::Mutex::new(None)),
            eis_decoder: None,
        }
    }

    /// Create a client over a portal transport and EIS event decoder.
    #[must_use]
    pub fn with_event_decoder(transport: T, decoder: impl PortalEisEventDecoder + 'static) -> Self {
        Self {
            transport: Arc::new(transport),
            remote_desktop_session: Arc::new(tokio::sync::Mutex::new(None)),
            input_capture_session: Arc::new(tokio::sync::Mutex::new(None)),
            eis_connection: Arc::new(tokio::sync::Mutex::new(None)),
            eis_decoder: Some(Arc::new(decoder)),
        }
    }

    /// Borrow the transport for diagnostics/tests.
    #[must_use]
    pub fn transport(&self) -> &T {
        &self.transport
    }

    /// Current EIS connection marker, if capture has been granted.
    pub async fn eis_connection(&self) -> Option<PortalEisConnection> {
        self.eis_connection.lock().await.clone()
    }

    /// Configure pointer barriers and enable InputCapture triggers.
    ///
    /// # Errors
    /// Returns [`InputError`] when no capture session exists, zones are empty,
    /// any barrier is rejected, or the portal call fails.
    pub async fn configure_pointer_barriers(
        &self,
        barriers: Vec<PortalPointerBarrier>,
    ) -> Result<PortalZoneSet, InputError> {
        let session = self
            .input_capture_session
            .lock()
            .await
            .clone()
            .ok_or(InputError::PermissionDenied)?;
        let zones = self.transport.get_zones(&session).await?;
        if zones.zones.is_empty() {
            return Err(InputError::Backend(
                "InputCapture returned no zones for pointer barriers".into(),
            ));
        }
        let failed = self
            .transport
            .set_pointer_barriers(&session, &barriers, zones.id)
            .await?;
        if !failed.is_empty() {
            return Err(InputError::Backend(format!(
                "InputCapture rejected pointer barriers: {failed:?}"
            )));
        }
        self.transport.enable(&session).await?;
        Ok(zones)
    }

    /// Configure one barrier on the right edge of the first InputCapture zone.
    ///
    /// # Errors
    /// Returns [`InputError`] when no capture session exists, no zones are
    /// available, the zone is empty, or the portal rejects the barrier.
    pub async fn configure_first_zone_right_edge_barrier(
        &self,
    ) -> Result<PortalZoneSet, InputError> {
        let session = self
            .input_capture_session
            .lock()
            .await
            .clone()
            .ok_or(InputError::PermissionDenied)?;
        let zones = self.transport.get_zones(&session).await?;
        let zone = zones.zones.first().copied().ok_or_else(|| {
            InputError::Backend("InputCapture returned no zones for pointer barriers".into())
        })?;
        if zone.width == 0 || zone.height == 0 {
            return Err(InputError::Backend(
                "InputCapture returned an empty first zone".into(),
            ));
        }
        let x = zone.x + i32::try_from(zone.width.saturating_sub(1)).unwrap_or(i32::MAX);
        let y2 = zone.y + i32::try_from(zone.height.saturating_sub(1)).unwrap_or(i32::MAX);
        let barrier = PortalPointerBarrier {
            id: 1,
            x1: x,
            y1: zone.y,
            x2: x,
            y2,
        };
        let failed = self
            .transport
            .set_pointer_barriers(&session, &[barrier], zones.id)
            .await?;
        if !failed.is_empty() {
            return Err(InputError::Backend(format!(
                "InputCapture rejected pointer barriers: {failed:?}"
            )));
        }
        self.transport.enable(&session).await?;
        Ok(zones)
    }
}

#[async_trait]
impl<T> WaylandPortalInputClient for XdgDesktopPortalInputClient<T>
where
    T: XdgDesktopPortalInputTransport,
{
    async fn request_input_session(
        &self,
        required: PortalInputGrant,
    ) -> Result<PortalInputGrant, InputError> {
        if required.remote_desktop {
            let session = self.transport.open_remote_desktop().await?;
            *self.remote_desktop_session.lock().await = Some(session);
        }
        if required.input_capture {
            let eis = self.transport.open_input_capture().await?;
            *self.input_capture_session.lock().await = Some(eis.session_handle.clone());
            *self.eis_connection.lock().await = Some(eis);
        }

        Ok(required)
    }

    async fn inject(&self, command: InjectionCommand) -> Result<(), InputError> {
        let session = self
            .remote_desktop_session
            .lock()
            .await
            .clone()
            .ok_or(InputError::PermissionDenied)?;
        self.transport
            .notify(&session, notify_method_for_command(command)?)
            .await
    }

    async fn next_event(&self) -> Result<InputEvent, InputError> {
        let connection = self
            .eis_connection
            .lock()
            .await
            .clone()
            .ok_or(InputError::PermissionDenied)?;
        let decoder = self.eis_decoder.as_ref().ok_or_else(|| {
            InputError::Backend("libei input event decoder is not configured".into())
        })?;
        decoder.next_event(&connection).await
    }
}

fn notify_method_for_command(command: InjectionCommand) -> Result<PortalNotifyMethod, InputError> {
    match command {
        InjectionCommand::MoveAbsolute { x, y } => {
            Ok(PortalNotifyMethod::PointerMotionAbsolute { stream: 0, x, y })
        }
        InjectionCommand::MoveRelative { dx, dy } => {
            Ok(PortalNotifyMethod::PointerMotion { dx, dy })
        }
        InjectionCommand::MoveRaw { dx, dy } => Ok(PortalNotifyMethod::PointerMotion {
            dx: f64::from(dx),
            dy: f64::from(dy),
        }),
        InjectionCommand::Button { button, pressed } => Ok(PortalNotifyMethod::PointerButton {
            button: match button {
                nexkvm_input::MouseButton::Left => crate::inject::btn_code::LEFT as i32,
                nexkvm_input::MouseButton::Right => crate::inject::btn_code::RIGHT as i32,
                nexkvm_input::MouseButton::Middle => crate::inject::btn_code::MIDDLE as i32,
            },
            state: u32::from(pressed),
        }),
        InjectionCommand::Scroll { dx, dy } => Ok(PortalNotifyMethod::PointerAxis { dx, dy }),
        InjectionCommand::Key { keycode, pressed } => {
            let keycode = i32::try_from(keycode)
                .map_err(|_| InputError::Backend(format!("Linux keycode too large: {keycode}")))?;
            Ok(PortalNotifyMethod::KeyboardKeycode {
                keycode,
                state: u32::from(pressed),
            })
        }
    }
}

#[cfg(target_os = "linux")]
fn reis_event_to_input(event: reis::event::EiEvent) -> Result<Option<InputEvent>, InputError> {
    let input = match event {
        reis::event::EiEvent::PointerMotion(event) => Some(InputEvent::RelativeMove {
            dx: f64::from(event.dx),
            dy: f64::from(event.dy),
        }),
        reis::event::EiEvent::PointerMotionAbsolute(event) => {
            let (x, y) = event
                .device
                .dimensions()
                .filter(|(width, height)| *width > 0 && *height > 0)
                .map_or(
                    (f64::from(event.dx_absolute), f64::from(event.dy_absolute)),
                    |(width, height)| {
                        (
                            f64::from(event.dx_absolute) / f64::from(width),
                            f64::from(event.dy_absolute) / f64::from(height),
                        )
                    },
                );
            Some(InputEvent::PointerMove { x, y })
        }
        reis::event::EiEvent::Button(event) => {
            let Some(button) = mouse_button_from_evdev(event.button) else {
                return Ok(None);
            };
            match event.state {
                reis::ei::button::ButtonState::Press => Some(InputEvent::ButtonPress(button)),
                reis::ei::button::ButtonState::Released => Some(InputEvent::ButtonRelease(button)),
            }
        }
        reis::event::EiEvent::ScrollDelta(event) => Some(InputEvent::Scroll {
            dx: f64::from(event.dx),
            dy: f64::from(event.dy),
        }),
        reis::event::EiEvent::ScrollDiscrete(event) => Some(InputEvent::Scroll {
            dx: f64::from(event.discrete_dx),
            dy: f64::from(event.discrete_dy),
        }),
        reis::event::EiEvent::KeyboardKey(event) => match event.state {
            reis::ei::keyboard::KeyState::Press => Some(InputEvent::KeyPress(event.key)),
            reis::ei::keyboard::KeyState::Released => Some(InputEvent::KeyRelease(event.key)),
        },
        reis::event::EiEvent::Disconnected(event) => {
            return Err(InputError::Backend(format!(
                "portal EIS disconnected: {:?} {}",
                event.reason,
                event.explanation.unwrap_or_default()
            )));
        }
        reis::event::EiEvent::SeatAdded(_)
        | reis::event::EiEvent::SeatRemoved(_)
        | reis::event::EiEvent::DeviceAdded(_)
        | reis::event::EiEvent::DeviceRemoved(_)
        | reis::event::EiEvent::DevicePaused(_)
        | reis::event::EiEvent::DeviceResumed(_)
        | reis::event::EiEvent::KeyboardModifiers(_)
        | reis::event::EiEvent::Frame(_)
        | reis::event::EiEvent::DeviceStartEmulating(_)
        | reis::event::EiEvent::DeviceStopEmulating(_)
        | reis::event::EiEvent::ScrollStop(_)
        | reis::event::EiEvent::ScrollCancel(_)
        | reis::event::EiEvent::TouchDown(_)
        | reis::event::EiEvent::TouchUp(_)
        | reis::event::EiEvent::TouchMotion(_)
        | reis::event::EiEvent::TouchCancel(_)
        | reis::event::EiEvent::TextKeysym(_)
        | reis::event::EiEvent::TextUtf8(_) => None,
    };
    Ok(input)
}

#[cfg(target_os = "linux")]
fn mouse_button_from_evdev(button: u32) -> Option<nexkvm_input::MouseButton> {
    let button = u16::try_from(button).ok()?;
    match button {
        crate::inject::btn_code::LEFT => Some(nexkvm_input::MouseButton::Left),
        crate::inject::btn_code::RIGHT => Some(nexkvm_input::MouseButton::Right),
        crate::inject::btn_code::MIDDLE => Some(nexkvm_input::MouseButton::Middle),
        _ => None,
    }
}

fn portal_options<'a>() -> HashMap<&'a str, Value<'a>> {
    HashMap::new()
}

fn portal_barriers(
    barriers: &[PortalPointerBarrier],
) -> Vec<HashMap<&'static str, Value<'static>>> {
    barriers
        .iter()
        .map(|barrier| {
            let mut values = HashMap::new();
            values.insert("barrier_id", Value::from(barrier.id));
            values.insert(
                "position",
                Value::from((barrier.x1, barrier.y1, barrier.x2, barrier.y2)),
            );
            values
        })
        .collect()
}

fn take_portal_result<T>(
    results: &mut HashMap<String, OwnedValue>,
    key: &'static str,
) -> Result<T, InputError>
where
    T: TryFrom<OwnedValue>,
    T::Error: std::fmt::Display,
{
    let value = results
        .remove(key)
        .ok_or_else(|| InputError::Backend(format!("portal response missing `{key}`")))?;
    value
        .try_into()
        .map_err(|error| InputError::Backend(format!("portal response `{key}`: {error}")))
}

fn optional_portal_result<T>(
    results: &mut HashMap<String, OwnedValue>,
    key: &'static str,
) -> Result<T, InputError>
where
    T: TryFrom<OwnedValue> + Default,
    T::Error: std::fmt::Display,
{
    match results.remove(key) {
        Some(value) => value
            .try_into()
            .map_err(|error| InputError::Backend(format!("portal response `{key}`: {error}"))),
        None => Ok(T::default()),
    }
}

fn portal_token(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);
    let next = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}_{next}")
}

fn portal_call_error(
    operation: &'static str,
) -> impl FnOnce(zbus::Error) -> InputError + Send + Sync + 'static {
    move |error| InputError::Backend(format!("{operation}: {error}"))
}

/// Daemon-facing Wayland input adapter backed by a portal client.
#[derive(Debug)]
pub struct LinuxWaylandPortalInput<C> {
    client: C,
    grant: PortalInputGrant,
}

impl<C> LinuxWaylandPortalInput<C>
where
    C: WaylandPortalInputClient,
{
    /// Open a Wayland portal input session.
    ///
    /// # Errors
    /// Returns [`InputError::PermissionDenied`] when the session lacks the
    /// required portal interfaces or the compositor grant is incomplete.
    pub async fn connect(portals: PortalAvailability, client: C) -> Result<Self, InputError> {
        if !portals.desktop || !portals.remote_desktop || !portals.input_capture {
            return Err(InputError::PermissionDenied);
        }

        let required = PortalInputGrant::REQUIRED;
        let grant = client.request_input_session(required).await?;
        if !grant.satisfies(required) {
            return Err(InputError::PermissionDenied);
        }

        Ok(Self { client, grant })
    }

    /// Borrow the portal client for observability/testing.
    #[must_use]
    pub const fn client(&self) -> &C {
        &self.client
    }

    /// Portal grants for this session.
    #[must_use]
    pub const fn grant(&self) -> PortalInputGrant {
        self.grant
    }
}

#[async_trait]
impl<C> InputInjector for LinuxWaylandPortalInput<C>
where
    C: WaylandPortalInputClient,
{
    async fn inject(&self, event: InputEvent) -> Result<(), InputError> {
        if !self.grant.remote_desktop {
            return Err(InputError::PermissionDenied);
        }
        self.client.inject(event.to_injection_command()).await
    }
}

#[async_trait]
impl<C> InputCapture for LinuxWaylandPortalInput<C>
where
    C: WaylandPortalInputClient,
{
    async fn next_event(&self) -> Result<InputEvent, InputError> {
        if !self.grant.input_capture {
            return Err(InputError::PermissionDenied);
        }
        self.client.next_event().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexkvm_input::MouseButton;
    use std::sync::Mutex;

    #[derive(Debug, Default)]
    struct RecordingTransport {
        opened_remote_desktop: Mutex<usize>,
        opened_input_capture: Mutex<usize>,
        notifications: Mutex<Vec<(String, PortalNotifyMethod)>>,
        zones_requested: Mutex<Vec<String>>,
        barriers_set: Mutex<Vec<(String, Vec<PortalPointerBarrier>, u32)>>,
        enabled: Mutex<Vec<String>>,
    }

    #[derive(Debug, Default)]
    struct QueueEisDecoder {
        events: Mutex<Vec<InputEvent>>,
    }

    #[async_trait]
    impl PortalEisEventDecoder for QueueEisDecoder {
        async fn next_event(
            &self,
            _connection: &PortalEisConnection,
        ) -> Result<InputEvent, InputError> {
            self.events
                .lock()
                .expect("poisoned")
                .pop()
                .ok_or_else(|| InputError::Backend("empty EIS queue".into()))
        }
    }

    #[async_trait]
    impl XdgDesktopPortalInputTransport for RecordingTransport {
        async fn open_remote_desktop(&self) -> Result<String, InputError> {
            *self.opened_remote_desktop.lock().expect("poisoned") += 1;
            Ok("/org/freedesktop/portal/desktop/session/nexkvm/remote".into())
        }

        async fn open_input_capture(&self) -> Result<PortalEisConnection, InputError> {
            *self.opened_input_capture.lock().expect("poisoned") += 1;
            Ok(PortalEisConnection {
                session_handle: "/org/freedesktop/portal/desktop/session/nexkvm/capture".into(),
                handle: "eis-fd".into(),
                fd: None,
            })
        }

        async fn notify(
            &self,
            session_handle: &str,
            method: PortalNotifyMethod,
        ) -> Result<(), InputError> {
            self.notifications
                .lock()
                .expect("poisoned")
                .push((session_handle.into(), method));
            Ok(())
        }

        async fn get_zones(&self, session_handle: &str) -> Result<PortalZoneSet, InputError> {
            self.zones_requested
                .lock()
                .expect("poisoned")
                .push(session_handle.into());
            Ok(PortalZoneSet {
                id: 77,
                zones: vec![PortalInputZone {
                    width: 1920,
                    height: 1080,
                    x: 0,
                    y: 0,
                }],
            })
        }

        async fn set_pointer_barriers(
            &self,
            session_handle: &str,
            barriers: &[PortalPointerBarrier],
            zone_set: u32,
        ) -> Result<Vec<u32>, InputError> {
            self.barriers_set.lock().expect("poisoned").push((
                session_handle.into(),
                barriers.to_vec(),
                zone_set,
            ));
            Ok(Vec::new())
        }

        async fn enable(&self, session_handle: &str) -> Result<(), InputError> {
            self.enabled
                .lock()
                .expect("poisoned")
                .push(session_handle.into());
            Ok(())
        }
    }

    #[tokio::test]
    async fn xdg_client_opens_remote_desktop_and_input_capture_sessions() {
        let client = XdgDesktopPortalInputClient::new(RecordingTransport::default());
        let grant = client
            .request_input_session(PortalInputGrant {
                remote_desktop: true,
                input_capture: true,
            })
            .await
            .expect("session");

        assert_eq!(
            grant,
            PortalInputGrant {
                remote_desktop: true,
                input_capture: true,
            }
        );
        assert_eq!(
            client.eis_connection().await,
            Some(PortalEisConnection {
                session_handle: "/org/freedesktop/portal/desktop/session/nexkvm/capture".into(),
                handle: "eis-fd".into(),
                fd: None,
            })
        );
        assert_eq!(
            *client
                .transport()
                .opened_remote_desktop
                .lock()
                .expect("poisoned"),
            1
        );
        assert_eq!(
            *client
                .transport()
                .opened_input_capture
                .lock()
                .expect("poisoned"),
            1
        );
    }

    #[tokio::test]
    async fn xdg_client_maps_input_events_to_remote_desktop_notify_methods() {
        let client = XdgDesktopPortalInputClient::new(RecordingTransport::default());
        client
            .request_input_session(PortalInputGrant {
                remote_desktop: true,
                input_capture: false,
            })
            .await
            .expect("session");

        client
            .inject(InjectionCommand::MoveAbsolute { x: 0.25, y: 0.75 })
            .await
            .expect("absolute pointer");
        client
            .inject(InjectionCommand::Button {
                button: MouseButton::Left,
                pressed: true,
            })
            .await
            .expect("button");
        client
            .inject(InjectionCommand::Key {
                keycode: 0x04,
                pressed: false,
            })
            .await
            .expect("key");

        let notifications = client
            .transport()
            .notifications
            .lock()
            .expect("poisoned")
            .clone();
        assert_eq!(
            notifications,
            vec![
                (
                    "/org/freedesktop/portal/desktop/session/nexkvm/remote".into(),
                    PortalNotifyMethod::PointerMotionAbsolute {
                        stream: 0,
                        x: 0.25,
                        y: 0.75,
                    },
                ),
                (
                    "/org/freedesktop/portal/desktop/session/nexkvm/remote".into(),
                    PortalNotifyMethod::PointerButton {
                        button: crate::inject::btn_code::LEFT as i32,
                        state: 1,
                    },
                ),
                (
                    "/org/freedesktop/portal/desktop/session/nexkvm/remote".into(),
                    PortalNotifyMethod::KeyboardKeycode {
                        keycode: 0x04,
                        state: 0,
                    },
                ),
            ]
        );
    }

    #[tokio::test]
    async fn xdg_client_configures_pointer_barriers_before_enabling_capture() {
        let client = XdgDesktopPortalInputClient::new(RecordingTransport::default());
        client
            .request_input_session(PortalInputGrant {
                remote_desktop: false,
                input_capture: true,
            })
            .await
            .expect("session");

        let requested = vec![PortalPointerBarrier {
            id: 1,
            x1: 1919,
            y1: 0,
            x2: 1919,
            y2: 1079,
        }];
        let zones = client
            .configure_pointer_barriers(requested.clone())
            .await
            .expect("barriers");

        assert_eq!(
            zones,
            PortalZoneSet {
                id: 77,
                zones: vec![PortalInputZone {
                    width: 1920,
                    height: 1080,
                    x: 0,
                    y: 0,
                }],
            }
        );
        assert_eq!(
            client
                .transport()
                .zones_requested
                .lock()
                .expect("poisoned")
                .as_slice(),
            &["/org/freedesktop/portal/desktop/session/nexkvm/capture".to_string()]
        );
        assert_eq!(
            client
                .transport()
                .barriers_set
                .lock()
                .expect("poisoned")
                .as_slice(),
            &[(
                "/org/freedesktop/portal/desktop/session/nexkvm/capture".to_string(),
                requested,
                77,
            )]
        );
        assert_eq!(
            client
                .transport()
                .enabled
                .lock()
                .expect("poisoned")
                .as_slice(),
            &["/org/freedesktop/portal/desktop/session/nexkvm/capture".to_string()]
        );
    }

    #[tokio::test]
    async fn xdg_client_decodes_capture_events_from_eis_connection() {
        let decoder = QueueEisDecoder {
            events: Mutex::new(vec![InputEvent::RelativeMove { dx: 2.0, dy: -1.0 }]),
        };
        let client =
            XdgDesktopPortalInputClient::with_event_decoder(RecordingTransport::default(), decoder);
        client
            .request_input_session(PortalInputGrant {
                remote_desktop: false,
                input_capture: true,
            })
            .await
            .expect("session");

        assert_eq!(
            client.next_event().await.expect("event"),
            InputEvent::RelativeMove { dx: 2.0, dy: -1.0 }
        );
    }

    #[tokio::test]
    async fn reis_decoder_requires_portal_eis_fd() {
        let decoder = ReisPortalEisEventDecoder;
        let error = decoder
            .next_event(&PortalEisConnection {
                session_handle: "/org/freedesktop/portal/desktop/session/nexkvm/capture".into(),
                handle: "eis-fd".into(),
                fd: None,
            })
            .await
            .expect_err("missing fd should fail");

        assert!(matches!(error, InputError::Backend(message) if message.contains("EIS fd")));
    }

    #[tokio::test]
    async fn xdg_client_configures_first_zone_right_edge_barrier() {
        let client = XdgDesktopPortalInputClient::new(RecordingTransport::default());
        client
            .request_input_session(PortalInputGrant {
                remote_desktop: false,
                input_capture: true,
            })
            .await
            .expect("session");

        let zones = client
            .configure_first_zone_right_edge_barrier()
            .await
            .expect("right edge barrier");

        assert_eq!(zones.id, 77);
        assert_eq!(
            client
                .transport()
                .barriers_set
                .lock()
                .expect("poisoned")
                .as_slice(),
            &[(
                "/org/freedesktop/portal/desktop/session/nexkvm/capture".to_string(),
                vec![PortalPointerBarrier {
                    id: 1,
                    x1: 1919,
                    y1: 0,
                    x2: 1919,
                    y2: 1079,
                }],
                77,
            )]
        );
    }
}
