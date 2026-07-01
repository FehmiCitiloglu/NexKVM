//! Linux platform backend.
//!
//! Linux support has to account for two display worlds:
//! - **Wayland**: native global input is intentionally forbidden. nexkvm must use
//!   compositor-mediated portals (`RemoteDesktop`, `InputCapture`) and PipeWire
//!   portal streams for screen/audio-related flows. Support varies by GNOME,
//!   KDE/KWin, and wlroots compositors.
//! - **X11**: permissive legacy fallback through XInput2/XRecord + XTEST and
//!   selection-based clipboard access.
//!
//! This crate implements the runtime capability resolver and compatibility
//! model. Actual D-Bus portal and X11 FFI bindings land behind the same safe
//! [`PlatformBackend`] boundary in later phases; no blocking OS call is made on
//! async paths here.

#![allow(clashing_extern_declarations)]

use async_trait::async_trait;
use nexkvm_core::platform::{PlatformBackend, PlatformCapabilities};
use nexkvm_core::{CoreError, OsKind};

pub mod clipboard;
pub mod inject;
pub mod pipewire_audio;
pub mod pipewire_screen;
pub mod portal_input;

pub use clipboard::LinuxClipboard;
pub use pipewire_audio::{
    NativePipeWireAudioGraph, PIPEWIRE_INTERFACE_NODE, PipeWireAudioBackend, PipeWireAudioGraph,
    PipeWireAudioGraphSnapshot, PipeWireAudioNode, PipeWireAudioStream, PipeWireRegistryCollector,
    PipeWireRegistryGlobal, StaticPipeWireAudioGraph, StaticPipeWireAudioStream,
    UnsupportedPipeWireAudioStream,
};
pub use pipewire_screen::{
    LinuxPipeWireScreenCapture, NativePipeWireFrameReader, PendingPipeWireFrameReader,
    PipeWireFrameFormat, PipeWireFrameReader, PipeWireFrameRequest, PipeWireMappedBuffer,
    PipeWireRawFrame, PipeWireRemoteFd, PipeWireScreenCastSession, PipeWireScreenCastStream,
    PipeWireSpaRawVideoInfo, PipeWireStreamTarget, PipeWireVideoFormat, SPA_PARAM_FORMAT,
    SPA_VIDEO_FORMAT_BGRA, SPA_VIDEO_FORMAT_BGRX, SPA_VIDEO_FORMAT_NV12, SPA_VIDEO_FORMAT_RGBA,
    SPA_VIDEO_FORMAT_RGBX, XdgDesktopPortalScreenCastTransport,
    ZbusXdgDesktopPortalScreenCastTransport,
};
pub use portal_input::{
    LinuxWaylandPortalInput, PortalEisConnection, PortalEisEventDecoder, PortalEisFd,
    PortalInputGrant, PortalInputZone, PortalNotifyMethod, PortalPointerBarrier, PortalZoneSet,
    ReisPortalEisEventDecoder, WaylandPortalInputClient, XdgDesktopPortalInputClient,
    XdgDesktopPortalInputTransport, ZbusXdgDesktopPortalInputTransport,
};

/// Display/session family detected for Linux.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinuxSessionKind {
    /// Native Wayland session (`XDG_SESSION_TYPE=wayland` or `WAYLAND_DISPLAY`).
    Wayland,
    /// X11 session (`XDG_SESSION_TYPE=x11` or `DISPLAY`).
    X11,
    /// No graphical display variables are present.
    Headless,
    /// Display variables exist but do not identify a supported stack.
    Unknown,
}

/// Known Linux desktop/compositor families that affect portal behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopEnvironment {
    /// GNOME / Mutter.
    Gnome,
    /// KDE Plasma / KWin.
    Kde,
    /// wlroots ecosystem without a more specific compositor name.
    Wlroots,
    /// Sway compositor.
    Sway,
    /// Hyprland compositor.
    Hyprland,
    /// COSMIC desktop.
    Cosmic,
    /// Unknown desktop/compositor.
    Unknown,
}

