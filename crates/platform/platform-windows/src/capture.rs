//! Windows capture mapping: low-level hook payloads -> [`InputEvent`].

#![allow(unsafe_code)]

use async_trait::async_trait;
use nexkvm_input::{InputCapture, InputError, InputEvent, MouseButton};
use std::fmt;
use std::mem::MaybeUninit;
use std::ptr;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, GetSystemMetrics, HHOOK, KBDLLHOOKSTRUCT, MSG, MSLLHOOKSTRUCT,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN, WM_KEYUP,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

/// Windows low-level hook event kinds relevant to input sharing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WinCaptureEventKind {
    /// `WM_MOUSEMOVE`.
    MouseMove,
    /// `WM_LBUTTONDOWN`.
    LeftDown,
    /// `WM_LBUTTONUP`.
    LeftUp,
    /// `WM_RBUTTONDOWN`.
    RightDown,
    /// `WM_RBUTTONUP`.
    RightUp,
    /// `WM_MBUTTONDOWN`.
    MiddleDown,
    /// `WM_MBUTTONUP`.
    MiddleUp,
    /// `WM_MOUSEWHEEL`.
    Wheel,
    /// `WM_MOUSEHWHEEL`.
    HWheel,
    /// `WM_KEYDOWN` / `WM_SYSKEYDOWN`.
    KeyDown,
    /// `WM_KEYUP` / `WM_SYSKEYUP`.
    KeyUp,
}

/// Testable snapshot of Windows hook fields needed for capture.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CapturedWinInputEvent {
    /// Hook event kind.
    pub kind: WinCaptureEventKind,
    /// Cursor x position in virtual-screen pixels.
    pub x: Option<i32>,
    /// Cursor y position in virtual-screen pixels.
    pub y: Option<i32>,
    /// Virtual-screen width in pixels.
    pub width: Option<i32>,
    /// Virtual-screen height in pixels.
    pub height: Option<i32>,
    /// USB HID usage id converted from the Windows scan code.
    pub keycode: Option<u32>,
    /// Wheel delta in Win32 `WHEEL_DELTA` units.
    pub wheel_delta: Option<i32>,
}

impl Default for CapturedWinInputEvent {
    fn default() -> Self {
        Self {
            kind: WinCaptureEventKind::MouseMove,
            x: None,
            y: None,
            width: None,
            height: None,
            keycode: None,
            wheel_delta: None,
        }
    }
}

/// Translate Windows hook fields into the platform-neutral input event.
#[must_use]
pub fn plan_capture_event(event: CapturedWinInputEvent) -> Option<InputEvent> {
    match event.kind {
        WinCaptureEventKind::MouseMove => Some(InputEvent::PointerMove {
            x: normalize_axis(event.x? as f64, event.width? as f64),
            y: normalize_axis(event.y? as f64, event.height? as f64),
        }),
        WinCaptureEventKind::LeftDown => Some(InputEvent::ButtonPress(MouseButton::Left)),
        WinCaptureEventKind::LeftUp => Some(InputEvent::ButtonRelease(MouseButton::Left)),
        WinCaptureEventKind::RightDown => Some(InputEvent::ButtonPress(MouseButton::Right)),
        WinCaptureEventKind::RightUp => Some(InputEvent::ButtonRelease(MouseButton::Right)),
        WinCaptureEventKind::MiddleDown => Some(InputEvent::ButtonPress(MouseButton::Middle)),
        WinCaptureEventKind::MiddleUp => Some(InputEvent::ButtonRelease(MouseButton::Middle)),
        WinCaptureEventKind::Wheel => Some(InputEvent::Scroll {
            dx: 0.0,
            dy: wheel_units(event.wheel_delta?),
        }),
        WinCaptureEventKind::HWheel => Some(InputEvent::Scroll {
            dx: wheel_units(event.wheel_delta?),
            dy: 0.0,
        }),
        WinCaptureEventKind::KeyDown => event.keycode.map(InputEvent::KeyPress),
        WinCaptureEventKind::KeyUp => event.keycode.map(InputEvent::KeyRelease),
    }
}

