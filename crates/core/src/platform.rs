//! Cross-platform backend boundary.
//!
//! Each `platform-*` crate provides a [`PlatformBackend`] implementation. Higher
//! layers program against this trait only, so OS-specific `unsafe` FFI stays
//! isolated behind a safe interface (per the project's unsafe policy).
//!
//! # Platform reality (flagged early)
//! Input/clipboard/display access requires native APIs and, on most platforms,
//! explicit user permission:
//! - **macOS**: Accessibility permission for input synthesis/capture; Screen
//!   Recording for display capture. Prompted on first use.
//! - **Linux/Wayland**: no global input injection by design — must go through
//!   compositor portals (`xdg-desktop-portal`, `libei`/InputCapture). X11 is
//!   permissive but legacy. [`PlatformCapabilities`] lets callers detect this.
//! - **Windows**: raw input + `SendInput`; UIPI may block injection into
//!   elevated windows.
//!
//! [`PlatformCapabilities`] is queried at startup so features degrade
//! gracefully instead of failing hard on restricted platforms.

use async_trait::async_trait;

use crate::error::CoreError;
use crate::identity::OsKind;

/// What a given platform backend can actually do, resolved at runtime.
///
/// Values reflect both the OS and the *current session* (e.g. Wayland vs X11,
/// or whether Accessibility permission has been granted).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlatformCapabilities {
    /// Can synthesize input events (move pointer, press keys) globally.
    pub can_inject_input: bool,
    /// Can capture global input events.
    pub can_capture_input: bool,
    /// Can read/write the system clipboard.
    pub can_access_clipboard: bool,
    /// Whether an OS permission prompt is still pending for the above.
    pub permission_pending: bool,
}

impl PlatformCapabilities {
    /// A conservative "nothing available yet" baseline.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            can_inject_input: false,
            can_capture_input: false,
            can_access_clipboard: false,
            permission_pending: false,
        }
    }
}

/// Native OS integration surfaces that product features can depend on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeIntegration {
    /// Global input capture from the local OS.
    InputCapture,
    /// Global input injection into the local OS.
    InputInjection,
    /// System clipboard read/write.
    Clipboard,
    /// Screen capture through the local display stack.
    ScreenCapture,
    /// Audio route/device control through the local audio stack.
    AudioRouting,
    /// Hardware or OS media encoding.
    MediaEncoding,
}

impl NativeIntegration {
    /// Stable lowercase label for diagnostics.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::InputCapture => "input-capture",
            Self::InputInjection => "input-injection",
            Self::Clipboard => "clipboard",
            Self::ScreenCapture => "screen-capture",
            Self::AudioRouting => "audio-routing",
            Self::MediaEncoding => "media-encoding",
        }
    }
}

/// Runtime status of a native integration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeIntegrationStatus {
    /// The integration is available for this process/session.
    Available,
    /// The OS can plausibly grant this integration, but permission is pending.
    PermissionRequired,
    /// The current backend/session does not support this integration yet.
    Unsupported,
}

impl NativeIntegrationStatus {
    /// Stable lowercase label for diagnostics.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::PermissionRequired => "permission-required",
            Self::Unsupported => "unsupported",
        }
    }
}

/// One native integration status entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeIntegrationAvailability {
    /// Integration surface.
    pub integration: NativeIntegration,
    /// Runtime status.
    pub status: NativeIntegrationStatus,
}

/// Runtime native integration report for diagnostics and feature gating.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeIntegrationReport {
    /// OS family the report applies to.
    pub os: OsKind,
    /// Per-integration status entries.
    pub integrations: Vec<NativeIntegrationAvailability>,
}

