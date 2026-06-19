//! macOS injection mapping: [`InjectionCommand`] → Quartz `CGEvent` plan.
//!
//! The backend posts events with `CGEventPost`/`CGEventCreateMouseEvent`/
//! `CGEventCreateKeyboardEvent` once Accessibility permission is granted. The
//! pure planning layer stays testable, while native FFI is isolated in
//! [`NativeEventPoster`].
//!
//! # Keycode caveat
//! [`InjectionCommand::Key`] carries an OS-neutral USB HID usage id. macOS
//! `CGEvent` keyboard events use `CGKeyCode` (ANSI virtual keycodes), which are
//! a different namespace. The FFI layer must map HID → `CGKeyCode` via a lookup
//! table (plus layout via `UCKeyTranslate`); this module passes the keycode
//! through unchanged and records the intended event type.

#![allow(unsafe_code)]

use async_trait::async_trait;
use nexkvm_input::{InjectionCommand, InputError, InputEvent, InputInjector, MouseButton};
use std::ffi::c_void;
use std::fmt;
use std::ptr;
use std::sync::Arc;

/// Quartz `CGEventType` values relevant to injection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum CgEventType {
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
    /// `kCGEventScrollWheel`.
    ScrollWheel = 22,
    /// `kCGEventOtherMouseDown`.
    OtherMouseDown = 25,
    /// `kCGEventOtherMouseUp`.
    OtherMouseUp = 26,
    /// `kCGEventKeyDown`.
    KeyDown = 10,
    /// `kCGEventKeyUp`.
    KeyUp = 11,
}

/// `CGMouseButton` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum CgMouseButton {
    /// `kCGMouseButtonLeft`.
    Left = 0,
    /// `kCGMouseButtonRight`.
    Right = 1,
    /// `kCGMouseButtonCenter`.
    Center = 2,
}

/// A concrete `CGEvent` to post, derived from an [`InjectionCommand`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CgEventPlan {
    /// Warp the cursor to a normalized `[0,1]` position, then post `MouseMoved`.
    WarpAbsolute {
        /// Horizontal position.
        x: f64,
        /// Vertical position.
        y: f64,
    },
    /// Post a `MouseMoved` with a screen-fraction delta (scaled at FFI time).
    MoveRelative {
        /// Horizontal delta.
        dx: f64,
        /// Vertical delta.
        dy: f64,
    },
    /// Post a `MouseMoved` with a raw device-count delta.
    MoveRaw {
        /// Horizontal delta in device units.
        dx: i32,
        /// Vertical delta in device units.
        dy: i32,
    },
    /// Post a mouse button event.
    MouseButton {
        /// The `CGEventType` (down/up variant).
        event_type: CgEventType,
        /// The `CGMouseButton`.
        button: CgMouseButton,
    },
    /// Post a scroll-wheel event (line units).
    Scroll {
        /// Horizontal delta.
        dx: f64,
        /// Vertical delta.
        dy: f64,
    },
    /// Post a keyboard event. `keycode` is still a USB HID usage id; the FFI
    /// layer maps it to a `CGKeyCode`.
    Key {
        /// `KeyDown` or `KeyUp`.
        event_type: CgEventType,
        /// HID usage id to be translated to a `CGKeyCode`.
        keycode: u32,
    },
}

/// Translate a neutral [`InjectionCommand`] into a macOS [`CgEventPlan`].
#[must_use]
pub fn plan(command: InjectionCommand) -> CgEventPlan {
    match command {
        InjectionCommand::MoveAbsolute { x, y } => CgEventPlan::WarpAbsolute { x, y },
        InjectionCommand::MoveRelative { dx, dy } => CgEventPlan::MoveRelative { dx, dy },
        InjectionCommand::MoveRaw { dx, dy } => CgEventPlan::MoveRaw { dx, dy },
        InjectionCommand::Button { button, pressed } => {
            let (event_type, button) = match button {
                MouseButton::Left if pressed => (CgEventType::LeftMouseDown, CgMouseButton::Left),
                MouseButton::Left => (CgEventType::LeftMouseUp, CgMouseButton::Left),
                MouseButton::Right if pressed => {
                    (CgEventType::RightMouseDown, CgMouseButton::Right)
                }
                MouseButton::Right => (CgEventType::RightMouseUp, CgMouseButton::Right),
                MouseButton::Middle if pressed => {
                    (CgEventType::OtherMouseDown, CgMouseButton::Center)
                }
                MouseButton::Middle => (CgEventType::OtherMouseUp, CgMouseButton::Center),
            };
            CgEventPlan::MouseButton { event_type, button }
        }
        InjectionCommand::Scroll { dx, dy } => CgEventPlan::Scroll { dx, dy },
        InjectionCommand::Key { keycode, pressed } => CgEventPlan::Key {
            event_type: if pressed {
                CgEventType::KeyDown
            } else {
                CgEventType::KeyUp
            },
            keycode,
        },
    }
}

