#![allow(unsafe_code)]

use async_trait::async_trait;
use nexkvm_input::{InputCapture, InputError, InputEvent, MouseButton};
use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

/// Quartz event kinds relevant to capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum CgCaptureEventType {
    /// `kCGEventLeftMouseDown`.
    LeftMouseDown = 1,
    /// `kCGEventLeftMouseUp`.
    LeftMouseUp = 2,
    /// `kCGEventRightMouseDown`.
    RightMouseDown = 3,
    /// `kCGEventRightMouseUp`.
    RightMouseUp = 4,
    /// `kCGEventMouseMoved`.
    MouseMoved = 5,
    /// `kCGEventLeftMouseDragged`.
    LeftMouseDragged = 6,
    /// `kCGEventRightMouseDragged`.
    RightMouseDragged = 7,
    /// `kCGEventKeyDown`.
    KeyDown = 10,
    /// `kCGEventKeyUp`.
    KeyUp = 11,
    /// `kCGEventFlagsChanged`.
    FlagsChanged = 12,
    /// `kCGEventScrollWheel`.
    ScrollWheel = 22,
    /// `kCGEventOtherMouseDown`.
    OtherMouseDown = 25,
    /// `kCGEventOtherMouseUp`.
    OtherMouseUp = 26,
    /// `kCGEventOtherMouseDragged`.
    OtherMouseDragged = 27,
}

/// Testable snapshot of the CoreGraphics event fields capture needs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CapturedCgEvent {
    /// Quartz event type.
    pub event_type: CgCaptureEventType,
    /// Mouse location in display pixels.
    pub location: Option<(f64, f64)>,
    /// Display size in pixels for normalizing absolute pointer position.
    pub display_size: Option<(f64, f64)>,
    /// macOS virtual key code for keyboard events.
    pub keycode: Option<u16>,
    /// Scroll delta x in line units.
    pub scroll_dx: Option<f64>,
    /// Scroll delta y in line units.
    pub scroll_dy: Option<f64>,
    /// Mouse delta x in display-fraction units.
    pub delta_dx: Option<f64>,
    /// Mouse delta y in display-fraction units.
    pub delta_dy: Option<f64>,
}

impl Default for CapturedCgEvent {
    fn default() -> Self {
        Self {
            event_type: CgCaptureEventType::MouseMoved,
            location: None,
            display_size: None,
            keycode: None,
            scroll_dx: None,
            scroll_dy: None,
            delta_dx: None,
            delta_dy: None,
        }
    }
}

/// Translate captured CoreGraphics fields into the platform-neutral input event.
#[must_use]
pub fn plan_capture_event(event: CapturedCgEvent) -> Option<InputEvent> {
    plan_capture_event_with_mode(event, false)
}

fn plan_capture_event_with_mode(event: CapturedCgEvent, suppressed: bool) -> Option<InputEvent> {
    match event.event_type {
        CgCaptureEventType::MouseMoved
        | CgCaptureEventType::LeftMouseDragged
        | CgCaptureEventType::RightMouseDragged
        | CgCaptureEventType::OtherMouseDragged => {
            if suppressed {
                return Some(InputEvent::RelativeMove {
                    dx: event.delta_dx?,
                    dy: event.delta_dy?,
                });
            }
            let (x, y) = event.location?;
            let (width, height) = event.display_size?;
            Some(InputEvent::PointerMove {
                x: normalize_axis(x, width),
                y: normalize_axis(y, height),
            })
        }
        CgCaptureEventType::LeftMouseDown => Some(InputEvent::ButtonPress(MouseButton::Left)),
        CgCaptureEventType::LeftMouseUp => Some(InputEvent::ButtonRelease(MouseButton::Left)),
        CgCaptureEventType::RightMouseDown => Some(InputEvent::ButtonPress(MouseButton::Right)),
        CgCaptureEventType::RightMouseUp => Some(InputEvent::ButtonRelease(MouseButton::Right)),
        CgCaptureEventType::OtherMouseDown => Some(InputEvent::ButtonPress(MouseButton::Middle)),
        CgCaptureEventType::OtherMouseUp => Some(InputEvent::ButtonRelease(MouseButton::Middle)),
        CgCaptureEventType::ScrollWheel => Some(InputEvent::Scroll {
            dx: event.scroll_dx.unwrap_or_default(),
            dy: event.scroll_dy.unwrap_or_default(),
        }),
        CgCaptureEventType::KeyDown => cg_to_hid_keycode(event.keycode?).map(InputEvent::KeyPress),
        CgCaptureEventType::KeyUp => cg_to_hid_keycode(event.keycode?).map(InputEvent::KeyRelease),
        CgCaptureEventType::FlagsChanged => None,
    }
}