impl NativeIntegrationReport {
    /// Build a cross-platform report from the backend capability summary.
    #[must_use]
    pub fn from_capabilities(os: OsKind, capabilities: PlatformCapabilities) -> Self {
        let input_capture = input_status(
            capabilities.can_capture_input,
            capabilities.permission_pending,
        );
        let input_injection = input_status(
            capabilities.can_inject_input,
            capabilities.permission_pending,
        );

        Self {
            os,
            integrations: vec![
                NativeIntegrationAvailability {
                    integration: NativeIntegration::InputCapture,
                    status: input_capture,
                },
                NativeIntegrationAvailability {
                    integration: NativeIntegration::InputInjection,
                    status: input_injection,
                },
                NativeIntegrationAvailability {
                    integration: NativeIntegration::Clipboard,
                    status: if capabilities.can_access_clipboard {
                        NativeIntegrationStatus::Available
                    } else {
                        NativeIntegrationStatus::Unsupported
                    },
                },
                NativeIntegrationAvailability {
                    integration: NativeIntegration::ScreenCapture,
                    status: NativeIntegrationStatus::Unsupported,
                },
                NativeIntegrationAvailability {
                    integration: NativeIntegration::AudioRouting,
                    status: NativeIntegrationStatus::Unsupported,
                },
                NativeIntegrationAvailability {
                    integration: NativeIntegration::MediaEncoding,
                    status: NativeIntegrationStatus::Unsupported,
                },
            ],
        }
    }

    /// Return the status for one integration surface.
    #[must_use]
    pub fn status(&self, integration: NativeIntegration) -> Option<NativeIntegrationStatus> {
        self.integrations
            .iter()
            .find(|entry| entry.integration == integration)
            .map(|entry| entry.status)
    }
}

fn input_status(available: bool, permission_pending: bool) -> NativeIntegrationStatus {
    if available {
        NativeIntegrationStatus::Available
    } else if permission_pending {
        NativeIntegrationStatus::PermissionRequired
    } else {
        NativeIntegrationStatus::Unsupported
    }
}

/// The OS integration surface implemented per platform.
///
/// Async because acquiring capabilities may involve awaiting a permission
/// prompt or portal negotiation. Implementations must keep blocking OS calls
/// off the async runtime (use `spawn_blocking` where needed).
#[async_trait]
pub trait PlatformBackend: Send + Sync {
    /// Which OS family this backend targets.
    fn os(&self) -> OsKind;

    /// Resolve the capabilities available in the current session.
    fn capabilities(&self) -> PlatformCapabilities;

    /// Request any OS permissions required for input/clipboard access.
    ///
    /// May trigger a system prompt. Returns the (possibly updated) capabilities
    /// after the request resolves.
    ///
    /// # Errors
    /// Returns [`CoreError::Unsupported`] if the platform cannot grant the
    /// capability at all (e.g. headless session).
    async fn request_permissions(&self) -> Result<PlatformCapabilities, CoreError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_integration_report_marks_available_capabilities() {
        let report = NativeIntegrationReport::from_capabilities(
            OsKind::Linux,
            PlatformCapabilities {
                can_inject_input: true,
                can_capture_input: true,
                can_access_clipboard: true,
                permission_pending: false,
            },
        );

        assert_eq!(report.os, OsKind::Linux);
        assert_eq!(
            report.status(NativeIntegration::InputInjection),
            Some(NativeIntegrationStatus::Available)
        );
        assert_eq!(
            report.status(NativeIntegration::InputCapture),
            Some(NativeIntegrationStatus::Available)
        );
        assert_eq!(
            report.status(NativeIntegration::Clipboard),
            Some(NativeIntegrationStatus::Available)
        );
    }

    #[test]
    fn native_integration_report_marks_permission_required_input() {
        let report = NativeIntegrationReport::from_capabilities(
            OsKind::MacOs,
            PlatformCapabilities {
                can_inject_input: false,
                can_capture_input: false,
                can_access_clipboard: false,
                permission_pending: true,
            },
        );

        assert_eq!(
            report.status(NativeIntegration::InputInjection),
            Some(NativeIntegrationStatus::PermissionRequired)
        );
        assert_eq!(
            report.status(NativeIntegration::InputCapture),
            Some(NativeIntegrationStatus::PermissionRequired)
        );
        assert_eq!(
            report.status(NativeIntegration::Clipboard),
            Some(NativeIntegrationStatus::Unsupported)
        );
    }
}