fn normalize_axis(value: f64, max: f64) -> f64 {
    if max <= 1.0 {
        return 0.0;
    }
    (value / max).clamp(0.0, 1.0)
}

fn wheel_units(delta: i32) -> f64 {
    delta as f64 / 120.0
}

fn scan_to_hid_keycode(scan_code: u32) -> Option<u32> {
    match scan_code {
        0x1e => Some(0x04), // A
        0x30 => Some(0x05), // B
        0x2e => Some(0x06), // C
        0x20 => Some(0x07), // D
        0x12 => Some(0x08), // E
        0x21 => Some(0x09), // F
        0x22 => Some(0x0A), // G
        0x23 => Some(0x0B), // H
        0x17 => Some(0x0C), // I
        0x24 => Some(0x0D), // J
        0x25 => Some(0x0E), // K
        0x26 => Some(0x0F), // L
        0x32 => Some(0x10), // M
        0x31 => Some(0x11), // N
        0x18 => Some(0x12), // O
        0x19 => Some(0x13), // P
        0x10 => Some(0x14), // Q
        0x13 => Some(0x15), // R
        0x1f => Some(0x16), // S
        0x14 => Some(0x17), // T
        0x16 => Some(0x18), // U
        0x2f => Some(0x19), // V
        0x11 => Some(0x1A), // W
        0x2d => Some(0x1B), // X
        0x15 => Some(0x1C), // Y
        0x2c => Some(0x1D), // Z
        0x01 => Some(0x29), // Escape
        0x39 => Some(0x2C), // Space
        _ => None,
    }
}

/// Windows input capture backed by low-level mouse and keyboard hooks.
#[derive(Clone)]
pub struct WindowsInputCapture {
    receiver: Arc<Mutex<Receiver<Result<InputEvent, InputError>>>>,
}

impl WindowsInputCapture {
    /// Start the Win32 low-level hook capture thread.
    #[must_use]
    pub fn new() -> Self {
        Self {
            receiver: start_low_level_hook_capture(),
        }
    }

    #[cfg(test)]
    fn with_events(events: Vec<InputEvent>) -> Self {
        let (sender, receiver) = mpsc::channel();
        for event in events {
            sender.send(Ok(event)).expect("send fixture event");
        }
        Self {
            receiver: Arc::new(Mutex::new(receiver)),
        }
    }
}

impl Default for WindowsInputCapture {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for WindowsInputCapture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WindowsInputCapture")
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl InputCapture for WindowsInputCapture {
    async fn next_event(&self) -> Result<InputEvent, InputError> {
        self.receiver
            .lock()
            .map_err(|_| InputError::Backend("Windows capture receiver lock poisoned".into()))?
            .recv()
            .map_err(|_| {
                InputError::Backend("Windows low-level hook capture loop stopped".into())
            })?
    }
}

type CaptureResultSender = Sender<Result<InputEvent, InputError>>;

static CAPTURE_SENDER: OnceLock<Mutex<Option<CaptureResultSender>>> = OnceLock::new();

fn capture_sender_slot() -> &'static Mutex<Option<CaptureResultSender>> {
    CAPTURE_SENDER.get_or_init(|| Mutex::new(None))
}

fn start_low_level_hook_capture() -> Arc<Mutex<Receiver<Result<InputEvent, InputError>>>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || run_low_level_hooks(sender));
    Arc::new(Mutex::new(receiver))
}