fn normalize_axis(value: f64, max: f64) -> f64 {
    if max <= 1.0 {
        return 0.0;
    }
    (value / max).clamp(0.0, 1.0)
}

fn cg_to_hid_keycode(keycode: u16) -> Option<u32> {
    match keycode {
        0 => Some(0x04),  // A
        11 => Some(0x05), // B
        8 => Some(0x06),  // C
        2 => Some(0x07),  // D
        14 => Some(0x08), // E
        3 => Some(0x09),  // F
        5 => Some(0x0A),  // G
        4 => Some(0x0B),  // H
        34 => Some(0x0C), // I
        38 => Some(0x0D), // J
        40 => Some(0x0E), // K
        37 => Some(0x0F), // L
        46 => Some(0x10), // M
        45 => Some(0x11), // N
        31 => Some(0x12), // O
        35 => Some(0x13), // P
        12 => Some(0x14), // Q
        15 => Some(0x15), // R
        1 => Some(0x16),  // S
        17 => Some(0x17), // T
        32 => Some(0x18), // U
        9 => Some(0x19),  // V
        13 => Some(0x1A), // W
        7 => Some(0x1B),  // X
        16 => Some(0x1C), // Y
        6 => Some(0x1D),  // Z
        53 => Some(0x29), // Escape
        49 => Some(0x2C), // Space
        _ => None,
    }
}

#[derive(Debug, Clone)]
pub struct MacosInputCapture {
    accessibility_trusted: bool,
    receiver: Option<Arc<Mutex<Receiver<InputEvent>>>>,
    suppressed: Arc<AtomicBool>,
}

impl MacosInputCapture {
    #[must_use]
    pub fn new(accessibility_trusted: bool) -> Self {
        let suppressed = Arc::new(AtomicBool::new(false));
        let receiver = if accessibility_trusted {
            Some(start_event_tap_capture(Arc::clone(&suppressed)))
        } else {
            None
        };
        Self {
            accessibility_trusted,
            receiver,
            suppressed,
        }
    }

    pub fn set_suppressed(&self, suppressed: bool) {
        self.suppressed.store(suppressed, Ordering::SeqCst);
    }

    #[cfg(test)]
    fn with_events(accessibility_trusted: bool, events: Vec<InputEvent>) -> Self {
        let (sender, receiver) = mpsc::channel();
        for event in events {
            sender.send(event).expect("send fixture event");
        }
        Self {
            accessibility_trusted,
            receiver: Some(Arc::new(Mutex::new(receiver))),
            suppressed: Arc::new(AtomicBool::new(false)),
        }
    }
}

#[async_trait]
impl InputCapture for MacosInputCapture {
    async fn next_event(&self) -> Result<InputEvent, InputError> {
        if !self.accessibility_trusted {
            return Err(InputError::PermissionDenied);
        }
        let Some(receiver) = &self.receiver else {
            return Err(InputError::Backend(
                "macOS CGEvent tap capture loop is not running".into(),
            ));
        };
        receiver
            .lock()
            .map_err(|_| InputError::Backend("macOS capture receiver lock poisoned".into()))?
            .recv()
            .map_err(|_| InputError::Backend("macOS CGEvent tap capture loop stopped".into()))
    }
}

type CGEventTapProxy = *mut c_void;
type CGEventRef = *mut c_void;
type CFMachPortRef = *mut c_void;
type CFRunLoopSourceRef = *mut c_void;
type CFRunLoopRef = *mut c_void;
type CFStringRef = *const c_void;
type CFTypeRef = *const c_void;
type CGEventTapCallBack =
    extern "C" fn(CGEventTapProxy, u32, CGEventRef, *mut c_void) -> CGEventRef;

