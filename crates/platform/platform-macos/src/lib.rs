//! macOS platform backend.
//!
//! Input capture/injection requires **Accessibility** trust, which this backend
//! can query and prompt for. Clipboard text read/write is available via
//! [`MacosClipboard`], while richer pasteboard formats and display capture
//! continue to land behind the same [`PlatformBackend`] boundary in later
//! phases.
//!
//! Compiled only on macOS; on other targets this crate is an empty library so
//! the workspace builds everywhere.

#![cfg(target_os = "macos")]

use async_trait::async_trait;
use nexkvm_core::platform::{PlatformBackend, PlatformCapabilities};
use nexkvm_core::{CoreError, OsKind};

mod accessibility;
pub mod capture;
pub mod clipboard;
pub mod inject;
pub mod permissions;

pub use capture::MacosInputCapture;
pub use clipboard::MacosClipboard;
pub use inject::MacosInputInjector;
pub use permissions::{MacosInputPermissionReport, MacosPermissionState};

/// macOS implementation of [`PlatformBackend`].
#[derive(Debug)]
pub struct MacosBackend {
    accessibility: Box<dyn accessibility::AccessibilityStatus>,
}

impl MacosBackend {
    /// Create the backend.
    #[must_use]
    pub fn new() -> Self {
        Self {
            accessibility: Box::new(accessibility::SystemAccessibility),
        }
    }

    /// Create a backend with an injected Accessibility status provider.
    #[cfg(test)]
    #[must_use]
    fn with_accessibility_status(
        accessibility: impl accessibility::AccessibilityStatus + 'static,
    ) -> Self {
        Self {
            accessibility: Box::new(accessibility),
        }
    }

    /// Report macOS input permission readiness for diagnostics.
    #[must_use]
    pub fn input_permission_report(&self) -> MacosInputPermissionReport {
        permissions::input_permission_report(self.accessibility.as_ref())
    }
}

impl Default for MacosBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PlatformBackend for MacosBackend {
    fn os(&self) -> OsKind {
        OsKind::MacOs
    }

    fn capabilities(&self) -> PlatformCapabilities {
        capabilities_from_accessibility(self.accessibility.is_trusted())
    }

    async fn request_permissions(&self) -> Result<PlatformCapabilities, CoreError> {
        Ok(capabilities_from_accessibility(
            self.accessibility.prompt_and_check(),
        ))
    }
}

fn capabilities_from_accessibility(accessibility_trusted: bool) -> PlatformCapabilities {
    PlatformCapabilities {
        can_inject_input: accessibility_trusted,
        can_capture_input: accessibility_trusted,
        can_access_clipboard: true,
        permission_pending: !accessibility_trusted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, Copy)]
    struct StubAccessibility {
        trusted_before_prompt: bool,
        trusted_after_prompt: bool,
    }

    impl accessibility::AccessibilityStatus for StubAccessibility {
        fn is_trusted(&self) -> bool {
            self.trusted_before_prompt
        }

        fn prompt_and_check(&self) -> bool {
            self.trusted_after_prompt
        }
    }

    #[test]
    fn capabilities_are_pending_until_accessibility_is_trusted() {
        let backend = MacosBackend::with_accessibility_status(StubAccessibility {
            trusted_before_prompt: false,
            trusted_after_prompt: false,
        });

        assert_eq!(
            backend.capabilities(),
            PlatformCapabilities {
                can_inject_input: false,
                can_capture_input: false,
                can_access_clipboard: true,
                permission_pending: true,
            }
        );
    }

    #[test]
    fn capabilities_enable_input_when_accessibility_is_trusted() {
        let backend = MacosBackend::with_accessibility_status(StubAccessibility {
            trusted_before_prompt: true,
            trusted_after_prompt: true,
        });

        assert_eq!(
            backend.capabilities(),
            PlatformCapabilities {
                can_inject_input: true,
                can_capture_input: true,
                can_access_clipboard: true,
                permission_pending: false,
            }
        );
    }

    #[tokio::test]
    async fn request_permissions_prompts_and_refreshes_capabilities() {
        let backend = MacosBackend::with_accessibility_status(StubAccessibility {
            trusted_before_prompt: false,
            trusted_after_prompt: true,
        });

        assert_eq!(
            backend.request_permissions().await.unwrap(),
            PlatformCapabilities {
                can_inject_input: true,
                can_capture_input: true,
                can_access_clipboard: true,
                permission_pending: false,
            }
        );
    }
}
