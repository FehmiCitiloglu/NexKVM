//! macOS platform backend.
//!
//! Real input/clipboard/display integration uses native APIs (`CGEventTap` for
//! capture, `CGEventPost` for injection, `NSPasteboard` for clipboard) and
//! requires **Accessibility** (and **Screen Recording** for display capture)
//! permission, prompted on first use. Those FFI calls are introduced in a later
//! phase; the foundation provides the [`MacosBackend`] skeleton implementing the
//! cross-platform [`PlatformBackend`] contract.
//!
//! Compiled only on macOS; on other targets this crate is an empty library so
//! the workspace builds everywhere.

#![cfg(target_os = "macos")]

use async_trait::async_trait;
use coklu_core::platform::{PlatformBackend, PlatformCapabilities};
use coklu_core::{CoreError, OsKind};

/// macOS implementation of [`PlatformBackend`].
#[derive(Debug, Default)]
pub struct MacosBackend;

impl MacosBackend {
    /// Create the backend.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl PlatformBackend for MacosBackend {
    fn os(&self) -> OsKind {
        OsKind::MacOs
    }

    fn capabilities(&self) -> PlatformCapabilities {
        // Until Accessibility permission is wired up, report capture/inject as
        // pending so higher layers prompt before relying on them.
        PlatformCapabilities {
            can_inject_input: false,
            can_capture_input: false,
            can_access_clipboard: false,
            permission_pending: true,
        }
    }

    async fn request_permissions(&self) -> Result<PlatformCapabilities, CoreError> {
        // Phase placeholder: a later phase triggers the Accessibility prompt via
        // `AXIsProcessTrustedWithOptions` and re-resolves capabilities.
        Err(CoreError::Unsupported(
            "macOS permission flow not yet implemented",
        ))
    }
}