impl DesktopEnvironment {
    /// Parse common desktop/compositor environment values.
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        let value = raw.to_ascii_lowercase();
        if value.contains("gnome") {
            Self::Gnome
        } else if value.contains("kde") || value.contains("plasma") {
            Self::Kde
        } else if value.contains("sway") {
            Self::Sway
        } else if value.contains("hyprland") {
            Self::Hyprland
        } else if value.contains("wlroots") {
            Self::Wlroots
        } else if value.contains("cosmic") {
            Self::Cosmic
        } else {
            Self::Unknown
        }
    }
}

/// Portal/interface availability relevant to native Wayland support.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PortalAvailability {
    /// `org.freedesktop.portal.Desktop` is expected to be reachable.
    pub desktop: bool,
    /// `RemoteDesktop` portal for compositor-mediated input injection.
    pub remote_desktop: bool,
    /// `InputCapture` portal for edge capture/suppression.
    pub input_capture: bool,
    /// `ScreenCast` portal, backed by PipeWire streams.
    pub screen_cast: bool,
    /// PipeWire user session appears available.
    pub pipewire: bool,
    /// Clipboard/data-control integration is expected for this compositor.
    pub clipboard: bool,
}

impl PortalAvailability {
    /// Whether the Wayland-native input path has enough pieces for full KVM use.
    #[must_use]
    pub const fn has_full_wayland_input(self) -> bool {
        self.desktop && self.remote_desktop && self.input_capture
    }

    /// Whether PipeWire portal support is available for screen/audio flows.
    #[must_use]
    pub const fn has_pipewire_portal(self) -> bool {
        self.desktop && self.screen_cast && self.pipewire
    }
}

/// X11 fallback availability detected for the current process environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum X11Fallback {
    /// No X11 display is visible.
    Unavailable,
    /// Native X11 session.
    Native,
    /// A Wayland session also exposes `DISPLAY` (typically XWayland). Useful as
    /// a legacy compatibility path, but not equivalent to compositor-native
    /// Wayland input capture.
    XWayland,
}

/// Coarse compatibility guidance for desktop-specific Wayland integration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompatibilityLevel {
    /// Expected full support after portal grants.
    Full,
    /// Useful subset is available; some UX may degrade.
    Partial,
    /// Legacy fallback path, generally X11.
    Fallback,
    /// No useful support detected.
    Unsupported,
}

/// Detailed Linux capability report for UI, diagnostics, and feature gating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinuxCapabilityDetails {
    /// Current display/session kind.
    pub session: LinuxSessionKind,
    /// Desktop/compositor family.
    pub desktop: DesktopEnvironment,
    /// Native Wayland portal availability.
    pub portals: PortalAvailability,
    /// X11 fallback status.
    pub x11_fallback: X11Fallback,
    /// GNOME compatibility level.
    pub gnome: CompatibilityLevel,
    /// KDE compatibility level.
    pub kde: CompatibilityLevel,
    /// Cross-platform summary exposed through [`PlatformBackend`].
    pub platform: PlatformCapabilities,
}

/// Linux audio stack capability report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinuxAudioCapabilities {
    /// PipeWire is expected to be available for user-session audio graph access.
    pub pipewire_available: bool,
    /// Access should go through an xdg-desktop-portal mediated flow.
    pub portal_required: bool,
    /// Can route playback between devices through PipeWire nodes/links.
    pub can_route_between_devices: bool,
    /// Can switch the default playback/capture device.
    pub can_switch_devices: bool,
    /// Can follow the active controlled device with audio routing.
    pub can_follow_mouse: bool,
    /// Can share a headset endpoint bidirectionally.
    pub can_share_headset: bool,
}

/// Linux handheld form factor detected from environment/platform hints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinuxHandheldKind {
    /// Valve Steam Deck / SteamOS gaming mode or desktop mode.
    SteamDeck,
    /// Other handheld gaming PC.
    GenericHandheld,
    /// Standard desktop/laptop form factor.
    Desktop,
}

/// Capability hints for Linux handheld support.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinuxHandheldCapabilities {
    /// Detected form factor.
    pub kind: LinuxHandheldKind,
    /// Whether gamepad-first input surfaces should be preferred.
    pub prefer_gamepad_navigation: bool,
    /// Whether touchscreen affordances should be enabled.
    pub touchscreen_likely: bool,
    /// Whether virtual keyboard fallback should be available.
    pub virtual_keyboard_likely: bool,
    /// Whether SteamOS gaming mode compatibility constraints apply.
    pub steam_game_mode: bool,
}

