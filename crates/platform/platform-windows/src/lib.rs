//! Windows platform backend.
//!
//! Real integration uses Win32: low-level hooks (`SetWindowsHookEx`) / Raw Input
//! for capture, `SendInput` for injection, and the clipboard API. Note that
//! User Interface Privilege Isolation (UIPI) can block injection into windows
//! owned by higher-integrity (elevated) processes; coklu surfaces this via
//! capabilities rather than failing silently.
//!
//! FFI lands in a later phase; this is the skeleton. Compiled only on Windows;
//! an empty library elsewhere.

#![cfg(target_os = "windows")]

use async_trait::async_trait;
use coklu_core::platform::{PlatformBackend, PlatformCapabilities};
use coklu_core::{CoreError, OsKind};

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
            can_inject_input: false,
            can_capture_input: false,
            can_access_clipboard: false,
            permission_pending: false,
        }
    }

    async fn request_permissions(&self) -> Result<PlatformCapabilities, CoreError> {
        Err(CoreError::Unsupported(
            "Windows backend not yet implemented",
        ))
    }
}
