//! macOS clipboard backend.
//!
//! This backend provides multi-format clipboard read/write via NSPasteboard FFI,
//! supporting text, HTML, RTF, and image formats with transparent MIME-to-UTI
//! mapping for cross-platform synchronization.

use async_trait::async_trait;
use nexkvm_clipboard::{Clipboard, ClipboardError, ClipboardSnapshot};
use std::fmt;

use super::pasteboard;

/// macOS clipboard adapter implementing [`Clipboard`] via NSPasteboard.
///
/// Supports multi-format clipboard content (text, HTML, RTF, images) by mapping
/// between NSPasteboard UTI types and standard MIME types. Reads and writes all
/// available formats to preserve rich clipboard content across devices.
#[derive(Clone)]
pub struct MacosClipboard {
    #[allow(dead_code)]
    _phantom: std::marker::PhantomData<()>,
}

impl MacosClipboard {
    /// Create a clipboard adapter bound to the native macOS pasteboard.
    #[must_use]
    pub fn new() -> Self {
        Self {
            _phantom: std::marker::PhantomData,
        }
    }
}

impl Default for MacosClipboard {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for MacosClipboard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MacosClipboard").finish_non_exhaustive()
    }
}

#[async_trait]
impl Clipboard for MacosClipboard {
    async fn read(&self) -> Result<Option<ClipboardSnapshot>, ClipboardError> {
        tokio::task::spawn_blocking(pasteboard::read_pasteboard)
            .await
            .map_err(|e| ClipboardError::Backend(format!("clipboard read task failed: {e}")))?
    }

    async fn write(&self, snapshot: ClipboardSnapshot) -> Result<(), ClipboardError> {
        tokio::task::spawn_blocking(move || pasteboard::write_pasteboard(&snapshot))
            .await
            .map_err(|e| ClipboardError::Backend(format!("clipboard write task failed: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    // Note: Full roundtrip tests require system clipboard access and a functioning
    // macOS environment, so we omit them from the unit test suite. Integration tests
    // can verify clipboard synchronization end-to-end when run on a full system.
    //
    // The NSPasteboard FFI implementation is tested via unit tests in pasteboard.rs.

    #[test]
    fn macro_test_to_detect_compile_time_issues() {
        // This test ensures the clipboard module compiles correctly.
        // It doesn't require system access.
    }
}