trait EventPoster: Send + Sync {
    fn post(&self, event_plan: CgEventPlan) -> Result<(), InputError>;
}

#[derive(Debug, Clone, Copy, Default)]
struct NativeEventPoster;

impl EventPoster for NativeEventPoster {
    fn post(&self, event_plan: CgEventPlan) -> Result<(), InputError> {
        post_plan(event_plan)
    }
}

/// macOS input injector boundary.
#[derive(Clone)]
pub struct MacosInputInjector {
    accessibility_trusted: bool,
    poster: Arc<dyn EventPoster>,
}

impl MacosInputInjector {
    /// Create an injector with the current Accessibility trust state.
    #[must_use]
    pub fn new(accessibility_trusted: bool) -> Self {
        Self {
            accessibility_trusted,
            poster: Arc::new(NativeEventPoster),
        }
    }

    #[cfg(test)]
    fn with_poster(accessibility_trusted: bool, poster: Arc<dyn EventPoster>) -> Self {
        Self {
            accessibility_trusted,
            poster,
        }
    }
}

impl fmt::Debug for MacosInputInjector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MacosInputInjector")
            .field("accessibility_trusted", &self.accessibility_trusted)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl InputInjector for MacosInputInjector {
    async fn inject(&self, event: InputEvent) -> Result<(), InputError> {
        if !self.accessibility_trusted {
            return Err(InputError::PermissionDenied);
        }
        let command = event.to_injection_command();
        self.poster.post(plan(command))
    }
}

type CGEventRef = *mut c_void;
type CGEventSourceRef = *const c_void;
type CFTypeRef = *const c_void;
type CGDirectDisplayID = u32;

const K_CG_HID_EVENT_TAP: u32 = 0;
const K_CG_SCROLL_EVENT_UNIT_LINE: u32 = 1;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn CGEventCreate(source: CGEventSourceRef) -> CGEventRef;
    fn CGEventGetLocation(event: CGEventRef) -> CGPoint;
    fn CGEventCreateKeyboardEvent(
        source: CGEventSourceRef,
        virtual_key: u16,
        key_down: bool,
    ) -> CGEventRef;
    fn CGEventCreateMouseEvent(
        source: CGEventSourceRef,
        mouse_type: u32,
        mouse_cursor_position: CGPoint,
        mouse_button: u32,
    ) -> CGEventRef;
    fn CGEventCreateScrollWheelEvent(
        source: CGEventSourceRef,
        units: u32,
        wheel_count: u32,
        ...
    ) -> CGEventRef;
    fn CGEventPost(tap: u32, event: CGEventRef);
    fn CGMainDisplayID() -> CGDirectDisplayID;
    fn CGDisplayPixelsWide(display: CGDirectDisplayID) -> usize;
    fn CGDisplayPixelsHigh(display: CGDirectDisplayID) -> usize;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: CFTypeRef);
}

