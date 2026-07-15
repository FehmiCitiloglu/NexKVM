//! NSPasteboard FFI wrapper for multi-format clipboard access.
//!
//! Maps between macOS NSPasteboard UTIs and standard MIME types for transparent
//! cross-platform clipboard synchronization of text, HTML, RTF, and image formats.

// Native framework calls are isolated in this module behind safe crate APIs.
#![allow(unsafe_code)]

use nexkvm_clipboard::{ClipboardContent, ClipboardError, ClipboardSnapshot};
use objc2::rc::autoreleasepool;
use objc2::{class, msg_send};
use objc2_app_kit::NSPasteboard;
use objc2_foundation::{NSArray, NSData, NSString};

const MAX_PASTEBOARD_FORMATS: usize = 32;
const MAX_PASTEBOARD_TOTAL_BYTES: usize = 16 * 1024 * 1024;
const MAX_UTI_BYTES: usize = 1_024;

/// Maps macOS NSPasteboard UTI types to standard MIME types.
///
/// Reference: https://developer.apple.com/library/archive/documentation/Miscellaneous/Reference/UTTypeIdentifiers/
fn uti_to_mime(uti: &str) -> Option<String> {
    match uti {
        // Text
        "public.utf8-plain-text" => Some("text/plain;charset=utf-8".into()),
        "public.plain-text" => Some("text/plain;charset=utf-8".into()),
        // HTML
        "public.html" => Some("text/html;charset=utf-8".into()),
        // RTF
        "public.rtf" => Some("text/rtf".into()),
        // Images (canonicalize to PNG)
        "public.png" => Some("image/png".into()),
        "public.jpeg" => Some("image/jpeg".into()),
        "public.tiff" => Some("image/tiff".into()),
        // File URLs
        "public.file-url" | "public.url" => Some("text/uri-list".into()),
        // Custom or unknown UTI
        _ => Some(format!("application/x-macos-uti:{uti}")),
    }
}

/// Maps standard MIME types to macOS NSPasteboard UTI types.
fn mime_to_uti(mime: &str) -> Option<String> {
    let base = mime.split(';').next().unwrap_or(mime).trim();
    match base {
        "text/plain" => Some("public.utf8-plain-text".into()),
        "text/html" => Some("public.html".into()),
        "text/rtf" | "application/rtf" => Some("public.rtf".into()),
        "image/png" => Some("public.png".into()),
        "image/jpeg" => Some("public.jpeg".into()),
        "image/tiff" => Some("public.tiff".into()),
        "text/uri-list" => Some("public.file-url".into()),
        custom if custom.starts_with("application/x-macos-uti:") => custom
            .strip_prefix("application/x-macos-uti:")
            .filter(|uti| !uti.is_empty() && uti.len() <= MAX_UTI_BYTES)
            .map(str::to_owned),
        // For unknown types, try to use as-is if it looks like a UTI
        s if s.contains('.') => Some(s.into()),
        _ => None,
    }
}

/// Read multi-format clipboard content from NSPasteboard.
pub fn read_pasteboard() -> Result<Option<ClipboardSnapshot>, ClipboardError> {
    autoreleasepool(|_| unsafe {
        // Get the general pasteboard
        let pasteboard: *mut NSPasteboard = msg_send![class!(NSPasteboard), generalPasteboard];
        if pasteboard.is_null() {
            return Err(ClipboardError::Backend(
                "failed to get general pasteboard".into(),
            ));
        }

        // Get available types
        let types: *mut NSArray<NSString> = msg_send![pasteboard, types];
        if types.is_null() {
            return Ok(None);
        }

        let count: usize = msg_send![types, count];
        if count == 0 {
            return Ok(None);
        }
        if count > MAX_PASTEBOARD_FORMATS {
            return Err(ClipboardError::TooLarge {
                size: count,
                limit: MAX_PASTEBOARD_FORMATS,
            });
        }

        let mut contents = Vec::new();
        let mut total_bytes = 0usize;

        // Read each available format
        for i in 0..count {
            let uti_obj: *mut NSString = msg_send![types, objectAtIndex: i];
            if uti_obj.is_null() {
                continue;
            }

            // Convert NSString to Rust string
            let uti_ptr: *const u8 = msg_send![uti_obj, UTF8String];
            if uti_ptr.is_null() {
                continue;
            }

            let uti_cstr = std::ffi::CStr::from_ptr(uti_ptr as *const i8);
            let uti = match uti_cstr.to_str() {
                Ok(u) => u,
                Err(_) => continue,
            };
            if uti.len() > MAX_UTI_BYTES {
                return Err(ClipboardError::TooLarge {
                    size: uti.len(),
                    limit: MAX_UTI_BYTES,
                });
            }

            // Try to get the MIME type for this UTI
            let mime = match uti_to_mime(uti) {
                Some(m) => m,
                None => continue,
            };

            // Read the data for this type
            let data: *mut NSData = msg_send![pasteboard, dataForType: uti_obj];
            if data.is_null() {
                continue;
            }

            let bytes_ptr: *const u8 = msg_send![data, bytes];
            let len: usize = msg_send![data, length];
            total_bytes = total_bytes
                .checked_add(len)
                .ok_or(ClipboardError::TooLarge {
                    size: usize::MAX,
                    limit: MAX_PASTEBOARD_TOTAL_BYTES,
                })?;
            if total_bytes > MAX_PASTEBOARD_TOTAL_BYTES {
                return Err(ClipboardError::TooLarge {
                    size: total_bytes,
                    limit: MAX_PASTEBOARD_TOTAL_BYTES,
                });
            }
            let bytes = if len == 0 {
                bytes::Bytes::new()
            } else {
                if bytes_ptr.is_null() {
                    return Err(ClipboardError::Backend(
                        "pasteboard returned null bytes for non-empty data".into(),
                    ));
                }
                bytes::Bytes::copy_from_slice(std::slice::from_raw_parts(bytes_ptr, len))
            };
            // Zero-length marker types are significant: password managers use
            // them to mark a selection as concealed.
            contents.push(ClipboardContent { mime, data: bytes });
        }

        if contents.is_empty() {
            Ok(None)
        } else {
            Ok(Some(ClipboardSnapshot::new(contents)))
        }
    })
}