fn run_low_level_hooks(sender: CaptureResultSender) {
    match capture_sender_slot().lock() {
        Ok(mut slot) => {
            *slot = Some(sender.clone());
        }
        Err(_) => {
            let _ = sender.send(Err(InputError::Backend(
                "Windows capture sender lock poisoned".into(),
            )));
            return;
        }
    }

    // SAFETY: Low-level hooks are installed for the current process with a null
    // module handle and thread id 0, which is the documented global low-level
    // hook setup for WH_*_LL callbacks.
    let keyboard_hook =
        unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), ptr::null_mut(), 0) };
    // SAFETY: Same low-level hook setup as above, with a mouse callback that
    // only reads the `MSLLHOOKSTRUCT` provided for the callback duration.
    let mouse_hook =
        unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), ptr::null_mut(), 0) };

    if keyboard_hook.is_null() || mouse_hook.is_null() {
        if !keyboard_hook.is_null() {
            // SAFETY: Hook handle was returned by SetWindowsHookExW.
            unsafe { UnhookWindowsHookEx(keyboard_hook) };
        }
        if !mouse_hook.is_null() {
            // SAFETY: Hook handle was returned by SetWindowsHookExW.
            unsafe { UnhookWindowsHookEx(mouse_hook) };
        }
        clear_capture_sender();
        let _ = sender.send(Err(InputError::Backend(
            "Windows low-level input hooks could not be installed".into(),
        )));
        return;
    }

    message_loop();

    // SAFETY: Hook handles were returned by SetWindowsHookExW and are still
    // owned by this thread.
    unsafe {
        UnhookWindowsHookEx(keyboard_hook);
        UnhookWindowsHookEx(mouse_hook);
    }
    clear_capture_sender();
}

fn message_loop() {
    let mut msg = MaybeUninit::<MSG>::zeroed();
    loop {
        // SAFETY: `msg` points to writable MSG storage and hwnd is null to
        // receive thread messages for this hook-owning thread.
        let result = unsafe { GetMessageW(msg.as_mut_ptr(), ptr::null_mut(), 0, 0) };
        if result <= 0 {
            break;
        }
    }
}

fn clear_capture_sender() {
    if let Ok(mut slot) = capture_sender_slot().lock() {
        *slot = None;
    }
}

unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        // SAFETY: For WH_KEYBOARD_LL callbacks, lparam points to a valid
        // KBDLLHOOKSTRUCT for the duration of this call.
        let data = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        if let Some(captured) = captured_keyboard_event(wparam as u32, data.scanCode) {
            send_captured_event(captured);
        }
    }
    // SAFETY: The hook chain must be continued with the original callback args.
    unsafe {
        CallNextHookEx(
            ptr::null_mut::<core::ffi::c_void>() as HHOOK,
            code,
            wparam,
            lparam,
        )
    }
}

unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        // SAFETY: For WH_MOUSE_LL callbacks, lparam points to a valid
        // MSLLHOOKSTRUCT for the duration of this call.
        let data = unsafe { &*(lparam as *const MSLLHOOKSTRUCT) };
        if let Some(captured) = captured_mouse_event(wparam as u32, data) {
            send_captured_event(captured);
        }
    }
    // SAFETY: The hook chain must be continued with the original callback args.
    unsafe {
        CallNextHookEx(
            ptr::null_mut::<core::ffi::c_void>() as HHOOK,
            code,
            wparam,
            lparam,
        )
    }
}

fn send_captured_event(captured: CapturedWinInputEvent) {
    let Some(event) = plan_capture_event(captured) else {
        return;
    };
    let Ok(slot) = capture_sender_slot().lock() else {
        return;
    };
    if let Some(sender) = slot.as_ref() {
        let _ = sender.send(Ok(event));
    }
}

fn captured_keyboard_event(message: u32, scan_code: u32) -> Option<CapturedWinInputEvent> {
    let kind = match message {
        WM_KEYDOWN | WM_SYSKEYDOWN => WinCaptureEventKind::KeyDown,
        WM_KEYUP | WM_SYSKEYUP => WinCaptureEventKind::KeyUp,
        _ => return None,
    };
    Some(CapturedWinInputEvent {
        kind,
        keycode: scan_to_hid_keycode(scan_code),
        ..CapturedWinInputEvent::default()
    })
}