const K_CG_SESSION_EVENT_TAP: u32 = 1;
const K_CG_HEAD_INSERT_EVENT_TAP: u32 = 0;
const K_CG_EVENT_TAP_OPTION_LISTEN_ONLY: u32 = 1;
const K_CG_EVENT_FIELD_KEYBOARD_EVENT_KEYCODE: u32 = 9;
const K_CG_MOUSE_EVENT_DELTA_X: u32 = 4;
const K_CG_MOUSE_EVENT_DELTA_Y: u32 = 5;
const K_CG_SCROLL_WHEEL_EVENT_DELTA_AXIS_1: u32 = 11;
const K_CG_SCROLL_WHEEL_EVENT_DELTA_AXIS_2: u32 = 12;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: u64,
        callback: CGEventTapCallBack,
        user_info: *mut c_void,
    ) -> CFMachPortRef;
    fn CGEventGetLocation(event: CGEventRef) -> CGPoint;
    fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
    fn CGMainDisplayID() -> u32;
    fn CGDisplayPixelsWide(display: u32) -> usize;
    fn CGDisplayPixelsHigh(display: u32) -> usize;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFMachPortCreateRunLoopSource(
        allocator: *const c_void,
        port: CFMachPortRef,
        order: isize,
    ) -> CFRunLoopSourceRef;
    fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    fn CFRunLoopAddSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
    fn CFRunLoopRun();
    fn CFRelease(cf: CFTypeRef);

    static kCFRunLoopCommonModes: CFStringRef;
}

#[derive(Debug)]
struct CaptureCallbackState {
    sender: Sender<InputEvent>,
    suppressed: Arc<AtomicBool>,
}

fn start_event_tap_capture(suppressed: Arc<AtomicBool>) -> Arc<Mutex<Receiver<InputEvent>>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || run_event_tap(CaptureCallbackState { sender, suppressed }));
    Arc::new(Mutex::new(receiver))
}

fn run_event_tap(state: CaptureCallbackState) {
    let state = Box::new(state);
    let user_info = Box::into_raw(state).cast::<c_void>();
    let mask = capture_event_mask();
    // SAFETY: The callback receives the boxed sender via `user_info` for the
    // lifetime of the run loop thread. The event mask only enables known event
    // constants handled by `capture_callback`.
    let tap = unsafe {
        CGEventTapCreate(
            K_CG_SESSION_EVENT_TAP,
            K_CG_HEAD_INSERT_EVENT_TAP,
            K_CG_EVENT_TAP_OPTION_LISTEN_ONLY,
            mask,
            capture_callback,
            user_info,
        )
    };
    if tap.is_null() {
        // SAFETY: Reclaim the sender if the tap could not be created.
        unsafe {
            drop(Box::from_raw(user_info.cast::<CaptureCallbackState>()));
        }
        return;
    }
    // SAFETY: `tap` is a valid mach port. Source is added to this thread's run
    // loop in common modes and both CoreFoundation objects are retained by the
    // run loop for its lifetime.
    unsafe {
        let source = CFMachPortCreateRunLoopSource(ptr::null(), tap, 0);
        if !source.is_null() {
            CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopCommonModes);
            CFRelease(source.cast());
        }
        CFRelease(tap.cast());
        CFRunLoopRun();
    }
}

extern "C" fn capture_callback(
    _proxy: CGEventTapProxy,
    event_type: u32,
    event: CGEventRef,
    user_info: *mut c_void,
) -> CGEventRef {
    if user_info.is_null() || event.is_null() {
        return event;
    }
    let Some(captured) = captured_from_native(event_type, event) else {
        return event;
    };
    // SAFETY: `user_info` was created from `Box<CaptureCallbackState>` in
    // `run_event_tap` and lives for the run loop thread lifetime.
    let state = unsafe { &*(user_info.cast::<CaptureCallbackState>()) };
    let suppressed = state.suppressed.load(Ordering::SeqCst);
    if let Some(input_event) = plan_capture_event_with_mode(captured, suppressed) {
        let _ = state.sender.send(input_event);
    }
    event
}

fn captured_from_native(event_type: u32, event: CGEventRef) -> Option<CapturedCgEvent> {
    let event_type = cg_capture_event_type(event_type)?;
    // SAFETY: `event` is provided by CoreGraphics to the callback and is valid
    // for the duration of this call. Field reads are side-effect free.
    let location = unsafe { CGEventGetLocation(event) };
    let display = main_display_size();
    let keycode =
        unsafe { CGEventGetIntegerValueField(event, K_CG_EVENT_FIELD_KEYBOARD_EVENT_KEYCODE) };
    let scroll_y =
        unsafe { CGEventGetIntegerValueField(event, K_CG_SCROLL_WHEEL_EVENT_DELTA_AXIS_1) };
    let scroll_x =
        unsafe { CGEventGetIntegerValueField(event, K_CG_SCROLL_WHEEL_EVENT_DELTA_AXIS_2) };
    let delta_x = unsafe { CGEventGetIntegerValueField(event, K_CG_MOUSE_EVENT_DELTA_X) };
    let delta_y = unsafe { CGEventGetIntegerValueField(event, K_CG_MOUSE_EVENT_DELTA_Y) };
    Some(CapturedCgEvent {
        event_type,
        location: Some((location.x, location.y)),
        display_size: Some(display),
        keycode: Some(keycode as u16),
        scroll_dx: Some(scroll_x as f64),
        scroll_dy: Some(scroll_y as f64),
        delta_dx: Some(delta_x as f64 / display.0.max(1.0)),
        delta_dy: Some(delta_y as f64 / display.1.max(1.0)),
    })
}