/// Write multi-format clipboard content to NSPasteboard.
pub fn write_pasteboard(snapshot: &ClipboardSnapshot) -> Result<(), ClipboardError> {
    autoreleasepool(|_| unsafe {
        // Get the general pasteboard
        let pasteboard: *mut NSPasteboard = msg_send![class!(NSPasteboard), generalPasteboard];
        if pasteboard.is_null() {
            return Err(ClipboardError::Backend(
                "failed to get general pasteboard".into(),
            ));
        }

        // Clear the pasteboard
        let _: isize = msg_send![pasteboard, clearContents];

        let mut written = 0usize;
        let mut failed = 0usize;
        // Write each format
        for content in snapshot.formats() {
            let Some(uti) = mime_to_uti(&content.mime) else {
                tracing::debug!("skipping unsupported MIME type: {}", content.mime);
                continue;
            };

            let uti_ns = NSString::from_str(&uti);
            let data_bytes = content.data.as_ref();
            let data_ptr = data_bytes.as_ptr() as *const std::ffi::c_void;
            let data_ns: *mut NSData =
                msg_send![class!(NSData), dataWithBytes:data_ptr length:data_bytes.len()];

            if data_ns.is_null() {
                failed = failed.saturating_add(1);
                continue;
            }

            let success: bool = msg_send![pasteboard, setData:data_ns forType:&*uti_ns];
            if !success {
                failed = failed.saturating_add(1);
            } else {
                written = written.saturating_add(1);
            }
        }
        if failed > 0 || (!snapshot.is_empty() && written == 0) {
            Err(ClipboardError::Backend(format!(
                "macOS pasteboard wrote {written} format(s) and failed {failed} format(s)"
            )))
        } else {
            Ok(())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_uti_to_mime_text() {
        assert_eq!(
            uti_to_mime("public.utf8-plain-text"),
            Some("text/plain;charset=utf-8".into())
        );
    }

    #[test]
    fn test_uti_to_mime_html() {
        assert_eq!(
            uti_to_mime("public.html"),
            Some("text/html;charset=utf-8".into())
        );
    }

    #[test]
    fn test_uti_to_mime_image() {
        assert_eq!(uti_to_mime("public.png"), Some("image/png".into()));
    }

    #[test]
    fn test_mime_to_uti_text() {
        assert_eq!(
            mime_to_uti("text/plain;charset=utf-8"),
            Some("public.utf8-plain-text".into())
        );
    }

    #[test]
    fn test_mime_to_uti_html() {
        assert_eq!(
            mime_to_uti("text/html;charset=utf-8"),
            Some("public.html".into())
        );
    }

    #[test]
    fn test_mime_to_uti_image() {
        assert_eq!(mime_to_uti("image/png"), Some("public.png".into()));
        assert_eq!(mime_to_uti("image/jpeg"), Some("public.jpeg".into()));
        assert_eq!(mime_to_uti("image/tiff"), Some("public.tiff".into()));
    }

    #[test]
    fn test_custom_macos_uti_round_trip_mapping() {
        let mime = uti_to_mime("org.nspasteboard.ConcealedType").unwrap();
        assert_eq!(
            mime_to_uti(&mime),
            Some("org.nspasteboard.ConcealedType".into())
        );
    }
}
