//! Windows platform backend.
//!
//! Real integration uses Win32: low-level hooks (`SetWindowsHookEx`) for
//! capture, `SendInput` for injection, and the clipboard API. Note that
//! User Interface Privilege Isolation (UIPI) can block injection into windows
//! owned by higher-integrity (elevated) processes; nexkvm surfaces this via
//! capabilities rather than failing silently.
//!
//! Compiled only on Windows; an empty library elsewhere.

#![cfg(target_os = "windows")]

use async_trait::async_trait;
use nexkvm_core::platform::{PlatformBackend, PlatformCapabilities};
use nexkvm_core::{CoreError, OsKind};

pub mod capture;
pub mod clipboard;
pub mod inject;

pub use capture::WindowsInputCapture;
pub use clipboard::WindowsClipboard;
pub use inject::WindowsInputInjector;

/// Windows implementation of [`PlatformBackend`].
#[derive(Debug, Default)]
pub struct WindowsBackend;

impl WindowsBackend {
    /// Create the backend.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl PlatformBackend for WindowsBackend {
    fn os(&self) -> OsKind {
        OsKind::Windows
    }

    fn capabilities(&self) -> PlatformCapabilities {
        // Windows generally permits these without a prompt; the real impl
        // verifies hook installation and UIPI constraints at runtime.
        PlatformCapabilities {
            can_inject_input: true,
            can_capture_input: true,
            can_access_clipboard: true,
            permission_pending: false,
        }
    }

    async fn request_permissions(&self) -> Result<PlatformCapabilities, CoreError> {
        Ok(self.capabilities())
    }
}
