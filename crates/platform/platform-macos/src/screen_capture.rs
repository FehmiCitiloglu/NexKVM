//! macOS screen capture backend using Screen Recording APIs.
//!
//! MVP implementation uses CGDisplayStream and CGDisplayCreateImage for
//! synchronous frame capture. Returns uncompressed BGRA8 frames suitable
//! for hardware encoding downstream.
//!
//! Frame sequence numbering is monotonic per backend instance to support
//! encoder state machines and frame ordering.

// Native framework calls are isolated in this module behind safe crate APIs.
#![allow(unsafe_code)]

use async_trait::async_trait;
use bytes::Bytes;
use nexkvm_streaming::{
    CaptureSource, CaptureSourceId, GpuMemoryKind, HardwareEncoder, PixelFormat,
    ScreenCaptureBackend, ScreenCodec, ScreenError, ScreenFeatureSet, ScreenFrame,
    ScreenPermissions, ScreenResolution, ScreenStreamCapabilities, ScreenStreamPlan,
    WindowVisibility,
};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{CStr, c_void};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

const WINDOW_LIST_OPTION_ON_SCREEN_ONLY: u32 = 1;
const WINDOW_LIST_OPTION_INCLUDE_WINDOW: u32 = 8;
const WINDOW_LIST_OPTION_EXCLUDE_DESKTOP: u32 = 16;
const WINDOW_IMAGE_OPTION_BOUNDS_IGNORE_FRAMING: u32 = 1;
const CF_NUMBER_S64: i32 = 4;
const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

#[repr(C)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
struct CGSize {
    width: f64,
    height: f64,
}

#[repr(C)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

const NULL_RECT: CGRect = CGRect {
    origin: CGPoint { x: 0.0, y: 0.0 },
    size: CGSize {
        width: 0.0,
        height: 0.0,
    },
};

unsafe extern "C" {
    fn CGDisplayCreateImage(display: u32) -> *mut c_void;
    fn CGWindowListCopyWindowInfo(option: u32, relative_to_window: u32) -> *mut c_void;
    fn CGWindowListCreateImage(
        rect: CGRect,
        option: u32,
        window_id: u32,
        image_option: u32,
    ) -> *mut c_void;
    fn CGImageGetWidth(image: *const c_void) -> usize;
    fn CGImageGetHeight(image: *const c_void) -> usize;
    fn CGImageGetDataProvider(image: *const c_void) -> *mut c_void;
    fn CFDataProviderCopyData(provider: *mut c_void) -> *mut c_void;
    fn CFDataGetBytePtr(data: *const c_void) -> *const u8;
    fn CFDataGetLength(data: *const c_void) -> isize;
    fn CFRelease(cf: *const c_void);
    fn mach_absolute_time() -> u64;
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;

    fn CFArrayGetCount(the_array: *const c_void) -> isize;
    fn CFArrayGetValueAtIndex(the_array: *const c_void, idx: isize) -> *const c_void;
    fn CFDictionaryGetValue(the_dict: *const c_void, key: *const c_void) -> *const c_void;
    fn CFNumberGetValue(number: *const c_void, number_type: i32, value_ptr: *mut c_void) -> bool;
    fn CFStringGetLength(the_string: *const c_void) -> isize;
    fn CFStringGetCString(
        the_string: *const c_void,
        buffer: *mut i8,
        buffer_size: isize,
        encoding: u32,
    ) -> bool;
    fn CFBooleanGetValue(boolean: *const c_void) -> bool;

    static kCGWindowNumber: *const c_void;
    static kCGWindowOwnerPID: *const c_void;
    static kCGWindowOwnerName: *const c_void;
    static kCGWindowName: *const c_void;
    static kCGWindowIsOnscreen: *const c_void;
}

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
        let screen_recording_granted = check_screen_recording_permission();
        let has_screen_capturekit = screen_capturekit_available();

        ScreenStreamCapabilities {
            permissions: ScreenPermissions {
                display_capture: screen_recording_granted,
                window_capture: screen_recording_granted,
                app_capture: screen_recording_granted && has_screen_capturekit,
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
                instant_app_preview: has_screen_capturekit,
            },
        }
    }

    async fn request_permissions(&self) -> Result<ScreenPermissions, ScreenError> {
        tokio::task::spawn_blocking(request_screen_recording_permissions)
            .await
            .map_err(|e| ScreenError::Backend(format!("permission request task failed: {e}")))?
    }

    async fn list_sources(&self) -> Result<Vec<CaptureSource>, ScreenError> {
        let has_screen_capturekit = screen_capturekit_available();

        tokio::task::spawn_blocking(move || enumerate_sources(has_screen_capturekit))
            .await
            .map_err(|e| ScreenError::Backend(format!("source enumeration task failed: {e}")))?
    }

    async fn capture_frame(&self, plan: &ScreenStreamPlan) -> Result<ScreenFrame, ScreenError> {
        let sequence = self.sequence.clone();
        let source = plan.source.clone();

        tokio::task::spawn_blocking(move || capture_source_frame(&source, &sequence))
            .await
            .map_err(|e| ScreenError::Backend(format!("capture task failed: {e}")))?
    }
}

