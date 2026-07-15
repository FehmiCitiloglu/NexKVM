#![allow(unsafe_code)]

use async_trait::async_trait;
use nexkvm_input::{InputCapture, InputError, InputEvent, MouseButton};
use std::ffi::c_void;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::thread;
use tokio::sync::{Mutex, mpsc};

const CAPTURE_QUEUE_CAPACITY: usize = 4_096;

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
    /// CoreGraphics modifier flags for `FlagsChanged` events.
    pub event_flags: Option<u64>,
    /// Individual state for the modifier key that triggered `FlagsChanged`.
    /// This disambiguates left/right keys that share one aggregate flag.
    pub key_down: Option<bool>,
    /// Scroll delta x in line units.
    pub scroll_dx: Option<f64>,
    /// Scroll delta y in line units.
    pub scroll_dy: Option<f64>,
    /// Mouse delta x in display-fraction units.
    pub delta_dx: Option<f64>,
    /// Mouse delta y in display-fraction units.
    pub delta_dy: Option<f64>,
}

/// Planned handling for one captured CoreGraphics event.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CaptureAction {
    /// Event to forward into the NexKVM input stream, if supported.
    pub forward: Option<InputEvent>,
    /// Whether the original event should continue to the local OS.
    pub pass_through: bool,
}