impl LinuxCapabilityDetails {
    /// Whether native Wayland integration is usable for the current session.
    #[must_use]
    pub const fn native_wayland_ready(&self) -> bool {
        matches!(self.session, LinuxSessionKind::Wayland) && self.portals.has_full_wayland_input()
    }

    /// Whether PipeWire portal support is usable.
    #[must_use]
    pub const fn pipewire_portal_ready(&self) -> bool {
        self.portals.has_pipewire_portal()
    }
}

/// Environment snapshot used by the Linux resolver.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinuxEnvironment {
    /// `XDG_SESSION_TYPE`.
    pub xdg_session_type: Option<String>,
    /// `WAYLAND_DISPLAY`.
    pub wayland_display: Option<String>,
    /// `DISPLAY`.
    pub display: Option<String>,
    /// `XDG_CURRENT_DESKTOP`.
    pub xdg_current_desktop: Option<String>,
    /// `XDG_SESSION_DESKTOP`.
    pub xdg_session_desktop: Option<String>,
    /// `DESKTOP_SESSION`.
    pub desktop_session: Option<String>,
    /// `PIPEWIRE_REMOTE`, when present.
    pub pipewire_remote: Option<String>,
    /// Optional explicit test/runtime override for desktop portal availability.
    pub portal_desktop: Option<bool>,
    /// Optional explicit test/runtime override for RemoteDesktop portal support.
    pub portal_remote_desktop: Option<bool>,
    /// Optional explicit test/runtime override for InputCapture portal support.
    pub portal_input_capture: Option<bool>,
    /// Optional explicit test/runtime override for ScreenCast portal support.
    pub portal_screen_cast: Option<bool>,
    /// Optional explicit test/runtime override for clipboard portal/data-control.
    pub portal_clipboard: Option<bool>,
    /// Optional explicit form-factor hint, e.g. `steam-deck` or `handheld`.
    pub handheld_kind: Option<String>,
    /// Optional SteamOS/gamescope mode hint.
    pub steam_game_mode: Option<bool>,
}

impl LinuxEnvironment {
    /// Capture environment variables from the current process.
    #[must_use]
    pub fn detect() -> Self {
        Self {
            xdg_session_type: std::env::var("XDG_SESSION_TYPE").ok(),
            wayland_display: std::env::var("WAYLAND_DISPLAY").ok(),
            display: std::env::var("DISPLAY").ok(),
            xdg_current_desktop: std::env::var("XDG_CURRENT_DESKTOP").ok(),
            xdg_session_desktop: std::env::var("XDG_SESSION_DESKTOP").ok(),
            desktop_session: std::env::var("DESKTOP_SESSION").ok(),
            pipewire_remote: std::env::var("PIPEWIRE_REMOTE").ok(),
            portal_desktop: parse_bool_env("NEXKVM_PORTAL_DESKTOP"),
            portal_remote_desktop: parse_bool_env("NEXKVM_PORTAL_REMOTE_DESKTOP"),
            portal_input_capture: parse_bool_env("NEXKVM_PORTAL_INPUT_CAPTURE"),
            portal_screen_cast: parse_bool_env("NEXKVM_PORTAL_SCREENCAST"),
            portal_clipboard: parse_bool_env("NEXKVM_PORTAL_CLIPBOARD"),
            handheld_kind: std::env::var("NEXKVM_LINUX_HANDHELD").ok(),
            steam_game_mode: parse_bool_env("STEAM_GAME_MODE")
                .or_else(|| parse_bool_env("NEXKVM_STEAM_GAME_MODE")),
        }
    }