/// Check if the app has Screen Recording permission.
fn check_screen_recording_permission() -> bool {
    // SAFETY: This calls CoreGraphics preflight API and does not dereference
    // pointers or assume additional invariants.
    unsafe { CGPreflightScreenCaptureAccess() }
}

fn request_screen_recording_permissions() -> Result<ScreenPermissions, ScreenError> {
    // SAFETY: This calls CoreGraphics permission APIs with no raw pointers.
    let granted = unsafe {
        if CGPreflightScreenCaptureAccess() {
            true
        } else {
            CGRequestScreenCaptureAccess()
        }
    };

    let has_screen_capturekit = screen_capturekit_available();
    Ok(ScreenPermissions {
        display_capture: granted,
        window_capture: granted,
        app_capture: granted && has_screen_capturekit,
        permission_pending: false,
    })
}

fn screen_capturekit_available() -> bool {
    use objc2::runtime::AnyClass;

    // ScreenCaptureKit class presence is a practical runtime probe here.
    AnyClass::get("SCShareableContent").is_some()
}

/// Enumerate displays, windows, and apps.
fn enumerate_sources(include_app_sources: bool) -> Result<Vec<CaptureSource>, ScreenError> {
    let mut sources = enumerate_displays()?;

    let window_infos = enumerate_windows()?;
    let mut app_ids: BTreeSet<i64> = BTreeSet::new();
    let mut app_names: BTreeMap<i64, String> = BTreeMap::new();

    for window in window_infos {
        app_ids.insert(window.owner_pid);
        app_names
            .entry(window.owner_pid)
            .or_insert(window.owner_name.clone());
        sources.push(CaptureSource::Window {
            id: CaptureSourceId::new(format!("window:{}", window.window_id)),
            title: window.title,
            app_id: Some(format!("app:{}", window.owner_pid)),
            visibility: if window.on_screen {
                WindowVisibility::Visible
            } else {
                WindowVisibility::Hidden
            },
        });
    }

    if include_app_sources {
        for app_id in app_ids {
            let name = app_names
                .get(&app_id)
                .cloned()
                .unwrap_or_else(|| format!("Application {app_id}"));
            sources.push(CaptureSource::Application {
                id: CaptureSourceId::new(format!("app:{app_id}")),
                name,
            });
        }
    }

    Ok(sources)
}

/// Enumerate all attached displays.
fn enumerate_displays() -> Result<Vec<CaptureSource>, ScreenError> {
    use objc2::msg_send;
    use objc2::runtime::AnyClass;

    // SAFETY: Objective-C class/message usage follows Cocoa API contracts.
    unsafe {
        let screen_class = match AnyClass::get("NSScreen") {
            Some(class) => class,
            None => return Err(ScreenError::Backend("NSScreen class not found".into())),
        };

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
            let label = if localized_name.is_null() {
                format!("Display {i}")
            } else {
                let c_str: *const u8 = msg_send![localized_name, UTF8String];
                if c_str.is_null() {
                    format!("Display {i}")
                } else {
                    CStr::from_ptr(c_str.cast::<i8>())
                        .to_string_lossy()
                        .into_owned()
                }
            };

            sources.push(CaptureSource::Display {
                id: CaptureSourceId::new(format!("{display_id}")),
                label,
            });
        }

        Ok(sources)
    }
}

