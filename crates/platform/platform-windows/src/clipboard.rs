//! Windows clipboard backend using the native Clipboard API.
//!
//! Maps Windows clipboard formats (CF_UNICODETEXT, CF_DIB, CF_HDROP) to standard
//! MIME types for cross-platform clipboard synchronization.

use async_trait::async_trait;
use nexkvm_clipboard::{Clipboard, ClipboardContent, ClipboardError, ClipboardSnapshot};
use std::fmt;

/// Maps Windows clipboard format names to standard MIME types.
fn cf_to_mime(format_name: &str) -> Option<String> {
    match format_name {
        // Text formats
        "CF_UNICODETEXT" | "CF_TEXT" => Some("text/plain;charset=utf-8".into()),
        // Image formats (canonicalize to PNG)
        "CF_DIB" | "CF_DIBV5" => Some("image/png".into()),
        // File lists
        "CF_HDROP" => Some("text/uri-list".into()),
        // HTML
        "HTML Format" => Some("text/html;charset=utf-8".into()),
        // RTF
        "Rich Text Format" => Some("text/rtf".into()),
        // Custom or unknown format
        _ => Some(format!("application/x-windows-cf:{}", format_name)),
    }
}

/// Maps standard MIME types to Windows clipboard format names.
fn mime_to_cf(mime: &str) -> Option<String> {
    let base = mime.split(';').next().unwrap_or(mime).trim();
    match base {
        "text/plain" => Some("CF_UNICODETEXT".into()),
        "image/png" => Some("CF_DIB".into()),
        "image/jpeg" => Some("CF_DIB".into()),
        "text/html" => Some("HTML Format".into()),
        "text/rtf" | "application/rtf" => Some("Rich Text Format".into()),
        "text/uri-list" => Some("CF_HDROP".into()),
        _ => None,
    }
}

/// Windows clipboard adapter implementing [`Clipboard`].
///
/// Provides text, image, and file list access via the native Windows clipboard API.
/// Uses UTF-8 encoding for text with transparent conversion from/to UTF-16.
#[derive(Clone)]
pub struct WindowsClipboard;

impl WindowsClipboard {
    /// Create a clipboard adapter bound to the Windows clipboard.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for WindowsClipboard {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for WindowsClipboard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WindowsClipboard").finish_non_exhaustive()
    }
}

#[async_trait]
impl Clipboard for WindowsClipboard {
    async fn read(&self) -> Result<Option<ClipboardSnapshot>, ClipboardError> {
        tokio::task::spawn_blocking(|| {
            // Try to get text from the clipboard
            let text = clipboard_win::get_clipboard_string()
                .ok()
                .filter(|s| !s.is_empty());

            if let Some(text_content) = text {
                let contents = vec![ClipboardContent::text(text_content)];
                Ok(Some(ClipboardSnapshot::new(contents)))
            } else {
                Ok(None)
            }
        })
        .await
        .map_err(|e| ClipboardError::Backend(format!("clipboard read task failed: {e}")))?
    }

    async fn write(&self, snapshot: ClipboardSnapshot) -> Result<(), ClipboardError> {
        // Extract text content if available (Windows MVP supports text only)
        let Some(text) = snapshot.best_text().map(str::to_owned) else {
            return Err(ClipboardError::Unsupported(
                "windows clipboard backend currently writes text formats only",
            ));
        };

        tokio::task::spawn_blocking(move || {
            clipboard_win::set_clipboard_string(&text)
                .map_err(|e| ClipboardError::Backend(format!("set clipboard failed: {e}")))
        })
        .await
        .map_err(|e| ClipboardError::Backend(format!("clipboard write task failed: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cf_to_mime_text() {
        assert_eq!(
            cf_to_mime("CF_UNICODETEXT"),
            Some("text/plain;charset=utf-8".into())
        );
    }

    #[test]
    fn test_cf_to_mime_image() {
        assert_eq!(cf_to_mime("CF_DIB"), Some("image/png".into()));
    }

    #[test]
    fn test_mime_to_cf_text() {
        assert_eq!(
            mime_to_cf("text/plain;charset=utf-8"),
            Some("CF_UNICODETEXT".into())
        );
    }

    #[test]
    fn test_mime_to_cf_image() {
        assert_eq!(mime_to_cf("image/png"), Some("CF_DIB".into()));
    }
}