fn post_plan(event_plan: CgEventPlan) -> Result<(), InputError> {
    match event_plan {
        CgEventPlan::WarpAbsolute { x, y } => {
            let point = normalized_to_main_display_point(x, y);
            let event = create_mouse_event(CgEventType::MouseMoved, point, CgMouseButton::Left)?;
            post_event(event);
            Ok(())
        }
        CgEventPlan::MoveRelative { .. } | CgEventPlan::MoveRaw { .. } => Err(InputError::Backend(
            "macOS relative/raw motion posting is not wired yet".into(),
        )),
        CgEventPlan::MouseButton { event_type, button } => {
            let point = current_mouse_location();
            let event = create_mouse_event(event_type, point, button)?;
            post_event(event);
            Ok(())
        }
        CgEventPlan::Scroll { dx, dy } => {
            let event = create_scroll_event(dx, dy)?;
            post_event(event);
            Ok(())
        }
        CgEventPlan::Key {
            event_type,
            keycode,
        } => {
            let Some(cg_keycode) = hid_to_cg_keycode(keycode) else {
                return Err(InputError::Backend(format!(
                    "unsupported macOS HID keycode: {keycode}"
                )));
            };
            let event = create_key_event(event_type, cg_keycode)?;
            post_event(event);
            Ok(())
        }
    }
}

fn create_key_event(event_type: CgEventType, keycode: u16) -> Result<CGEventRef, InputError> {
    let key_down = matches!(event_type, CgEventType::KeyDown);
    // SAFETY: Passing a null source uses the default event source. The keycode
    // is a macOS virtual key code from `hid_to_cg_keycode`.
    let event = unsafe { CGEventCreateKeyboardEvent(ptr::null(), keycode, key_down) };
    non_null_event(event, "CGEventCreateKeyboardEvent")
}

fn create_mouse_event(
    event_type: CgEventType,
    point: CGPoint,
    button: CgMouseButton,
) -> Result<CGEventRef, InputError> {
    // SAFETY: Passing a null source uses the default event source. `event_type`
    // and `button` are constrained enums matching CoreGraphics constants.
    let event =
        unsafe { CGEventCreateMouseEvent(ptr::null(), event_type as u32, point, button as u32) };
    non_null_event(event, "CGEventCreateMouseEvent")
}

fn create_scroll_event(dx: f64, dy: f64) -> Result<CGEventRef, InputError> {
    // SAFETY: Passing a null source uses the default event source. CoreGraphics
    // accepts two line-unit wheels: vertical then horizontal.
    let event = unsafe {
        CGEventCreateScrollWheelEvent(
            ptr::null(),
            K_CG_SCROLL_EVENT_UNIT_LINE,
            2,
            dy.round() as i32,
            dx.round() as i32,
        )
    };
    non_null_event(event, "CGEventCreateScrollWheelEvent")
}

fn non_null_event(event: CGEventRef, operation: &'static str) -> Result<CGEventRef, InputError> {
    if event.is_null() {
        Err(InputError::Backend(format!("{operation} returned null")))
    } else {
        Ok(event)
    }
}

fn post_event(event: CGEventRef) {
    // SAFETY: `event` is a non-null CoreGraphics object created by one of the
    // `CGEventCreate*` functions above. It is posted, then released exactly once.
    unsafe {
        CGEventPost(K_CG_HID_EVENT_TAP, event);
        CFRelease(event.cast());
    }
}

fn current_mouse_location() -> CGPoint {
    // SAFETY: A null source uses the default event source. If creation fails,
    // fall back to origin; callers still receive a valid mouse event object or a
    // create failure from `create_mouse_event`.
    let event = unsafe { CGEventCreate(ptr::null()) };
    if event.is_null() {
        return CGPoint { x: 0.0, y: 0.0 };
    }
    // SAFETY: `event` is non-null and valid until released below.
    let point = unsafe { CGEventGetLocation(event) };
    // SAFETY: `event` follows CoreFoundation create/copy ownership rules.
    unsafe {
        CFRelease(event.cast());
    }
    point
}

fn normalized_to_main_display_point(x: f64, y: f64) -> CGPoint {
    // SAFETY: Display metadata functions are read-only and return current main
    // display dimensions. Zero sizes are guarded with `max(1)`.
    let (width, height) = unsafe {
        let display = CGMainDisplayID();
        (
            CGDisplayPixelsWide(display).max(1) as f64,
            CGDisplayPixelsHigh(display).max(1) as f64,
        )
    };
    CGPoint {
        x: x.clamp(0.0, 1.0) * (width - 1.0),
        y: y.clamp(0.0, 1.0) * (height - 1.0),
    }
}