#[derive(Debug, Clone)]
struct WindowInfo {
    window_id: i64,
    owner_pid: i64,
    owner_name: String,
    title: String,
    on_screen: bool,
}

fn enumerate_windows() -> Result<Vec<WindowInfo>, ScreenError> {
    // SAFETY: CoreGraphics returns a retained CFArray that we release before returning.
    unsafe {
        let list = CGWindowListCopyWindowInfo(
            WINDOW_LIST_OPTION_ON_SCREEN_ONLY | WINDOW_LIST_OPTION_EXCLUDE_DESKTOP,
            0,
        );
        if list.is_null() {
            return Ok(Vec::new());
        }

        let count = CFArrayGetCount(list);
        let mut windows = Vec::new();
        for index in 0..count {
            let entry = CFArrayGetValueAtIndex(list, index);
            if entry.is_null() {
                continue;
            }

            let window_id = match dict_get_i64(entry, kCGWindowNumber) {
                Some(value) if value > 0 => value,
                _ => continue,
            };
            let owner_pid = dict_get_i64(entry, kCGWindowOwnerPID).unwrap_or_default();
            if owner_pid <= 0 {
                continue;
            }

            let owner_name = dict_get_string(entry, kCGWindowOwnerName)
                .unwrap_or_else(|| format!("Application {owner_pid}"));
            let title = dict_get_string(entry, kCGWindowName)
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| format!("{owner_name} Window"));
            let on_screen = dict_get_bool(entry, kCGWindowIsOnscreen).unwrap_or(true);

            windows.push(WindowInfo {
                window_id,
                owner_pid,
                owner_name,
                title,
                on_screen,
            });
        }

        CFRelease(list);
        Ok(windows)
    }
}

fn dict_get_i64(dict: *const c_void, key: *const c_void) -> Option<i64> {
    // SAFETY: We only read dictionary/number values returned by CoreGraphics.
    unsafe {
        let value = CFDictionaryGetValue(dict, key);
        if value.is_null() {
            return None;
        }

        let mut out = 0_i64;
        if CFNumberGetValue(
            value,
            CF_NUMBER_S64,
            (&mut out as *mut i64).cast::<c_void>(),
        ) {
            Some(out)
        } else {
            None
        }
    }
}

fn dict_get_bool(dict: *const c_void, key: *const c_void) -> Option<bool> {
    // SAFETY: CFBooleanGetValue is used on dictionary values supplied by CoreGraphics.
    unsafe {
        let value = CFDictionaryGetValue(dict, key);
        if value.is_null() {
            None
        } else {
            Some(CFBooleanGetValue(value))
        }
    }
}

fn dict_get_string(dict: *const c_void, key: *const c_void) -> Option<String> {
    // SAFETY: We attempt UTF-8 extraction from CFString values provided by the OS.
    unsafe {
        let value = CFDictionaryGetValue(dict, key);
        if value.is_null() {
            return None;
        }

        let len = CFStringGetLength(value);
        if len <= 0 {
            return Some(String::new());
        }

        let mut buffer = vec![0_i8; (len as usize * 4) + 1];
        if CFStringGetCString(
            value,
            buffer.as_mut_ptr(),
            buffer.len() as isize,
            CF_STRING_ENCODING_UTF8,
        ) {
            CStr::from_ptr(buffer.as_ptr())
                .to_str()
                .ok()
                .map(ToOwned::to_owned)
        } else {
            None
        }
    }
}