fn captured_mouse_event(message: u32, data: &MSLLHOOKSTRUCT) -> Option<CapturedWinInputEvent> {
    let kind = match message {
        WM_MOUSEMOVE => WinCaptureEventKind::MouseMove,
        WM_LBUTTONDOWN => WinCaptureEventKind::LeftDown,
        WM_LBUTTONUP => WinCaptureEventKind::LeftUp,
        WM_RBUTTONDOWN => WinCaptureEventKind::RightDown,
        WM_RBUTTONUP => WinCaptureEventKind::RightUp,
        WM_MBUTTONDOWN => WinCaptureEventKind::MiddleDown,
        WM_MBUTTONUP => WinCaptureEventKind::MiddleUp,
        WM_MOUSEWHEEL => WinCaptureEventKind::Wheel,
        WM_MOUSEHWHEEL => WinCaptureEventKind::HWheel,
        _ => return None,
    };
    let metrics = virtual_screen_metrics();
    Some(CapturedWinInputEvent {
        kind,
        x: Some(data.pt.x - metrics.x),
        y: Some(data.pt.y - metrics.y),
        width: Some(metrics.width),
        height: Some(metrics.height),
        wheel_delta: Some(wheel_delta_from_mouse_data(data.mouseData)),
        ..CapturedWinInputEvent::default()
    })
}

#[derive(Debug, Clone, Copy)]
struct VirtualScreenMetrics {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

fn virtual_screen_metrics() -> VirtualScreenMetrics {
    // SAFETY: GetSystemMetrics is read-only and these indexes are valid system
    // metric constants.
    unsafe {
        VirtualScreenMetrics {
            x: GetSystemMetrics(SM_XVIRTUALSCREEN),
            y: GetSystemMetrics(SM_YVIRTUALSCREEN),
            width: GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1),
            height: GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1),
        }
    }
}

fn wheel_delta_from_mouse_data(mouse_data: u32) -> i32 {
    ((mouse_data >> 16) as u16 as i16) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mouse_move_normalizes_to_pointer_move() {
        assert_eq!(
            plan_capture_event(CapturedWinInputEvent {
                kind: WinCaptureEventKind::MouseMove,
                x: Some(960),
                y: Some(540),
                width: Some(1920),
                height: Some(1080),
                ..CapturedWinInputEvent::default()
            }),
            Some(InputEvent::PointerMove { x: 0.5, y: 0.5 })
        );
    }

    #[test]
    fn button_and_key_events_map_to_input_events() {
        assert_eq!(
            plan_capture_event(CapturedWinInputEvent {
                kind: WinCaptureEventKind::LeftDown,
                ..CapturedWinInputEvent::default()
            }),
            Some(InputEvent::ButtonPress(MouseButton::Left))
        );
        assert_eq!(
            plan_capture_event(CapturedWinInputEvent {
                kind: WinCaptureEventKind::KeyUp,
                keycode: Some(0x04),
                ..CapturedWinInputEvent::default()
            }),
            Some(InputEvent::KeyRelease(0x04))
        );
    }

    #[test]
    fn wheel_delta_converts_to_scroll_units() {
        assert_eq!(
            plan_capture_event(CapturedWinInputEvent {
                kind: WinCaptureEventKind::Wheel,
                wheel_delta: Some(120),
                ..CapturedWinInputEvent::default()
            }),
            Some(InputEvent::Scroll { dx: 0.0, dy: 1.0 })
        );
    }

    #[test]
    fn scan_codes_map_to_hid_keycodes() {
        assert_eq!(scan_to_hid_keycode(0x1e), Some(0x04));
        assert_eq!(scan_to_hid_keycode(0x39), Some(0x2C));
        assert_eq!(scan_to_hid_keycode(0xff), None);
    }

    #[test]
    fn mouse_wheel_delta_extracts_signed_high_word() {
        assert_eq!(wheel_delta_from_mouse_data(120u32 << 16), 120);
        assert_eq!(
            wheel_delta_from_mouse_data((-120i16 as u16 as u32) << 16),
            -120
        );
    }

    #[tokio::test]
    async fn capture_returns_queued_event() {
        let capture = WindowsInputCapture::with_events(vec![InputEvent::KeyPress(0x04)]);

        assert_eq!(
            capture.next_event().await.unwrap(),
            InputEvent::KeyPress(0x04)
        );
    }
}