fn hid_to_cg_keycode(keycode: u32) -> Option<u16> {
    match keycode {
        0x04 => Some(0),  // A
        0x05 => Some(11), // B
        0x06 => Some(8),  // C
        0x07 => Some(2),  // D
        0x08 => Some(14), // E
        0x09 => Some(3),  // F
        0x0A => Some(5),  // G
        0x0B => Some(4),  // H
        0x0C => Some(34), // I
        0x0D => Some(38), // J
        0x0E => Some(40), // K
        0x0F => Some(37), // L
        0x10 => Some(46), // M
        0x11 => Some(45), // N
        0x12 => Some(31), // O
        0x13 => Some(35), // P
        0x14 => Some(12), // Q
        0x15 => Some(15), // R
        0x16 => Some(1),  // S
        0x17 => Some(17), // T
        0x18 => Some(32), // U
        0x19 => Some(9),  // V
        0x1A => Some(13), // W
        0x1B => Some(7),  // X
        0x1C => Some(16), // Y
        0x1D => Some(6),  // Z
        0x29 => Some(53), // Escape
        0x2C => Some(49), // Space
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Debug, Default)]
    struct RecordingPoster {
        events: Mutex<Vec<CgEventPlan>>,
    }

    impl EventPoster for RecordingPoster {
        fn post(&self, event_plan: CgEventPlan) -> Result<(), InputError> {
            self.events.lock().unwrap().push(event_plan);
            Ok(())
        }
    }

    impl RecordingPoster {
        fn events(&self) -> Vec<CgEventPlan> {
            self.events.lock().unwrap().clone()
        }
    }

    #[test]
    fn absolute_move_warps_cursor() {
        assert_eq!(
            plan(InjectionCommand::MoveAbsolute { x: 0.5, y: 0.25 }),
            CgEventPlan::WarpAbsolute { x: 0.5, y: 0.25 }
        );
    }

    #[test]
    fn left_press_and_release_map_to_down_up() {
        assert_eq!(
            plan(InjectionCommand::Button {
                button: MouseButton::Left,
                pressed: true
            }),
            CgEventPlan::MouseButton {
                event_type: CgEventType::LeftMouseDown,
                button: CgMouseButton::Left
            }
        );
        assert_eq!(
            plan(InjectionCommand::Button {
                button: MouseButton::Left,
                pressed: false
            }),
            CgEventPlan::MouseButton {
                event_type: CgEventType::LeftMouseUp,
                button: CgMouseButton::Left
            }
        );
    }

    #[test]
    fn middle_button_uses_other_mouse_center() {
        assert_eq!(
            plan(InjectionCommand::Button {
                button: MouseButton::Middle,
                pressed: true
            }),
            CgEventPlan::MouseButton {
                event_type: CgEventType::OtherMouseDown,
                button: CgMouseButton::Center
            }
        );
    }

    #[test]
    fn key_release_maps_to_keyup_with_passthrough_keycode() {
        assert_eq!(
            plan(InjectionCommand::Key {
                keycode: 0x04,
                pressed: false
            }),
            CgEventPlan::Key {
                event_type: CgEventType::KeyUp,
                keycode: 0x04
            }
        );
    }

    #[tokio::test]
    async fn injector_refuses_without_accessibility_permission() {
        let injector = MacosInputInjector::new(false);
        let result = injector
            .inject(nexkvm_input::InputEvent::KeyPress(0x04))
            .await;

        assert!(matches!(
            result,
            Err(nexkvm_input::InputError::PermissionDenied)
        ));
    }

    #[tokio::test]
    async fn injector_accepts_supported_event_when_accessibility_is_ready() {
        let poster = Arc::new(RecordingPoster::default());
        let injector = MacosInputInjector::with_poster(true, poster.clone());

        injector
            .inject(nexkvm_input::InputEvent::ButtonPress(MouseButton::Left))
            .await
            .unwrap();
        assert_eq!(
            poster.events(),
            vec![CgEventPlan::MouseButton {
                event_type: CgEventType::LeftMouseDown,
                button: CgMouseButton::Left
            }]
        );
    }

    #[test]
    fn maps_mvp_hid_keycodes_to_macos_virtual_keycodes() {
        assert_eq!(hid_to_cg_keycode(0x04), Some(0));
        assert_eq!(hid_to_cg_keycode(0x29), Some(53));
        assert_eq!(hid_to_cg_keycode(0xFFFF), None);
    }
}