/// Capture a single frame from any supported source.
fn capture_source_frame(
    source: &CaptureSource,
    sequence: &Arc<AtomicU64>,
) -> Result<ScreenFrame, ScreenError> {
    match source {
        CaptureSource::Display { id, .. } => {
            let display_id =
                id.0.parse::<u32>()
                    .map_err(|_| ScreenError::Backend("invalid display ID format".into()))?;
            capture_display_frame(display_id, sequence)
        }
        CaptureSource::Window { id, .. } => {
            let window_id =
                id.0.strip_prefix("window:")
                    .ok_or(ScreenError::SourceUnavailable(
                        "invalid window source id".to_string(),
                    ))?
                    .parse::<u32>()
                    .map_err(|_| ScreenError::Backend("invalid window ID format".into()))?;
            capture_window_frame(window_id, sequence)
        }
        CaptureSource::Application { id, .. } => {
            let app_pid =
                id.0.strip_prefix("app:")
                    .ok_or(ScreenError::SourceUnavailable(
                        "invalid application source id".to_string(),
                    ))?
                    .parse::<i64>()
                    .map_err(|_| ScreenError::Backend("invalid application ID format".into()))?;
            capture_application_frame(app_pid, sequence)
        }
    }
}

fn capture_display_frame(
    display_id: u32,
    sequence: &Arc<AtomicU64>,
) -> Result<ScreenFrame, ScreenError> {
    // SAFETY: CoreGraphics returns a retained CGImageRef that we release in
    // `screen_frame_from_image`.
    unsafe {
        let image = CGDisplayCreateImage(display_id);
        if image.is_null() {
            return Err(ScreenError::SourceUnavailable(format!(
                "display {display_id} is unavailable"
            )));
        }
        screen_frame_from_image(image, sequence)
    }
}

fn capture_window_frame(
    window_id: u32,
    sequence: &Arc<AtomicU64>,
) -> Result<ScreenFrame, ScreenError> {
    // SAFETY: CoreGraphics returns a retained CGImageRef that we release in
    // `screen_frame_from_image`.
    unsafe {
        let image = CGWindowListCreateImage(
            NULL_RECT,
            WINDOW_LIST_OPTION_INCLUDE_WINDOW,
            window_id,
            WINDOW_IMAGE_OPTION_BOUNDS_IGNORE_FRAMING,
        );
        if image.is_null() {
            return Err(ScreenError::SourceUnavailable(format!(
                "window {window_id} is unavailable"
            )));
        }
        screen_frame_from_image(image, sequence)
    }
}

fn capture_application_frame(
    app_pid: i64,
    sequence: &Arc<AtomicU64>,
) -> Result<ScreenFrame, ScreenError> {
    let windows = enumerate_windows()?;
    let maybe_window = windows
        .into_iter()
        .find(|window| window.owner_pid == app_pid && window.on_screen)
        .or_else(|| {
            enumerate_windows()
                .ok()?
                .into_iter()
                .find(|window| window.owner_pid == app_pid)
        });

    let window = maybe_window.ok_or(ScreenError::SourceUnavailable(format!(
        "application {app_pid} has no capturable windows"
    )))?;

    capture_window_frame(window.window_id as u32, sequence)
}

fn screen_frame_from_image(
    image: *mut c_void,
    sequence: &Arc<AtomicU64>,
) -> Result<ScreenFrame, ScreenError> {
    // SAFETY: `image` is a valid CGImageRef from CoreGraphics. We copy provider
    // bytes into owned Rust memory and release all CF objects.
    unsafe {
        let width = CGImageGetWidth(image) as u32;
        let height = CGImageGetHeight(image) as u32;
        let provider = CGImageGetDataProvider(image);
        if provider.is_null() {
            CFRelease(image);
            return Err(ScreenError::Backend(
                "failed to get image data provider".into(),
            ));
        }

        let cf_data = CFDataProviderCopyData(provider);
        if cf_data.is_null() {
            CFRelease(image);
            return Err(ScreenError::Backend("failed to copy image bytes".into()));
        }

        let ptr = CFDataGetBytePtr(cf_data);
        let len = CFDataGetLength(cf_data);
        if ptr.is_null() || len <= 0 {
            CFRelease(cf_data);
            CFRelease(image);
            return Err(ScreenError::Backend(
                "captured image had no pixel bytes".into(),
            ));
        }

        let pixel_data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
        CFRelease(cf_data);
        CFRelease(image);

        // SAFETY: `mach_absolute_time` is pure and requires no pointer invariants.
        let capture_time_micros = mach_absolute_time() / 1000;

        Ok(ScreenFrame {
            sequence: sequence.fetch_add(1, Ordering::SeqCst),
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