    fn session(&self) -> LinuxSessionKind {
        match self
            .xdg_session_type
            .as_deref()
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("wayland") => LinuxSessionKind::Wayland,
            Some("x11") => LinuxSessionKind::X11,
            Some(_) => LinuxSessionKind::Unknown,
            None if self.wayland_display.is_some() => LinuxSessionKind::Wayland,
            None if self.display.is_some() => LinuxSessionKind::X11,
            None => LinuxSessionKind::Headless,
        }
    }

    fn desktop(&self) -> DesktopEnvironment {
        [
            self.xdg_current_desktop.as_deref(),
            self.xdg_session_desktop.as_deref(),
            self.desktop_session.as_deref(),
        ]
        .into_iter()
        .flatten()
        .map(DesktopEnvironment::parse)
        .find(|desktop| *desktop != DesktopEnvironment::Unknown)
        .unwrap_or(DesktopEnvironment::Unknown)
    }

    fn x11_fallback(&self) -> X11Fallback {
        match (self.session(), self.display.is_some()) {
            (LinuxSessionKind::X11, true) => X11Fallback::Native,
            (LinuxSessionKind::Wayland, true) => X11Fallback::XWayland,
            _ => X11Fallback::Unavailable,
        }
    }
}

/// Linux implementation of [`PlatformBackend`].
#[derive(Debug, Clone)]
pub struct LinuxBackend {
    env: LinuxEnvironment,
}

impl LinuxBackend {
    /// Create the backend using the current process environment.
    #[must_use]
    pub fn new() -> Self {
        Self {
            env: LinuxEnvironment::detect(),
        }
    }

    /// Create a backend from an injected environment snapshot. Intended for
    /// tests and deterministic diagnostics.
    #[must_use]
    pub fn with_environment(env: LinuxEnvironment) -> Self {
        Self { env }
    }

    /// Detailed Linux-specific capability report.
    #[must_use]
    pub fn capability_details(&self) -> LinuxCapabilityDetails {
        let session = self.env.session();
        let desktop = self.env.desktop();
        let x11_fallback = self.env.x11_fallback();
        let portals = resolve_portals(session, desktop, &self.env);
        let platform = summarize_capabilities(session, portals, x11_fallback);

        LinuxCapabilityDetails {
            session,
            desktop,
            portals,
            x11_fallback,
            gnome: compatibility_for(DesktopEnvironment::Gnome, desktop, session, portals),
            kde: compatibility_for(DesktopEnvironment::Kde, desktop, session, portals),
            platform,
        }
    }

    /// Linux-specific audio capability report.
    #[must_use]
    pub fn audio_capabilities(&self) -> LinuxAudioCapabilities {
        let details = self.capability_details();
        let pipewire_available = match details.session {
            LinuxSessionKind::Wayland => details.portals.has_pipewire_portal(),
            LinuxSessionKind::X11 => details.portals.pipewire || self.env.pipewire_remote.is_some(),
            LinuxSessionKind::Headless | LinuxSessionKind::Unknown => false,
        };
        let portal_required = matches!(details.session, LinuxSessionKind::Wayland);

        LinuxAudioCapabilities {
            pipewire_available,
            portal_required,
            can_route_between_devices: pipewire_available,
            can_switch_devices: pipewire_available,
            can_follow_mouse: pipewire_available && details.platform.can_inject_input,
            can_share_headset: pipewire_available,
        }
    }

    /// Linux handheld compatibility report for Steam Deck-style devices.
    #[must_use]
    pub fn handheld_capabilities(&self) -> LinuxHandheldCapabilities {
        let kind = handheld_kind(&self.env);
        let steam_game_mode = self.env.steam_game_mode.unwrap_or(false)
            || matches!(kind, LinuxHandheldKind::SteamDeck)
                && self
                    .env
                    .desktop_session
                    .as_deref()
                    .is_some_and(|session| session.to_ascii_lowercase().contains("gamescope"));

        LinuxHandheldCapabilities {
            kind,
            prefer_gamepad_navigation: matches!(
                kind,
                LinuxHandheldKind::SteamDeck | LinuxHandheldKind::GenericHandheld
            ),
            touchscreen_likely: matches!(
                kind,
                LinuxHandheldKind::SteamDeck | LinuxHandheldKind::GenericHandheld
            ),
            virtual_keyboard_likely: matches!(
                kind,
                LinuxHandheldKind::SteamDeck | LinuxHandheldKind::GenericHandheld
            ),
            steam_game_mode,
        }
    }
}

impl Default for LinuxBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PlatformBackend for LinuxBackend {
    fn os(&self) -> OsKind {
        OsKind::Linux
    }

    fn capabilities(&self) -> PlatformCapabilities {
        self.capability_details().platform
    }