fn cg_capture_event_type(raw: u32) -> Option<CgCaptureEventType> {
    match raw {
        1 => Some(CgCaptureEventType::LeftMouseDown),
        2 => Some(CgCaptureEventType::LeftMouseUp),
        3 => Some(CgCaptureEventType::RightMouseDown),
        4 => Some(CgCaptureEventType::RightMouseUp),
        5 => Some(CgCaptureEventType::MouseMoved),
        6 => Some(CgCaptureEventType::LeftMouseDragged),
        7 => Some(CgCaptureEventType::RightMouseDragged),
        10 => Some(CgCaptureEventType::KeyDown),
        11 => Some(CgCaptureEventType::KeyUp),
        12 => Some(CgCaptureEventType::FlagsChanged),
        22 => Some(CgCaptureEventType::ScrollWheel),
        25 => Some(CgCaptureEventType::OtherMouseDown),
        26 => Some(CgCaptureEventType::OtherMouseUp),
        27 => Some(CgCaptureEventType::OtherMouseDragged),
        _ => None,
    }
}

fn capture_event_mask() -> u64 {
    [
        CgCaptureEventType::LeftMouseDown,
        CgCaptureEventType::LeftMouseUp,
        CgCaptureEventType::RightMouseDown,
        CgCaptureEventType::RightMouseUp,
        CgCaptureEventType::MouseMoved,
        CgCaptureEventType::LeftMouseDragged,
        CgCaptureEventType::RightMouseDragged,
        CgCaptureEventType::KeyDown,
        CgCaptureEventType::KeyUp,
        CgCaptureEventType::ScrollWheel,
        CgCaptureEventType::OtherMouseDown,
        CgCaptureEventType::OtherMouseUp,
        CgCaptureEventType::OtherMouseDragged,
    ]
    .into_iter()
    .fold(0u64, |mask, event_type| mask | (1u64 << event_type as u32))
}

fn main_display_size() -> (f64, f64) {
    // SAFETY: Display metadata functions are read-only. Zero sizes are guarded
    // with `max(1)` to avoid divide-by-zero in normalization.
    unsafe {
        let display = CGMainDisplayID();
        (
            CGDisplayPixelsWide(display).max(1) as f64,
            CGDisplayPixelsHigh(display).max(1) as f64,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captured_mouse_move_normalizes_to_input_event() {
        let event = CapturedCgEvent {
            event_type: CgCaptureEventType::MouseMoved,
            location: Some((960.0, 540.0)),
            display_size: Some((1920.0, 1080.0)),
            ..CapturedCgEvent::default()
        };

        assert_eq!(
            plan_capture_event(event),
            Some(InputEvent::PointerMove { x: 0.5, y: 0.5 })
        );
    }

    #[test]
    fn captured_button_and_key_events_map_to_input_events() {
        assert_eq!(
            plan_capture_event(CapturedCgEvent {
                event_type: CgCaptureEventType::LeftMouseDown,
                ..CapturedCgEvent::default()
            }),
            Some(InputEvent::ButtonPress(nexkvm_input::MouseButton::Left))
        );
        assert_eq!(
            plan_capture_event(CapturedCgEvent {
                event_type: CgCaptureEventType::KeyUp,
                keycode: Some(0),
                ..CapturedCgEvent::default()
            }),
            Some(InputEvent::KeyRelease(0x04))
        );
    }

    #[test]
    fn unsupported_capture_event_is_ignored() {
        assert_eq!(
            plan_capture_event(CapturedCgEvent {
                event_type: CgCaptureEventType::FlagsChanged,
                ..CapturedCgEvent::default()
            }),
            None
        );
    }

    #[tokio::test]
    async fn capture_refuses_without_accessibility_permission() {
        let capture = MacosInputCapture::new(false);
        let result = capture.next_event().await;

        assert!(matches!(result, Err(InputError::PermissionDenied)));
    }

    #[tokio::test]
    async fn capture_returns_queued_event_when_accessibility_is_ready() {
        let capture = MacosInputCapture::with_events(true, vec![InputEvent::KeyPress(0x04)]);

        assert_eq!(
            capture.next_event().await.unwrap(),
            InputEvent::KeyPress(0x04)
        );
    }
}
