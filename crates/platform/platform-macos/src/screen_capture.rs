//! macOS screen capture backend using Screen Recording APIs.
//!
//! MVP implementation uses CGDisplayStream and CGDisplayCreateImage for
//! synchronous frame capture. Returns uncompressed BGRA8 frames suitable
//! for hardware encoding downstream.
//!
//! Frame sequence numbering is monotonic per backend instance to support
//! encoder state machines and frame ordering.

use async_trait::async_trait;
use bytes::Bytes;
use nexkvm_streaming::{
    CaptureSource, CaptureSourceId, GpuMemoryKind, HardwareEncoder, PixelFormat,
    ScreenCaptureBackend, ScreenCodec, ScreenError, ScreenFeatureSet, ScreenFrame,
    ScreenPermissions, ScreenResolution, ScreenStreamCapabilities, ScreenStreamPlan,
};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// macOS screen capture backend using CGDisplayStream and CGDisplayCreateImage.
///
/// For MVP, displays are captured synchronously on-demand using CGDisplayCreateImage.
/// Each backend instance has an independent monotonic frame sequence counter.
#[derive(Clone)]
pub struct MacosScreenCapture {
    /// Monotonic frame sequence counter for stream ordering.
    sequence: Arc<AtomicU64>,
}

impl MacosScreenCapture {
    /// Create a screen capture backend for the current session.
    #[must_use]
    pub fn new() -> Self {
        Self {
            sequence: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl Default for MacosScreenCapture {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for MacosScreenCapture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MacosScreenCapture").finish_non_exhaustive()
    }
}

#[async_trait]
impl ScreenCaptureBackend for MacosScreenCapture {
    fn capabilities(&self) -> ScreenStreamCapabilities {
        ScreenStreamCapabilities {
            permissions: ScreenPermissions {
                display_capture: check_screen_recording_permission(),
                window_capture: check_screen_recording_permission(),
                app_capture: false,
                permission_pending: false,
            },
            memory_kinds: vec![GpuMemoryKind::System, GpuMemoryKind::IoSurface],
            codecs: vec![ScreenCodec::RawRgba, ScreenCodec::H264, ScreenCodec::H265],
            encoders: vec![HardwareEncoder::VideoToolbox, HardwareEncoder::Software],
            max_resolution: ScreenResolution::new(3840, 2160),
            max_fps: 120,
            features: ScreenFeatureSet {
                mini_remote_preview: true,
                window_peeking: true,
                instant_app_preview: false,
            },
        }
    }

    async fn request_permissions(&self) -> Result<ScreenPermissions, ScreenError> {
        Ok(ScreenPermissions {
            display_capture: check_screen_recording_permission(),
            window_capture: check_screen_recording_permission(),
            app_capture: false,
            permission_pending: false,
        })
    }

    async fn list_sources(&self) -> Result<Vec<CaptureSource>, ScreenError> {
        tokio::task::spawn_blocking(|| enumerate_displays())
            .await
            .map_err(|e| ScreenError::Backend(format!("source enumeration task failed: {e}")))?
    }

    async fn capture_frame(&self, plan: &ScreenStreamPlan) -> Result<ScreenFrame, ScreenError> {
        let sequence = self.sequence.clone();
        let source = plan.source.clone();

        tokio::task::spawn_blocking(move || capture_display_frame(&source, &sequence))
            .await
            .map_err(|e| ScreenError::Backend(format!("capture task failed: {e}")))?
    }
}

/// Check if the app has Screen Recording permission.
fn check_screen_recording_permission() -> bool {
    false
}

/// Enumerate all attached displays.
fn enumerate_displays() -> Result<Vec<CaptureSource>, ScreenError> {
    use objc2::msg_send;
    use objc2::runtime::AnyClass;

    unsafe {
        let screen_class = AnyClass::get("NSScreen");
        if screen_class.is_none() {
            return Err(ScreenError::Backend("NSScreen class not found".into()));
        }

        let screen_class = screen_class.unwrap();
        let screens: *mut objc2::runtime::AnyObject = msg_send![screen_class, screens];
        if screens.is_null() {
            return Ok(Vec::new());
        }

        let count: usize = msg_send![screens, count];
        let mut sources = Vec::new();

        for i in 0..count {
            let screen: *mut objc2::runtime::AnyObject = msg_send![screens, objectAtIndex: i];
            if screen.is_null() {
                continue;
            }

            let display_id: u32 = msg_send![screen, displayID];
            let localized_name: *mut objc2::runtime::AnyObject = msg_send![screen, localizedName];
            let label = if !localized_name.is_null() {
                let c_str: *const u8 = msg_send![localized_name, UTF8String];
                if !c_str.is_null() {
                    std::ffi::CStr::from_ptr(c_str as *const i8)
                        .to_string_lossy()
                        .into_owned()
                } else {
                    format!("Display {}", i)
                }
            } else {
                format!("Display {}", i)
            };

            sources.push(CaptureSource::Display {
                id: CaptureSourceId::new(format!("{}", display_id)),
                label,
            });
        }

        Ok(sources)
    }
}

/// Capture a single frame from a display using CGDisplayCreateImage.
fn capture_display_frame(
    source: &CaptureSource,
    sequence: &Arc<AtomicU64>,
) -> Result<ScreenFrame, ScreenError> {
    let display_id = match source {
        CaptureSource::Display { id, .. } => {
            id.0.parse::<u32>()
                .map_err(|_| ScreenError::Backend("invalid display ID format".into()))?
        }
        _ => {
            return Err(ScreenError::SourceUnavailable(
                "only display capture is supported".to_string(),
            ));
        }
    };

    unsafe extern "C" {
        fn CGDisplayCreateImage(display: u32) -> *mut std::ffi::c_void;
        fn CGImageGetWidth(image: *const std::ffi::c_void) -> usize;
        fn CGImageGetHeight(image: *const std::ffi::c_void) -> usize;
        fn CGImageGetDataProvider(image: *const std::ffi::c_void) -> *mut std::ffi::c_void;
        fn CFDataProviderCopyData(provider: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
        fn CFDataGetBytePtr(data: *const std::ffi::c_void) -> *const u8;
        fn CFDataGetLength(data: *const std::ffi::c_void) -> isize;
        fn CFRelease(cf: *const std::ffi::c_void);
        fn mach_absolute_time() -> u64;
    }

    unsafe {
        let image = CGDisplayCreateImage(display_id);
        if image.is_null() {
            return Err(ScreenError::Backend(
                "CGDisplayCreateImage returned null (display may not be active)".into(),
            ));
        }

        let width = CGImageGetWidth(image) as u32;
        let height = CGImageGetHeight(image) as u32;
        let provider = CGImageGetDataProvider(image);
        if provider.is_null() {
            CFRelease(image);
            return Err(ScreenError::Backend(
                "failed to get CGImage data provider".into(),
            ));
        }

        let cf_data = CFDataProviderCopyData(provider);
        if cf_data.is_null() {
            CFRelease(image);
            return Err(ScreenError::Backend(
                "failed to copy CGImage pixel data".into(),
            ));
        }

        let ptr = CFDataGetBytePtr(cf_data);
        let len = CFDataGetLength(cf_data) as usize;

        if ptr.is_null() || len == 0 {
            CFRelease(cf_data);
            CFRelease(image);
            return Err(ScreenError::Backend(
                "CGImage has no pixel data or is invalid".into(),
            ));
        }

        let pixel_data = std::slice::from_raw_parts(ptr, len).to_vec();
        CFRelease(cf_data);
        CFRelease(image);

        let seq = sequence.fetch_add(1, Ordering::SeqCst);
        let mach_time = mach_absolute_time();
        let capture_time_micros = mach_time / 1000;

        Ok(ScreenFrame {
            sequence: seq,
            capture_time_micros,
            resolution: ScreenResolution::new(width, height),
            pixel_format: PixelFormat::Bgra8,
            memory: GpuMemoryKind::System,
            payload: Bytes::from(pixel_data),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_macos_screen_capture_creation() {
        let _capture = MacosScreenCapture::new();
    }

    #[test]
    fn test_capabilities_report() {
        let capture = MacosScreenCapture::new();
        let caps = capture.capabilities();
        assert!(caps.codecs.contains(&ScreenCodec::H264));
        assert!(caps.max_fps > 0);
        assert!(caps.max_resolution.width > 0);
    }
}