    async fn request_permissions(&self) -> Result<PlatformCapabilities, CoreError> {
        let details = self.capability_details();
        match details.session {
            LinuxSessionKind::Wayland if details.portals.has_full_wayland_input() => {
                Ok(PlatformCapabilities {
                    permission_pending: false,
                    ..details.platform
                })
            }
            LinuxSessionKind::Wayland if details.portals.desktop => Ok(details.platform),
            LinuxSessionKind::Wayland => Err(CoreError::Unsupported(
                "Wayland session requires xdg-desktop-portal RemoteDesktop/InputCapture support",
            )),
            LinuxSessionKind::X11 => Ok(details.platform),
            LinuxSessionKind::Headless | LinuxSessionKind::Unknown => {
                Err(CoreError::Unsupported("no supported Linux display session"))
            }
        }
    }
}

fn resolve_portals(
    session: LinuxSessionKind,
    desktop: DesktopEnvironment,
    env: &LinuxEnvironment,
) -> PortalAvailability {
    if !matches!(session, LinuxSessionKind::Wayland) {
        return PortalAvailability::default();
    }

    let defaults = default_portals_for_desktop(desktop);
    PortalAvailability {
        desktop: env.portal_desktop.unwrap_or(defaults.desktop),
        remote_desktop: env.portal_remote_desktop.unwrap_or(defaults.remote_desktop),
        input_capture: env.portal_input_capture.unwrap_or(defaults.input_capture),
        screen_cast: env.portal_screen_cast.unwrap_or(defaults.screen_cast),
        pipewire: env.pipewire_remote.is_some() || defaults.pipewire,
        clipboard: env.portal_clipboard.unwrap_or(defaults.clipboard),
    }
}

fn default_portals_for_desktop(desktop: DesktopEnvironment) -> PortalAvailability {
    match desktop {
        // GNOME/Mutter has strong RemoteDesktop/ScreenCast portal support.
        // InputCapture is newer and version-sensitive, so default to pending
        // unless explicitly probed/overridden.
        DesktopEnvironment::Gnome => PortalAvailability {
            desktop: true,
            remote_desktop: true,
            input_capture: false,
            screen_cast: true,
            pipewire: true,
            clipboard: true,
        },
        // KDE/KWin has mature xdg-desktop-portal-kde support, but KVM-style
        // capture availability is still compositor/version dependent.
        DesktopEnvironment::Kde => PortalAvailability {
            desktop: true,
            remote_desktop: true,
            input_capture: false,
            screen_cast: true,
            pipewire: true,
            clipboard: true,
        },
        // wlroots compositors vary by installed portal backend. Be conservative
        // for input but allow screen cast/clipboard expectations where portals
        // are commonly present.
        DesktopEnvironment::Sway | DesktopEnvironment::Hyprland | DesktopEnvironment::Wlroots => {
            PortalAvailability {
                desktop: true,
                remote_desktop: false,
                input_capture: false,
                screen_cast: true,
                pipewire: true,
                clipboard: true,
            }
        }
        DesktopEnvironment::Cosmic | DesktopEnvironment::Unknown => PortalAvailability::default(),
    }
}

fn summarize_capabilities(
    session: LinuxSessionKind,
    portals: PortalAvailability,
    x11_fallback: X11Fallback,
) -> PlatformCapabilities {
    match session {
        LinuxSessionKind::X11 => PlatformCapabilities {
            can_inject_input: true,
            can_capture_input: true,
            can_access_clipboard: true,
            permission_pending: false,
        },
        LinuxSessionKind::Wayland if portals.has_full_wayland_input() => PlatformCapabilities {
            can_inject_input: true,
            can_capture_input: true,
            can_access_clipboard: portals.clipboard,
            permission_pending: true,
        },
        LinuxSessionKind::Wayland => PlatformCapabilities {
            can_inject_input: portals.remote_desktop,
            can_capture_input: portals.input_capture,
            can_access_clipboard: portals.clipboard || x11_fallback != X11Fallback::Unavailable,
            permission_pending: portals.desktop,
        },
        LinuxSessionKind::Headless | LinuxSessionKind::Unknown => PlatformCapabilities::none(),
    }
}

