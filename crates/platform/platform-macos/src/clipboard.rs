//! macOS clipboard backend.
//!
//! This MVP backend provides text clipboard read/write via native `pbpaste` /
//! `pbcopy` tools that bridge to `NSPasteboard` on macOS. It is intentionally
//! limited to UTF-8 text while the richer multi-format mapping lands.

use async_trait::async_trait;
use nexkvm_clipboard::{Clipboard, ClipboardError, ClipboardSnapshot};
use std::fmt;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Arc;

trait PasteboardIo: Send + Sync {
    fn read_text(&self) -> Result<Option<String>, ClipboardError>;
    fn write_text(&self, text: String) -> Result<(), ClipboardError>;
}

#[derive(Debug, Clone, Copy, Default)]
struct NativePasteboard;

impl PasteboardIo for NativePasteboard {
    fn read_text(&self) -> Result<Option<String>, ClipboardError> {
        let output = Command::new("pbpaste")
            .output()
            .map_err(|e| ClipboardError::Backend(format!("pbpaste launch failed: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(ClipboardError::Backend(format!(
                "pbpaste failed (status={}): {}",
                output.status,
                stderr.trim()
            )));
        }

        if output.stdout.is_empty() {
            return Ok(None);
        }

        let text = String::from_utf8(output.stdout)
            .map_err(|e| ClipboardError::Backend(format!("pbpaste returned non-utf8 data: {e}")))?;
        Ok(Some(text))
    }

    fn write_text(&self, text: String) -> Result<(), ClipboardError> {
        let mut child = Command::new("pbcopy")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| ClipboardError::Backend(format!("pbcopy launch failed: {e}")))?;

        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| ClipboardError::Backend("pbcopy stdin unavailable".into()))?;
        stdin
            .write_all(text.as_bytes())
            .map_err(|e| ClipboardError::Backend(format!("pbcopy stdin write failed: {e}")))?;
        drop(stdin);

        let output = child
            .wait_with_output()
            .map_err(|e| ClipboardError::Backend(format!("pbcopy wait failed: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(ClipboardError::Backend(format!(
                "pbcopy failed (status={}): {}",
                output.status,
                stderr.trim()
            )));
        }

        Ok(())
    }
}

/// macOS clipboard adapter implementing [`Clipboard`].
///
/// Notes:
/// - Reads/writes UTF-8 text only in this phase.
/// - Binary/rich formats (HTML, RTF, images, files) are returned as
///   [`ClipboardError::Unsupported`] on write and ignored on read.
#[derive(Clone)]
pub struct MacosClipboard {
    io: Arc<dyn PasteboardIo>,
}

impl MacosClipboard {
    /// Create a clipboard adapter bound to the native macOS pasteboard.
    #[must_use]
    pub fn new() -> Self {
        Self {
            io: Arc::new(NativePasteboard),
        }
    }

    #[cfg(test)]
    fn with_io(io: Arc<dyn PasteboardIo>) -> Self {
        Self { io }
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
        let io = Arc::clone(&self.io);
        tokio::task::spawn_blocking(move || io.read_text())
            .await
            .map_err(|e| ClipboardError::Backend(format!("clipboard read task failed: {e}")))?
            .map(|text| text.map(ClipboardSnapshot::from_text))
    }

    async fn write(&self, snapshot: ClipboardSnapshot) -> Result<(), ClipboardError> {
        let Some(text) = snapshot.best_text().map(str::to_owned) else {
            return Err(ClipboardError::Unsupported(
                "macos clipboard backend currently writes text formats only",
            ));
        };

        let io = Arc::clone(&self.io);
        tokio::task::spawn_blocking(move || io.write_text(text))
            .await
            .map_err(|e| ClipboardError::Backend(format!("clipboard write task failed: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexkvm_clipboard::{ClipboardContent, ClipboardSnapshot};
    use std::sync::Mutex;

    #[derive(Debug, Default)]
    struct FakePasteboard {
        read_text: Mutex<Option<String>>,
        written_text: Mutex<Option<String>>,
    }

    impl FakePasteboard {
        fn with_read_text(text: Option<&str>) -> Self {
            Self {
                read_text: Mutex::new(text.map(ToOwned::to_owned)),
                written_text: Mutex::new(None),
            }
        }

        fn written(&self) -> Option<String> {
            self.written_text.lock().expect("poisoned").clone()
        }
    }

    impl PasteboardIo for FakePasteboard {
        fn read_text(&self) -> Result<Option<String>, ClipboardError> {
            Ok(self.read_text.lock().expect("poisoned").clone())
        }

        fn write_text(&self, text: String) -> Result<(), ClipboardError> {
            *self.written_text.lock().expect("poisoned") = Some(text);
            Ok(())
        }
    }

    #[tokio::test]
    async fn read_text_maps_to_snapshot() {
        let fake = Arc::new(FakePasteboard::with_read_text(Some("hello")));
        let clipboard = MacosClipboard::with_io(fake);

        let snapshot = clipboard.read().await.unwrap().expect("snapshot");
        assert_eq!(snapshot.best_text(), Some("hello"));
    }

    #[tokio::test]
    async fn write_uses_best_text_representation() {
        let fake = Arc::new(FakePasteboard::default());
        let clipboard = MacosClipboard::with_io(fake.clone());
        let snapshot = ClipboardSnapshot::new(vec![
            ClipboardContent::html("<b>hi</b>"),
            ClipboardContent::text("hi"),
        ]);

        clipboard.write(snapshot).await.unwrap();
        assert_eq!(fake.written(), Some("hi".to_string()));
    }

    #[tokio::test]
    async fn write_rejects_non_text_snapshot() {
        let fake = Arc::new(FakePasteboard::default());
        let clipboard = MacosClipboard::with_io(fake);
        let snapshot = ClipboardSnapshot::new(vec![ClipboardContent::image_png(vec![1, 2, 3])]);

        let error = clipboard.write(snapshot).await.unwrap_err();
        assert!(matches!(error, ClipboardError::Unsupported(_)));
    }
}
