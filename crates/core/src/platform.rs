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