fn compatibility_for(
    target: DesktopEnvironment,
    detected: DesktopEnvironment,
    session: LinuxSessionKind,
    portals: PortalAvailability,
) -> CompatibilityLevel {
    if detected != target {
        return CompatibilityLevel::Unsupported;
    }
    match session {
        LinuxSessionKind::Wayland if portals.has_full_wayland_input() => CompatibilityLevel::Full,
        LinuxSessionKind::Wayland if portals.desktop && portals.remote_desktop => {
            CompatibilityLevel::Partial
        }
        LinuxSessionKind::X11 => CompatibilityLevel::Fallback,
        LinuxSessionKind::Wayland | LinuxSessionKind::Headless | LinuxSessionKind::Unknown => {
            CompatibilityLevel::Unsupported
        }
    }
}

fn handheld_kind(env: &LinuxEnvironment) -> LinuxHandheldKind {
    let hint = env
        .handheld_kind
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let desktop = env
        .desktop_session
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if hint.contains("steam") || hint.contains("deck") || desktop.contains("gamescope") {
        LinuxHandheldKind::SteamDeck
    } else if hint.contains("handheld") || hint.contains("gaming") {
        LinuxHandheldKind::GenericHandheld
    } else {
        LinuxHandheldKind::Desktop
    }
}