impl Default for CapturedCgEvent {
    fn default() -> Self {
        Self {
            event_type: CgCaptureEventType::MouseMoved,
            location: None,
            display_size: None,
            keycode: None,
            event_flags: None,
            key_down: None,
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
    plan_capture_action(event, false).forward
}

#[must_use]
pub fn plan_capture_action(event: CapturedCgEvent, suppressed: bool) -> CaptureAction {
    CaptureAction {
        forward: plan_capture_event_with_mode(event, suppressed),
        pass_through: !suppressed,
    }
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
        CgCaptureEventType::FlagsChanged => {
            let keycode = event.keycode?;
            let (hid, mask) = cg_modifier_to_hid_and_flag(keycode)?;
            let pressed = event
                .key_down
                .unwrap_or_else(|| event.event_flags.unwrap_or_default() & mask != 0);
            if pressed {
                Some(InputEvent::KeyPress(hid))
            } else {
                Some(InputEvent::KeyRelease(hid))
            }
        }
    }
}

fn normalize_axis(value: f64, max: f64) -> f64 {
    if max <= 1.0 {
        return 0.0;
    }
    (value / max).clamp(0.0, 1.0)
}

pub(crate) fn cg_to_hid_keycode(keycode: u16) -> Option<u32> {
    match keycode {
        0 => Some(0x04),   // A
        11 => Some(0x05),  // B
        8 => Some(0x06),   // C
        2 => Some(0x07),   // D
        14 => Some(0x08),  // E
        3 => Some(0x09),   // F
        5 => Some(0x0A),   // G
        4 => Some(0x0B),   // H
        34 => Some(0x0C),  // I
        38 => Some(0x0D),  // J
        40 => Some(0x0E),  // K
        37 => Some(0x0F),  // L
        46 => Some(0x10),  // M
        45 => Some(0x11),  // N
        31 => Some(0x12),  // O
        35 => Some(0x13),  // P
        12 => Some(0x14),  // Q
        15 => Some(0x15),  // R
        1 => Some(0x16),   // S
        17 => Some(0x17),  // T
        32 => Some(0x18),  // U
        9 => Some(0x19),   // V
        13 => Some(0x1A),  // W
        7 => Some(0x1B),   // X
        16 => Some(0x1C),  // Y
        6 => Some(0x1D),   // Z
        18 => Some(0x1E),  // 1
        19 => Some(0x1F),  // 2
        20 => Some(0x20),  // 3
        21 => Some(0x21),  // 4
        23 => Some(0x22),  // 5
        22 => Some(0x23),  // 6
        26 => Some(0x24),  // 7
        28 => Some(0x25),  // 8
        25 => Some(0x26),  // 9
        29 => Some(0x27),  // 0
        36 => Some(0x28),  // Return
        53 => Some(0x29),  // Escape
        51 => Some(0x2A),  // Backspace
        48 => Some(0x2B),  // Tab
        49 => Some(0x2C),  // Space
        27 => Some(0x2D),  // Minus
        24 => Some(0x2E),  // Equal
        33 => Some(0x2F),  // Left bracket
        30 => Some(0x30),  // Right bracket
        42 => Some(0x31),  // Backslash
        41 => Some(0x33),  // Semicolon
        39 => Some(0x34),  // Apostrophe
        50 => Some(0x35),  // Grave
        43 => Some(0x36),  // Comma
        47 => Some(0x37),  // Period
        44 => Some(0x38),  // Slash
        122 => Some(0x3A), // F1
        120 => Some(0x3B), // F2
        99 => Some(0x3C),  // F3
        118 => Some(0x3D), // F4
        96 => Some(0x3E),  // F5
        97 => Some(0x3F),  // F6
        98 => Some(0x40),  // F7
        100 => Some(0x41), // F8
        101 => Some(0x42), // F9
        109 => Some(0x43), // F10
        103 => Some(0x44), // F11
        111 => Some(0x45), // F12
        115 => Some(0x4A), // Home
        116 => Some(0x4B), // Page up
        117 => Some(0x4C), // Forward delete
        119 => Some(0x4D), // End
        121 => Some(0x4E), // Page down
        124 => Some(0x4F), // Right arrow
        123 => Some(0x50), // Left arrow
        125 => Some(0x51), // Down arrow
        126 => Some(0x52), // Up arrow
        75 => Some(0x54),  // Keypad divide
        67 => Some(0x55),  // Keypad multiply
        78 => Some(0x56),  // Keypad minus
        69 => Some(0x57),  // Keypad plus
        76 => Some(0x58),  // Keypad enter
        83 => Some(0x59),  // Keypad 1
        84 => Some(0x5A),  // Keypad 2
        85 => Some(0x5B),  // Keypad 3
        86 => Some(0x5C),  // Keypad 4
        87 => Some(0x5D),  // Keypad 5
        88 => Some(0x5E),  // Keypad 6
        89 => Some(0x5F),  // Keypad 7
        91 => Some(0x60),  // Keypad 8
        92 => Some(0x61),  // Keypad 9
        82 => Some(0x62),  // Keypad 0
        65 => Some(0x63),  // Keypad decimal
        81 => Some(0x67),  // Keypad equal
        _ => None,
    }
}

const CG_EVENT_FLAG_MASK_ALPHA_SHIFT: u64 = 0x0001_0000;
const CG_EVENT_FLAG_MASK_SHIFT: u64 = 0x0002_0000;
const CG_EVENT_FLAG_MASK_CONTROL: u64 = 0x0004_0000;
const CG_EVENT_FLAG_MASK_ALTERNATE: u64 = 0x0008_0000;
const CG_EVENT_FLAG_MASK_COMMAND: u64 = 0x0010_0000;

pub(crate) fn cg_modifier_to_hid_and_flag(keycode: u16) -> Option<(u32, u64)> {
    match keycode {
        59 => Some((0xE0, CG_EVENT_FLAG_MASK_CONTROL)),
        56 => Some((0xE1, CG_EVENT_FLAG_MASK_SHIFT)),
        58 => Some((0xE2, CG_EVENT_FLAG_MASK_ALTERNATE)),
        55 => Some((0xE3, CG_EVENT_FLAG_MASK_COMMAND)),
        62 => Some((0xE4, CG_EVENT_FLAG_MASK_CONTROL)),
        60 => Some((0xE5, CG_EVENT_FLAG_MASK_SHIFT)),
        61 => Some((0xE6, CG_EVENT_FLAG_MASK_ALTERNATE)),
        54 => Some((0xE7, CG_EVENT_FLAG_MASK_COMMAND)),
        57 => Some((0x39, CG_EVENT_FLAG_MASK_ALPHA_SHIFT)),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum CaptureFault {
    None = 0,
    QueueOverflow = 1,
    ReceiverClosed = 2,
}

impl CaptureFault {
    fn from_raw(raw: u8) -> Self {
        match raw {
            1 => Self::QueueOverflow,
            2 => Self::ReceiverClosed,
            _ => Self::None,
        }
    }

    fn message(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::QueueOverflow => Some(
                "macOS capture queue overflowed while local input was suppressed; remote input session stopped",
            ),
            Self::ReceiverClosed => Some(
                "macOS capture receiver closed while local input was suppressed; remote input session stopped",
            ),
        }
    }
}

#[derive(Debug)]
struct EventTapControl {
    suppressed: AtomicBool,
    fault: AtomicU8,
    shutdown: AtomicBool,
    native_handles: std::sync::Mutex<EventTapNativeHandles>,
    tap_reenable_count: AtomicU64,
}

#[derive(Debug, Default)]
struct EventTapNativeHandles {
    tap: usize,
    run_loop: usize,
}

impl Default for EventTapControl {
    fn default() -> Self {
        Self {
            suppressed: AtomicBool::new(false),
            fault: AtomicU8::new(CaptureFault::None as u8),
            shutdown: AtomicBool::new(false),
            native_handles: std::sync::Mutex::new(EventTapNativeHandles::default()),
            tap_reenable_count: AtomicU64::new(0),
        }
    }
}

impl EventTapControl {
    fn record_fault(&self, fault: CaptureFault) {
        let _ = self.fault.compare_exchange(
            CaptureFault::None as u8,
            fault as u8,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
    }

    fn fault(&self) -> CaptureFault {
        CaptureFault::from_raw(self.fault.load(Ordering::SeqCst))
    }

    fn restore_local_cursor(&self) {
        update_suppression(&self.suppressed, false, set_native_cursor_hidden);
    }

    fn native_handles(&self) -> std::sync::MutexGuard<'_, EventTapNativeHandles> {
        self.native_handles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[derive(Debug)]
struct CaptureLifecycle {
    control: Arc<EventTapControl>,
    update_native_cursor: bool,
    event_thread: std::sync::Mutex<Option<thread::JoinHandle<()>>>,
}

impl CaptureLifecycle {
    fn new(control: Arc<EventTapControl>, event_thread: Option<thread::JoinHandle<()>>) -> Self {
        Self {
            control,
            update_native_cursor: true,
            event_thread: std::sync::Mutex::new(event_thread),
        }
    }

    #[cfg(test)]
    fn for_test(control: Arc<EventTapControl>) -> Self {
        Self {
            control,
            update_native_cursor: false,
            event_thread: std::sync::Mutex::new(None),
        }
    }
}

impl Drop for CaptureLifecycle {
    fn drop(&mut self) {
        self.control.shutdown.store(true, Ordering::SeqCst);
        let was_suppressed = self.control.suppressed.swap(false, Ordering::SeqCst);
        if was_suppressed && self.update_native_cursor {
            set_native_cursor_hidden(false);
        }
        let handles = self.control.native_handles();
        if handles.run_loop != 0 {
            // SAFETY: The event-tap thread publishes and clears this pointer
            // under the same mutex. Holding it prevents teardown/release from
            // racing this stop request.
            unsafe { CFRunLoopStop(handles.run_loop as CFRunLoopRef) };
        }
        drop(handles);
        let event_thread = self
            .event_thread
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(event_thread) = event_thread
            && event_thread.join().is_err()
        {
            tracing::warn!("macOS event-tap thread panicked during shutdown");
        }
    }
}

#[derive(Debug, Clone)]
pub struct MacosInputCapture {
    accessibility_trusted: bool,
    receiver: Option<Arc<Mutex<mpsc::Receiver<InputEvent>>>>,
    lifecycle: Arc<CaptureLifecycle>,
}

impl MacosInputCapture {
    #[must_use]
    pub fn new(accessibility_trusted: bool) -> Self {
        let control = Arc::new(EventTapControl::default());
        let (receiver, event_thread) = if accessibility_trusted {
            let (receiver, event_thread) = start_event_tap_capture(Arc::clone(&control));
            (Some(receiver), Some(event_thread))
        } else {
            (None, None)
        };
        Self {
            accessibility_trusted,
            receiver,
            lifecycle: Arc::new(CaptureLifecycle::new(control, event_thread)),
        }
    }

    pub fn set_suppressed(&self, suppressed: bool) {
        if suppressed && self.lifecycle.control.fault() != CaptureFault::None {
            self.lifecycle.control.restore_local_cursor();
            return;
        }
        update_suppression(
            &self.lifecycle.control.suppressed,
            suppressed,
            set_native_cursor_hidden,
        );
    }

    #[cfg(test)]
    fn with_events(accessibility_trusted: bool, events: Vec<InputEvent>) -> Self {
        let control = Arc::new(EventTapControl::default());
        Self::with_control_and_events_with_permission(control, events, accessibility_trusted)
    }

    #[cfg(test)]
    fn with_control_and_events(control: Arc<EventTapControl>, events: Vec<InputEvent>) -> Self {
        Self::with_control_and_events_with_permission(control, events, true)
    }

    #[cfg(test)]
    fn with_control_and_events_with_permission(
        control: Arc<EventTapControl>,
        events: Vec<InputEvent>,
        accessibility_trusted: bool,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(events.len().max(1));
        for event in events {
            sender.try_send(event).expect("send fixture event");
        }
        Self {
            accessibility_trusted,
            receiver: Some(Arc::new(Mutex::new(receiver))),
            lifecycle: Arc::new(CaptureLifecycle::for_test(control)),
        }
    }

    #[cfg(test)]
    fn from_test_parts(lifecycle: Arc<CaptureLifecycle>) -> Self {
        Self {
            accessibility_trusted: false,
            receiver: None,
            lifecycle,
        }
    }

    /// Discard input captured before the current authenticated peer session.
    /// This prevents a stale disconnected backlog from triggering a handoff.
    pub async fn discard_pending(&self) {
        let Some(receiver) = &self.receiver else {
            return;
        };
        let mut receiver = receiver.lock().await;
        while receiver.try_recv().is_ok() {}
    }
}

fn update_suppression(
    state: &AtomicBool,
    suppressed: bool,
    mut set_cursor_hidden: impl FnMut(bool),
) {
    if state.swap(suppressed, Ordering::SeqCst) != suppressed {
        set_cursor_hidden(suppressed);
    }
}

fn set_native_cursor_hidden(hidden: bool) {
    let (associated, hidden) = native_cursor_plan(hidden);
    // SAFETY: Cursor functions accept value parameters and a display identifier
    // returned by CoreGraphics; they retain no caller-owned memory.
    unsafe {
        let display = CGMainDisplayID();
        let association_error = CGAssociateMouseAndMouseCursorPosition(u32::from(associated));
        let visibility_error = if hidden {
            CGDisplayHideCursor(display)
        } else {
            CGDisplayShowCursor(display)
        };
        if association_error != 0 || visibility_error != 0 {
            tracing::warn!(
                association_error,
                visibility_error,
                hidden,
                "failed to update native cursor suppression"
            );
        }
    }
}

fn native_cursor_plan(suppressed: bool) -> (bool, bool) {
    (!suppressed, suppressed)
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
        if let Some(message) = self.lifecycle.control.fault().message() {
            return Err(InputError::Backend(message.into()));
        }
        let event =
            receiver.lock().await.recv().await.ok_or_else(|| {
                InputError::Backend("macOS CGEvent tap capture loop stopped".into())
            })?;
        if let Some(message) = self.lifecycle.control.fault().message() {
            return Err(InputError::Backend(message.into()));
        }
        Ok(event)
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
const K_CG_EVENT_TAP_OPTION_DEFAULT: u32 = 0;
const K_CG_EVENT_FIELD_KEYBOARD_EVENT_KEYCODE: u32 = 9;
const K_CG_EVENT_SOURCE_USER_DATA: u32 = 42;
const K_CG_MOUSE_EVENT_DELTA_X: u32 = 4;
const K_CG_MOUSE_EVENT_DELTA_Y: u32 = 5;
const K_CG_SCROLL_WHEEL_EVENT_DELTA_AXIS_1: u32 = 11;
const K_CG_SCROLL_WHEEL_EVENT_DELTA_AXIS_2: u32 = 12;
const K_CG_EVENT_SOURCE_STATE_HID_SYSTEM_STATE: i32 = 1;
const K_CG_EVENT_TAP_DISABLED_BY_TIMEOUT: u32 = u32::MAX - 1;
const K_CG_EVENT_TAP_DISABLED_BY_USER_INPUT: u32 = u32::MAX;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
struct CGSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
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
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
    fn CGEventGetLocation(event: CGEventRef) -> CGPoint;
    fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
    fn CGEventGetFlags(event: CGEventRef) -> u64;
    fn CGEventSourceKeyState(state_id: i32, key: u16) -> bool;
    fn CGMainDisplayID() -> u32;
    fn CGDisplayBounds(display: u32) -> CGRect;
    fn CGGetActiveDisplayList(
        max_displays: u32,
        active_displays: *mut u32,
        display_count: *mut u32,
    ) -> i32;
    fn CGDisplayHideCursor(display: u32) -> i32;
    fn CGDisplayShowCursor(display: u32) -> i32;
    fn CGAssociateMouseAndMouseCursorPosition(connected: u32) -> i32;
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
    fn CFRunLoopRemoveSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
    fn CFRunLoopRun();
    fn CFRunLoopStop(rl: CFRunLoopRef);
    fn CFRelease(cf: CFTypeRef);

    static kCFRunLoopCommonModes: CFStringRef;
}

#[derive(Debug)]
struct CaptureCallbackState {
    sender: mpsc::Sender<InputEvent>,
    control: Arc<EventTapControl>,
}

fn start_event_tap_capture(
    control: Arc<EventTapControl>,
) -> (
    Arc<Mutex<mpsc::Receiver<InputEvent>>>,
    thread::JoinHandle<()>,
) {
    let (sender, receiver) = mpsc::channel(CAPTURE_QUEUE_CAPACITY);
    let event_thread =
        thread::spawn(move || run_event_tap(CaptureCallbackState { sender, control }));
    (Arc::new(Mutex::new(receiver)), event_thread)
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
            capture_tap_options(),
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
    // Publish the actual event-tap port, not the callback proxy. It remains
    // retained until the run-loop teardown below completes.
    // SAFETY: `user_info` still owns a live `CaptureCallbackState` here.
    let control = unsafe { Arc::clone(&(*user_info.cast::<CaptureCallbackState>()).control) };
    control.native_handles().tap = tap as usize;

    // SAFETY: `tap` is a valid mach port. The source and tap are explicitly
    // removed/released after the run loop stops, and `user_info` is reclaimed
    // only after callbacks on this thread have ended.
    unsafe {
        let source = CFMachPortCreateRunLoopSource(ptr::null(), tap, 0);
        if source.is_null() {
            control.native_handles().tap = 0;
            CFRelease(tap.cast());
            drop(Box::from_raw(user_info.cast::<CaptureCallbackState>()));
            return;
        }
        let run_loop = CFRunLoopGetCurrent();
        control.native_handles().run_loop = run_loop as usize;
        CFRunLoopAddSource(run_loop, source, kCFRunLoopCommonModes);
        if !control.shutdown.load(Ordering::Acquire) {
            CFRunLoopRun();
        }
        control.native_handles().run_loop = 0;
        CFRunLoopRemoveSource(run_loop, source, kCFRunLoopCommonModes);
        CFRelease(source.cast());
        control.native_handles().tap = 0;
        CFRelease(tap.cast());
        drop(Box::from_raw(user_info.cast::<CaptureCallbackState>()));
    }
}

fn capture_tap_options() -> u32 {
    K_CG_EVENT_TAP_OPTION_DEFAULT
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TapDisableReason {
    Timeout,
    UserInput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NativeCallbackPlan {
    Reenable(TapDisableReason),
    Capture,
    PassThrough,
}

fn plan_native_callback(event_type: u32, event_present: bool) -> NativeCallbackPlan {
    match event_type {
        K_CG_EVENT_TAP_DISABLED_BY_TIMEOUT => {
            NativeCallbackPlan::Reenable(TapDisableReason::Timeout)
        }
        K_CG_EVENT_TAP_DISABLED_BY_USER_INPUT => {
            NativeCallbackPlan::Reenable(TapDisableReason::UserInput)
        }
        _ if event_present => NativeCallbackPlan::Capture,
        _ => NativeCallbackPlan::PassThrough,
    }
}

fn is_nexkvm_injected_event(source_user_data: i64) -> bool {
    source_user_data == crate::NEXKVM_EVENT_SOURCE_USER_DATA
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueueSendOutcome {
    Sent,
    Full,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueueSendPlan {
    UseCaptureAction,
    DropAndPassThrough,
    FailSafePassThrough(CaptureFault),
}

fn plan_queue_send(
    suppressed: bool,
    droppable_motion: bool,
    outcome: QueueSendOutcome,
) -> QueueSendPlan {
    match (suppressed, droppable_motion, outcome) {
        (_, _, QueueSendOutcome::Sent) => QueueSendPlan::UseCaptureAction,
        (false, true, QueueSendOutcome::Full) | (false, _, QueueSendOutcome::Closed) => {
            QueueSendPlan::DropAndPassThrough
        }
        (_, _, QueueSendOutcome::Full) => {
            QueueSendPlan::FailSafePassThrough(CaptureFault::QueueOverflow)
        }
        (true, _, QueueSendOutcome::Closed) => {
            QueueSendPlan::FailSafePassThrough(CaptureFault::ReceiverClosed)
        }
    }
}

fn is_droppable_motion(event: InputEvent) -> bool {
    matches!(
        event,
        InputEvent::PointerMove { .. }
            | InputEvent::RelativeMove { .. }
            | InputEvent::RawMotion { .. }
    )
}

extern "C" fn capture_callback(
    _proxy: CGEventTapProxy,
    event_type: u32,
    event: CGEventRef,
    user_info: *mut c_void,
) -> CGEventRef {
    if user_info.is_null() {
        return event;
    }
    // SAFETY: `user_info` was created from `Box<CaptureCallbackState>` in
    // `run_event_tap` and is reclaimed only after this run loop stops.
    let state = unsafe { &*(user_info.cast::<CaptureCallbackState>()) };
    match plan_native_callback(event_type, !event.is_null()) {
        NativeCallbackPlan::Reenable(reason) => {
            let handles = state.control.native_handles();
            if handles.tap != 0 && !state.control.shutdown.load(Ordering::Acquire) {
                // SAFETY: `tap` is the retained CFMachPort returned by
                // CGEventTapCreate and the native-handle mutex prevents its
                // release from racing this call.
                unsafe { CGEventTapEnable(handles.tap as CFMachPortRef, true) };
                let count = state
                    .control
                    .tap_reenable_count
                    .fetch_add(1, Ordering::Relaxed)
                    .saturating_add(1);
                tracing::warn!(?reason, count, "macOS event tap disabled; re-enabled");
            } else {
                tracing::warn!(?reason, "macOS event tap disabled during shutdown");
            }
            return event;
        }
        NativeCallbackPlan::PassThrough => return event,
        NativeCallbackPlan::Capture => {}
    }
    // SAFETY: `event` is non-null in the Capture plan and remains owned by
    // CoreGraphics for the callback duration. Reading source user data is
    // side-effect free. NexKVM-tagged events must pass through locally but
    // must never re-enter the outbound capture queue.
    let source_user_data =
        unsafe { CGEventGetIntegerValueField(event, K_CG_EVENT_SOURCE_USER_DATA) };
    if is_nexkvm_injected_event(source_user_data) {
        tracing::trace!("ignored NexKVM-injected event in macOS capture tap");
        return event;
    }
    let Some(captured) = captured_from_native(event_type, event) else {
        return event;
    };
    let suppressed = state.control.suppressed.load(Ordering::SeqCst);
    let action = plan_capture_action(captured, suppressed);
    let (outcome, droppable_motion) = match action.forward {
        Some(input_event) => {
            let droppable_motion = is_droppable_motion(input_event);
            let outcome = match state.sender.try_send(input_event) {
                Ok(()) => QueueSendOutcome::Sent,
                Err(mpsc::error::TrySendError::Full(_)) => QueueSendOutcome::Full,
                Err(mpsc::error::TrySendError::Closed(_)) => QueueSendOutcome::Closed,
            };
            (outcome, droppable_motion)
        }
        None => return event,
    };
    match plan_queue_send(suppressed, droppable_motion, outcome) {
        QueueSendPlan::UseCaptureAction if !action.pass_through => ptr::null_mut(),
        QueueSendPlan::UseCaptureAction | QueueSendPlan::DropAndPassThrough => event,
        QueueSendPlan::FailSafePassThrough(fault) => {
            state.control.record_fault(fault);
            state.control.restore_local_cursor();
            tracing::error!(
                ?fault,
                "macOS capture failed open to protect local input safety"
            );
            event
        }
    }
}

fn captured_from_native(event_type: u32, event: CGEventRef) -> Option<CapturedCgEvent> {
    let event_type = cg_capture_event_type(event_type)?;
    // SAFETY: `event` is provided by CoreGraphics to the callback and is valid
    // for the duration of this call. Field reads are side-effect free.
    let location = unsafe { CGEventGetLocation(event) };
    let desktop = active_desktop_bounds();
    let display = (desktop.size.width.max(1.0), desktop.size.height.max(1.0));
    let keycode =
        unsafe { CGEventGetIntegerValueField(event, K_CG_EVENT_FIELD_KEYBOARD_EVENT_KEYCODE) };
    let event_flags = unsafe { CGEventGetFlags(event) };
    let key_down = if matches!(event_type, CgCaptureEventType::FlagsChanged) {
        // SAFETY: The state id is a documented CoreGraphics constant and the
        // key code was read from the current callback event.
        Some(unsafe {
            CGEventSourceKeyState(K_CG_EVENT_SOURCE_STATE_HID_SYSTEM_STATE, keycode as u16)
        })
    } else {
        None
    };
    let scroll_y =
        unsafe { CGEventGetIntegerValueField(event, K_CG_SCROLL_WHEEL_EVENT_DELTA_AXIS_1) };
    let scroll_x =
        unsafe { CGEventGetIntegerValueField(event, K_CG_SCROLL_WHEEL_EVENT_DELTA_AXIS_2) };
    let delta_x = unsafe { CGEventGetIntegerValueField(event, K_CG_MOUSE_EVENT_DELTA_X) };
    let delta_y = unsafe { CGEventGetIntegerValueField(event, K_CG_MOUSE_EVENT_DELTA_Y) };
    Some(CapturedCgEvent {
        event_type,
        location: Some((location.x - desktop.origin.x, location.y - desktop.origin.y)),
        display_size: Some(display),
        keycode: Some(keycode as u16),
        event_flags: Some(event_flags),
        key_down,
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
        CgCaptureEventType::FlagsChanged,
        CgCaptureEventType::ScrollWheel,
        CgCaptureEventType::OtherMouseDown,
        CgCaptureEventType::OtherMouseUp,
        CgCaptureEventType::OtherMouseDragged,
    ]
    .into_iter()
    .fold(0u64, |mask, event_type| mask | (1u64 << event_type as u32))
}

fn active_desktop_bounds() -> CGRect {
    const MAX_DISPLAYS: usize = 32;
    let mut displays = [0u32; MAX_DISPLAYS];
    let mut count = 0u32;
    // SAFETY: CoreGraphics receives a valid fixed-size output array and count
    // pointer. Display metadata calls retain no caller-owned memory.
    unsafe {
        let status = CGGetActiveDisplayList(MAX_DISPLAYS as u32, displays.as_mut_ptr(), &mut count);
        if status == 0 && count > 0 {
            return union_display_bounds(
                displays[..count.min(MAX_DISPLAYS as u32) as usize]
                    .iter()
                    .map(|display| CGDisplayBounds(*display)),
            )
            .unwrap_or_else(main_display_bounds);
        }
    }
    main_display_bounds()
}

fn main_display_bounds() -> CGRect {
    // SAFETY: Display metadata is read-only and the main display id is valid.
    unsafe { CGDisplayBounds(CGMainDisplayID()) }
}

fn union_display_bounds(bounds: impl IntoIterator<Item = CGRect>) -> Option<CGRect> {
    let mut bounds = bounds.into_iter();
    let first = bounds.next()?;
    let mut min_x = first.origin.x;
    let mut min_y = first.origin.y;
    let mut max_x = first.origin.x + first.size.width.max(0.0);
    let mut max_y = first.origin.y + first.size.height.max(0.0);
    for bounds in bounds {
        min_x = min_x.min(bounds.origin.x);
        min_y = min_y.min(bounds.origin.y);
        max_x = max_x.max(bounds.origin.x + bounds.size.width.max(0.0));
        max_y = max_y.max(bounds.origin.y + bounds.size.height.max(0.0));
    }
    Some(CGRect {
        origin: CGPoint { x: min_x, y: min_y },
        size: CGSize {
            width: (max_x - min_x).max(1.0),
            height: (max_y - min_y).max(1.0),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suppression_visibility_changes_only_on_state_transitions() {
        let state = AtomicBool::new(false);
        let mut visibility = Vec::new();

        update_suppression(&state, true, |hidden| visibility.push(hidden));
        update_suppression(&state, true, |hidden| visibility.push(hidden));
        update_suppression(&state, false, |hidden| visibility.push(hidden));
        update_suppression(&state, false, |hidden| visibility.push(hidden));

        assert_eq!(visibility, vec![true, false]);
    }

    #[test]
    fn suppression_disconnects_the_local_pointer_from_mouse_motion() {
        assert_eq!(native_cursor_plan(true), (false, true));
        assert_eq!(native_cursor_plan(false), (true, false));
    }

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
    fn desktop_union_handles_retina_logical_sizes_and_negative_monitor_origins() {
        let union = union_display_bounds([
            CGRect {
                origin: CGPoint { x: 0.0, y: 0.0 },
                size: CGSize {
                    width: 1512.0,
                    height: 982.0,
                },
            },
            CGRect {
                origin: CGPoint {
                    x: -1920.0,
                    y: -100.0,
                },
                size: CGSize {
                    width: 1920.0,
                    height: 1080.0,
                },
            },
        ])
        .unwrap();

        assert_eq!(
            union.origin,
            CGPoint {
                x: -1920.0,
                y: -100.0
            }
        );
        assert_eq!(
            union.size,
            CGSize {
                width: 3432.0,
                height: 1082.0,
            }
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
    fn captured_common_keys_and_modifiers_map_to_usb_hid() {
        assert_eq!(
            plan_capture_event(CapturedCgEvent {
                event_type: CgCaptureEventType::KeyDown,
                keycode: Some(18),
                ..CapturedCgEvent::default()
            }),
            Some(InputEvent::KeyPress(0x1E))
        );
        assert_eq!(
            plan_capture_event(CapturedCgEvent {
                event_type: CgCaptureEventType::KeyDown,
                keycode: Some(36),
                ..CapturedCgEvent::default()
            }),
            Some(InputEvent::KeyPress(0x28))
        );
        assert_eq!(
            plan_capture_event(CapturedCgEvent {
                event_type: CgCaptureEventType::FlagsChanged,
                keycode: Some(56),
                event_flags: Some(CG_EVENT_FLAG_MASK_SHIFT),
                ..CapturedCgEvent::default()
            }),
            Some(InputEvent::KeyPress(0xE1))
        );
        assert_eq!(
            plan_capture_event(CapturedCgEvent {
                event_type: CgCaptureEventType::FlagsChanged,
                keycode: Some(56),
                event_flags: Some(0),
                ..CapturedCgEvent::default()
            }),
            Some(InputEvent::KeyRelease(0xE1))
        );
    }

    #[test]
    fn individual_modifier_state_wins_over_the_shared_aggregate_flag() {
        assert_eq!(
            plan_capture_event(CapturedCgEvent {
                event_type: CgCaptureEventType::FlagsChanged,
                keycode: Some(56),
                event_flags: Some(CG_EVENT_FLAG_MASK_SHIFT),
                key_down: Some(false),
                ..CapturedCgEvent::default()
            }),
            Some(InputEvent::KeyRelease(0xE1))
        );
        assert_eq!(
            plan_capture_event(CapturedCgEvent {
                event_type: CgCaptureEventType::FlagsChanged,
                keycode: Some(60),
                event_flags: Some(CG_EVENT_FLAG_MASK_SHIFT),
                key_down: Some(true),
                ..CapturedCgEvent::default()
            }),
            Some(InputEvent::KeyPress(0xE5))
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

    #[test]
    fn suppressed_mouse_motion_is_forwarded_and_not_passed_through() {
        let action = plan_capture_action(
            CapturedCgEvent {
                event_type: CgCaptureEventType::MouseMoved,
                delta_dx: Some(0.2),
                delta_dy: Some(-0.1),
                ..CapturedCgEvent::default()
            },
            true,
        );

        assert_eq!(
            action.forward,
            Some(InputEvent::RelativeMove { dx: 0.2, dy: -0.1 })
        );
        assert!(!action.pass_through);
    }

    #[test]
    fn suppressed_click_is_forwarded_and_not_passed_through() {
        let action = plan_capture_action(
            CapturedCgEvent {
                event_type: CgCaptureEventType::LeftMouseDown,
                ..CapturedCgEvent::default()
            },
            true,
        );

        assert_eq!(
            action.forward,
            Some(InputEvent::ButtonPress(nexkvm_input::MouseButton::Left))
        );
        assert!(!action.pass_through);
    }

    #[test]
    fn event_tap_is_active_so_suppression_can_drop_local_events() {
        assert_eq!(capture_tap_options(), 0);
    }

    #[test]
    fn suppressed_queue_overflow_fails_open_for_the_current_event() {
        assert_eq!(
            plan_queue_send(true, false, QueueSendOutcome::Full),
            QueueSendPlan::FailSafePassThrough(CaptureFault::QueueOverflow)
        );
    }

    #[test]
    fn local_motion_overflow_remains_local_and_does_not_fail_the_session() {
        assert_eq!(
            plan_queue_send(false, true, QueueSendOutcome::Full),
            QueueSendPlan::DropAndPassThrough
        );
    }

    #[test]
    fn local_key_or_button_overflow_ends_the_queue_instead_of_losing_state() {
        assert_eq!(
            plan_queue_send(false, false, QueueSendOutcome::Full),
            QueueSendPlan::FailSafePassThrough(CaptureFault::QueueOverflow)
        );
        assert!(!is_droppable_motion(InputEvent::KeyRelease(0xE1)));
        assert!(!is_droppable_motion(InputEvent::ButtonRelease(
            nexkvm_input::MouseButton::Left
        )));
    }

    #[test]
    fn event_tap_disable_pseudo_events_are_reenabled_not_decoded_as_input() {
        assert_eq!(
            plan_native_callback(K_CG_EVENT_TAP_DISABLED_BY_TIMEOUT, false),
            NativeCallbackPlan::Reenable(TapDisableReason::Timeout)
        );
        assert_eq!(
            plan_native_callback(K_CG_EVENT_TAP_DISABLED_BY_USER_INPUT, false),
            NativeCallbackPlan::Reenable(TapDisableReason::UserInput)
        );
    }

    #[test]
    fn nexkvm_injected_events_are_passed_through_without_recapture() {
        assert!(is_nexkvm_injected_event(
            crate::NEXKVM_EVENT_SOURCE_USER_DATA
        ));
        assert!(!is_nexkvm_injected_event(0));
        assert!(!is_nexkvm_injected_event(i64::MAX));
    }

    #[test]
    fn final_capture_clone_restores_suppression_even_with_callback_state_alive() {
        let control = Arc::new(EventTapControl::default());
        control.suppressed.store(true, Ordering::SeqCst);
        let callback_keepalive = Arc::clone(&control);
        let lifecycle = Arc::new(CaptureLifecycle::for_test(Arc::clone(&control)));
        let capture = MacosInputCapture::from_test_parts(Arc::clone(&lifecycle));
        let capture_clone = capture.clone();

        drop(capture);
        assert!(control.suppressed.load(Ordering::SeqCst));
        drop(capture_clone);
        drop(lifecycle);

        assert!(!control.suppressed.load(Ordering::SeqCst));
        assert!(control.shutdown.load(Ordering::SeqCst));
        assert_eq!(Arc::strong_count(&control), 2);
        drop(callback_keepalive);
    }

    #[tokio::test]
    async fn capture_health_fault_ends_the_forwarder_before_stale_events_escape() {
        let control = Arc::new(EventTapControl::default());
        let capture = MacosInputCapture::with_control_and_events(
            Arc::clone(&control),
            vec![InputEvent::KeyPress(0x04)],
        );
        control.record_fault(CaptureFault::QueueOverflow);

        let error = capture.next_event().await.unwrap_err();
        assert!(matches!(error, InputError::Backend(message) if message.contains("overflow")));
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn idle_capture_does_not_block_the_async_runtime() {
        let (sender, receiver) = mpsc::channel(1);
        let control = Arc::new(EventTapControl::default());
        let capture = MacosInputCapture {
            accessibility_trusted: true,
            receiver: Some(Arc::new(Mutex::new(receiver))),
            lifecycle: Arc::new(CaptureLifecycle::for_test(control)),
        };

        let result =
            tokio::time::timeout(std::time::Duration::from_millis(20), capture.next_event()).await;

        assert!(result.is_err(), "an idle capture should remain pending");
        drop(sender);
    }
}