fn parse_bool_env(name: &str) -> Option<bool> {
    std::env::var(name)
        .ok()
        .and_then(|value| parse_bool(&value))
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(session: &str, desktop: &str) -> LinuxEnvironment {
        LinuxEnvironment {
            xdg_session_type: Some(session.into()),
            xdg_current_desktop: Some(desktop.into()),
            ..LinuxEnvironment::default()
        }
    }

    #[test]
    fn detects_wayland_gnome_with_pipewire_portal() {
        let mut env = env("wayland", "GNOME");
        env.pipewire_remote = Some("pipewire-0".into());
        let details = LinuxBackend::with_environment(env).capability_details();
        assert_eq!(details.session, LinuxSessionKind::Wayland);
        assert_eq!(details.desktop, DesktopEnvironment::Gnome);
        assert!(details.portals.remote_desktop);
        assert!(details.pipewire_portal_ready());
        assert_eq!(details.gnome, CompatibilityLevel::Partial);
    }

    #[test]
    fn explicit_input_capture_enables_full_native_wayland() {
        let mut env = env("wayland", "KDE");
        env.portal_input_capture = Some(true);
        let details = LinuxBackend::with_environment(env).capability_details();
        assert!(details.native_wayland_ready());
        assert_eq!(details.kde, CompatibilityLevel::Full);
        assert!(details.platform.can_inject_input);
        assert!(details.platform.can_capture_input);
        assert!(details.platform.permission_pending);
    }

    #[tokio::test]
    async fn permission_request_marks_granted_wayland_portals_ready() {
        let mut env = env("wayland", "KDE");
        env.portal_input_capture = Some(true);
        let capabilities = LinuxBackend::with_environment(env)
            .request_permissions()
            .await
            .expect("permission request");

        assert!(capabilities.can_inject_input);
        assert!(capabilities.can_capture_input);
        assert!(!capabilities.permission_pending);
    }

    #[test]
    fn x11_session_is_full_legacy_fallback() {
        let mut env = env("x11", "KDE");
        env.display = Some(":0".into());
        let details = LinuxBackend::with_environment(env).capability_details();
        assert_eq!(details.session, LinuxSessionKind::X11);
        assert_eq!(details.x11_fallback, X11Fallback::Native);
        assert_eq!(details.kde, CompatibilityLevel::Fallback);
        assert_eq!(
            details.platform,
            PlatformCapabilities {
                can_inject_input: true,
                can_capture_input: true,
                can_access_clipboard: true,
                permission_pending: false,
            }
        );
    }

    #[test]
    fn wayland_with_display_marks_xwayland_fallback() {
        let mut env = env("wayland", "GNOME");
        env.display = Some(":1".into());
        let details = LinuxBackend::with_environment(env).capability_details();
        assert_eq!(details.x11_fallback, X11Fallback::XWayland);
        assert!(details.platform.can_access_clipboard);
    }

    #[test]
    fn wayland_pipewire_audio_supports_follow_mouse_when_input_available() {
        let mut env = env("wayland", "GNOME");
        env.portal_input_capture = Some(true);
        let audio = LinuxBackend::with_environment(env).audio_capabilities();
        assert!(audio.pipewire_available);
        assert!(audio.portal_required);
        assert!(audio.can_route_between_devices);
        assert!(audio.can_switch_devices);
        assert!(audio.can_follow_mouse);
        assert!(audio.can_share_headset);
    }

    #[test]
    fn x11_pipewire_audio_does_not_require_portal() {
        let mut env = env("x11", "KDE");
        env.display = Some(":0".into());
        env.pipewire_remote = Some("pipewire-0".into());
        let audio = LinuxBackend::with_environment(env).audio_capabilities();
        assert!(audio.pipewire_available);
        assert!(!audio.portal_required);
        assert!(audio.can_switch_devices);
        assert!(audio.can_follow_mouse);
    }

    #[tokio::test]
    async fn pipewire_audio_backend_maps_audio_nodes_to_devices() {
        use crate::{
            PipeWireAudioBackend, PipeWireAudioGraphSnapshot, PipeWireAudioNode,
            StaticPipeWireAudioGraph,
        };
        use nexkvm_streaming::{AudioBackend, AudioDeviceRole, AudioFormat};

        let backend =
            PipeWireAudioBackend::new(StaticPipeWireAudioGraph::new(PipeWireAudioGraphSnapshot {
                nodes: vec![
                    PipeWireAudioNode::new(41)
                        .with_property("media.class", "Audio/Sink")
                        .with_property("node.name", "alsa_output.pci-0000_00_1f.3")
                        .with_property("node.description", "Built-in Speakers")
                        .with_default(true),
                    PipeWireAudioNode::new(42)
                        .with_property("media.class", "Audio/Source")
                        .with_property("node.name", "alsa_input.pci-0000_00_1f.3")
                        .with_property("node.description", "Built-in Microphone"),
                    PipeWireAudioNode::new(77)
                        .with_property("media.class", "Stream/Output/Audio")
                        .with_property("node.name", "browser"),
                ],
            }));

        let devices = backend.devices().await.unwrap();
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].id.0, "pipewire-node:41");
        assert_eq!(devices[0].label, "Built-in Speakers");
        assert_eq!(devices[0].role, AudioDeviceRole::Playback);
        assert!(devices[0].is_default);
        assert_eq!(devices[1].id.0, "pipewire-node:42");
        assert_eq!(devices[1].role, AudioDeviceRole::Capture);
        assert_eq!(backend.preferred_format(), AudioFormat::opus_stereo_48k());
    }

    #[tokio::test]
    async fn pipewire_audio_backend_switches_playback_by_pipewire_node_id() {
        use crate::{
            PipeWireAudioBackend, PipeWireAudioGraphSnapshot, PipeWireAudioNode,
            StaticPipeWireAudioGraph,
        };
        use nexkvm_streaming::{AudioBackend, AudioDeviceId};

        let backend =
            PipeWireAudioBackend::new(StaticPipeWireAudioGraph::new(PipeWireAudioGraphSnapshot {
                nodes: vec![PipeWireAudioNode::new(41).with_property("media.class", "Audio/Sink")],
            }));

        backend
            .switch_playback_device(&AudioDeviceId::new("pipewire-node:41"))
            .await
            .unwrap();
        assert!(
            backend
                .switch_playback_device(&AudioDeviceId::new("pipewire-node:not-a-node"))
                .await
                .is_err()
        );
        assert!(
            backend
                .switch_playback_device(&AudioDeviceId::new("alsa:41"))
                .await
                .is_err()
        );
    }

    #[test]
    fn headless_has_no_capabilities() {
        let details =
            LinuxBackend::with_environment(LinuxEnvironment::default()).capability_details();
        assert_eq!(details.session, LinuxSessionKind::Headless);
        assert_eq!(details.platform, PlatformCapabilities::none());
        let audio =
            LinuxBackend::with_environment(LinuxEnvironment::default()).audio_capabilities();
        assert!(!audio.pipewire_available);
        assert!(!audio.can_route_between_devices);
    }

    #[test]
    fn desktop_parser_handles_common_names() {
        assert_eq!(
            DesktopEnvironment::parse("GNOME"),
            DesktopEnvironment::Gnome
        );
        assert_eq!(
            DesktopEnvironment::parse("KDE:Plasma"),
            DesktopEnvironment::Kde
        );
        assert_eq!(DesktopEnvironment::parse("sway"), DesktopEnvironment::Sway);
        assert_eq!(
            DesktopEnvironment::parse("Hyprland"),
            DesktopEnvironment::Hyprland
        );
    }

    #[test]
    fn detects_steam_deck_handheld_profile() {
        let mut env = env("wayland", "gamescope");
        env.handheld_kind = Some("steam-deck".into());
        env.steam_game_mode = Some(true);
        let handheld = LinuxBackend::with_environment(env).handheld_capabilities();
        assert_eq!(handheld.kind, LinuxHandheldKind::SteamDeck);
        assert!(handheld.prefer_gamepad_navigation);
        assert!(handheld.touchscreen_likely);
        assert!(handheld.virtual_keyboard_likely);
        assert!(handheld.steam_game_mode);
    }

    #[derive(Debug, Default)]
    struct RecordingPortalClient {
        requests: std::sync::Mutex<Vec<PortalInputGrant>>,
        injected: std::sync::Mutex<Vec<nexkvm_input::InjectionCommand>>,
        events: std::sync::Mutex<Vec<nexkvm_input::InputEvent>>,
        grant: PortalInputGrant,
    }

    #[async_trait]
    impl WaylandPortalInputClient for RecordingPortalClient {
        async fn request_input_session(
            &self,
            required: PortalInputGrant,
        ) -> Result<PortalInputGrant, nexkvm_input::InputError> {
            self.requests.lock().expect("poisoned").push(required);
            Ok(self.grant)
        }

        async fn inject(
            &self,
            command: nexkvm_input::InjectionCommand,
        ) -> Result<(), nexkvm_input::InputError> {
            self.injected.lock().expect("poisoned").push(command);
            Ok(())
        }

        async fn next_event(&self) -> Result<nexkvm_input::InputEvent, nexkvm_input::InputError> {
            self.events
                .lock()
                .expect("poisoned")
                .pop()
                .ok_or_else(|| nexkvm_input::InputError::Backend("empty portal queue".into()))
        }
    }

    #[tokio::test]
    async fn wayland_portal_input_session_injects_and_captures() {
        use nexkvm_input::{InputCapture, InputEvent, InputInjector, MouseButton};

        let client = RecordingPortalClient {
            grant: PortalInputGrant {
                remote_desktop: true,
                input_capture: true,
            },
            events: std::sync::Mutex::new(vec![InputEvent::ButtonPress(MouseButton::Left)]),
            ..RecordingPortalClient::default()
        };
        let input = LinuxWaylandPortalInput::connect(
            PortalAvailability {
                desktop: true,
                remote_desktop: true,
                input_capture: true,
                screen_cast: false,
                pipewire: false,
                clipboard: false,
            },
            client,
        )
        .await
        .expect("portal session");

        input
            .inject(InputEvent::PointerMove { x: 0.4, y: 0.6 })
            .await
            .expect("inject");
        assert_eq!(
            input.client().injected.lock().expect("poisoned").as_slice(),
            &[nexkvm_input::InjectionCommand::MoveAbsolute { x: 0.4, y: 0.6 }]
        );
        assert_eq!(
            input.next_event().await.expect("capture"),
            InputEvent::ButtonPress(MouseButton::Left)
        );
        assert_eq!(
            input.client().requests.lock().expect("poisoned").as_slice(),
            &[PortalInputGrant {
                remote_desktop: true,
                input_capture: true,
            }]
        );
    }

    #[tokio::test]
    async fn wayland_portal_input_requires_input_capture_portal() {
        let result = LinuxWaylandPortalInput::connect(
            PortalAvailability {
                desktop: true,
                remote_desktop: true,
                input_capture: false,
                screen_cast: false,
                pipewire: false,
                clipboard: false,
            },
            RecordingPortalClient::default(),
        )
        .await;

        assert!(matches!(
            result,
            Err(nexkvm_input::InputError::PermissionDenied)
        ));
    }
}
