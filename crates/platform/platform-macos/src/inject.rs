//! macOS injection mapping: [`InjectionCommand`] → Quartz `CGEvent` plan.
//!
//! The backend posts events with `CGEventPost`/`CGEventCreateMouseEvent`/
//! `CGEventCreateKeyboardEvent` once Accessibility permission is granted. The
//! pure planning layer stays testable, while native FFI is isolated in
//! `NativeEventPoster`.
//!
//! [`InjectionCommand::Key`] carries an OS-neutral USB HID usage id. macOS
//! `CGEvent` keyboard events use a different `CGKeyCode` namespace, so the FFI
//! layer maps every supported HID usage through an explicit lookup table.

#![allow(unsafe_code)]

use async_trait::async_trait;
use nexkvm_input::{InjectionCommand, InputError, InputEvent, InputInjector, MouseButton};
use std::ffi::c_void;
use std::fmt;
use std::ptr;
use std::sync::{Arc, Mutex};

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
    /// `kCGEventLeftMouseDragged`.
    LeftMouseDragged = 6,
    /// `kCGEventRightMouseDragged`.
    RightMouseDragged = 7,
    /// `kCGEventScrollWheel`.
    ScrollWheel = 22,
    /// `kCGEventOtherMouseDown`.
    OtherMouseDown = 25,
    /// `kCGEventOtherMouseUp`.
    OtherMouseUp = 26,
    /// `kCGEventOtherMouseDragged`.
    OtherMouseDragged = 27,
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

#[derive(Debug, Default)]
struct NativeEventPoster {
    pressed_buttons: Mutex<Vec<CgMouseButton>>,
}

impl EventPoster for NativeEventPoster {
    fn post(&self, event_plan: CgEventPlan) -> Result<(), InputError> {
        let mut pressed = self
            .pressed_buttons
            .lock()
            .map_err(|_| InputError::Backend("macOS mouse-button state lock poisoned".into()))?;
        let held_button = pressed.last().copied();
        post_plan(event_plan, held_button)?;
        if let CgEventPlan::MouseButton { event_type, button } = event_plan {
            update_pressed_buttons(&mut pressed, event_type, button);
        }
        Ok(())
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
            poster: Arc::new(NativeEventPoster::default()),
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
const K_CG_EVENT_SOURCE_USER_DATA: u32 = 42;

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
    fn CGEventSetIntegerValueField(event: CGEventRef, field: u32, value: i64);
    fn CGMainDisplayID() -> CGDirectDisplayID;
    fn CGDisplayBounds(display: CGDirectDisplayID) -> CGRect;
    fn CGGetActiveDisplayList(
        max_displays: u32,
        active_displays: *mut CGDirectDisplayID,
        display_count: *mut u32,
    ) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: CFTypeRef);
}

fn post_plan(
    event_plan: CgEventPlan,
    held_button: Option<CgMouseButton>,
) -> Result<(), InputError> {
    match event_plan {
        CgEventPlan::WarpAbsolute { x, y } => {
            let point = normalized_to_desktop_point(x, y)?;
            let (event_type, button) = motion_event_type(held_button);
            let event = create_mouse_event(event_type, point, button)?;
            post_event(event);
            Ok(())
        }
        CgEventPlan::MoveRelative { dx, dy } => {
            let point = moved_point_on_desktop(current_mouse_location(), dx, dy, true)?;
            let (event_type, button) = motion_event_type(held_button);
            let event = create_mouse_event(event_type, point, button)?;
            post_event(event);
            Ok(())
        }
        CgEventPlan::MoveRaw { dx, dy } => {
            let point = moved_point_on_desktop(
                current_mouse_location(),
                f64::from(dx),
                f64::from(dy),
                false,
            )?;
            let (event_type, button) = motion_event_type(held_button);
            let event = create_mouse_event(event_type, point, button)?;
            post_event(event);
            Ok(())
        }
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

fn motion_event_type(held_button: Option<CgMouseButton>) -> (CgEventType, CgMouseButton) {
    match held_button {
        Some(CgMouseButton::Left) => (CgEventType::LeftMouseDragged, CgMouseButton::Left),
        Some(CgMouseButton::Right) => (CgEventType::RightMouseDragged, CgMouseButton::Right),
        Some(CgMouseButton::Center) => (CgEventType::OtherMouseDragged, CgMouseButton::Center),
        None => (CgEventType::MouseMoved, CgMouseButton::Left),
    }
}

fn update_pressed_buttons(
    pressed: &mut Vec<CgMouseButton>,
    event_type: CgEventType,
    button: CgMouseButton,
) {
    let is_down = matches!(
        event_type,
        CgEventType::LeftMouseDown | CgEventType::RightMouseDown | CgEventType::OtherMouseDown
    );
    if is_down {
        if !pressed.contains(&button) {
            pressed.push(button);
        }
    } else if let Some(index) = pressed.iter().rposition(|held| *held == button) {
        pressed.remove(index);
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

fn native_post_user_data() -> i64 {
    crate::NEXKVM_EVENT_SOURCE_USER_DATA
}

fn post_event(event: CGEventRef) {
    // SAFETY: `event` is a non-null CoreGraphics object created by one of the
    // `CGEventCreate*` functions above. The documented source-user-data field
    // accepts an i64 marker. The event is posted, then released exactly once.
    unsafe {
        CGEventSetIntegerValueField(event, K_CG_EVENT_SOURCE_USER_DATA, native_post_user_data());
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

fn normalized_to_desktop_point(x: f64, y: f64) -> Result<CGPoint, InputError> {
    if !x.is_finite() || !y.is_finite() {
        return Err(InputError::Backend(
            "non-finite absolute mouse position".into(),
        ));
    }
    Ok(normalized_point_in_bounds(active_desktop_bounds(), x, y))
}

fn normalized_point_in_bounds(bounds: CGRect, x: f64, y: f64) -> CGPoint {
    CGPoint {
        x: bounds.origin.x + x.clamp(0.0, 1.0) * (bounds.size.width.max(1.0) - 1.0),
        y: bounds.origin.y + y.clamp(0.0, 1.0) * (bounds.size.height.max(1.0) - 1.0),
    }
}

fn moved_point_on_desktop(
    current: CGPoint,
    dx: f64,
    dy: f64,
    normalized_delta: bool,
) -> Result<CGPoint, InputError> {
    if !dx.is_finite() || !dy.is_finite() {
        return Err(InputError::Backend(
            "non-finite relative mouse delta".into(),
        ));
    }
    Ok(moved_point_in_bounds(
        current,
        active_desktop_bounds(),
        dx,
        dy,
        normalized_delta,
    ))
}

fn moved_point_in_bounds(
    current: CGPoint,
    bounds: CGRect,
    dx: f64,
    dy: f64,
    normalized_delta: bool,
) -> CGPoint {
    let width = bounds.size.width.max(1.0);
    let height = bounds.size.height.max(1.0);
    let scale_x = if normalized_delta { width } else { 1.0 };
    let scale_y = if normalized_delta { height } else { 1.0 };
    let maximum_x = bounds.origin.x + width - 1.0;
    let maximum_y = bounds.origin.y + height - 1.0;
    CGPoint {
        x: (current.x + dx * scale_x).clamp(bounds.origin.x, maximum_x),
        y: (current.y + dy * scale_y).clamp(bounds.origin.y, maximum_y),
    }
}

fn main_display_bounds() -> CGRect {
    // SAFETY: Display metadata is read-only and the main display id is valid.
    unsafe { CGDisplayBounds(CGMainDisplayID()) }
}

fn active_desktop_bounds() -> CGRect {
    const MAX_DISPLAYS: usize = 32;
    let mut displays = [0; MAX_DISPLAYS];
    let mut count = 0;
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

fn hid_to_cg_keycode(keycode: u32) -> Option<u16> {
    match keycode {
        0x04 => Some(0),   // A
        0x05 => Some(11),  // B
        0x06 => Some(8),   // C
        0x07 => Some(2),   // D
        0x08 => Some(14),  // E
        0x09 => Some(3),   // F
        0x0A => Some(5),   // G
        0x0B => Some(4),   // H
        0x0C => Some(34),  // I
        0x0D => Some(38),  // J
        0x0E => Some(40),  // K
        0x0F => Some(37),  // L
        0x10 => Some(46),  // M
        0x11 => Some(45),  // N
        0x12 => Some(31),  // O
        0x13 => Some(35),  // P
        0x14 => Some(12),  // Q
        0x15 => Some(15),  // R
        0x16 => Some(1),   // S
        0x17 => Some(17),  // T
        0x18 => Some(32),  // U
        0x19 => Some(9),   // V
        0x1A => Some(13),  // W
        0x1B => Some(7),   // X
        0x1C => Some(16),  // Y
        0x1D => Some(6),   // Z
        0x1E => Some(18),  // 1
        0x1F => Some(19),  // 2
        0x20 => Some(20),  // 3
        0x21 => Some(21),  // 4
        0x22 => Some(23),  // 5
        0x23 => Some(22),  // 6
        0x24 => Some(26),  // 7
        0x25 => Some(28),  // 8
        0x26 => Some(25),  // 9
        0x27 => Some(29),  // 0
        0x28 => Some(36),  // Return
        0x29 => Some(53),  // Escape
        0x2A => Some(51),  // Backspace
        0x2B => Some(48),  // Tab
        0x2C => Some(49),  // Space
        0x2D => Some(27),  // Minus
        0x2E => Some(24),  // Equal
        0x2F => Some(33),  // Left bracket
        0x30 => Some(30),  // Right bracket
        0x31 => Some(42),  // Backslash
        0x33 => Some(41),  // Semicolon
        0x34 => Some(39),  // Apostrophe
        0x35 => Some(50),  // Grave
        0x36 => Some(43),  // Comma
        0x37 => Some(47),  // Period
        0x38 => Some(44),  // Slash
        0x39 => Some(57),  // Caps Lock
        0x3A => Some(122), // F1
        0x3B => Some(120), // F2
        0x3C => Some(99),  // F3
        0x3D => Some(118), // F4
        0x3E => Some(96),  // F5
        0x3F => Some(97),  // F6
        0x40 => Some(98),  // F7
        0x41 => Some(100), // F8
        0x42 => Some(101), // F9
        0x43 => Some(109), // F10
        0x44 => Some(103), // F11
        0x45 => Some(111), // F12
        0x4A => Some(115), // Home
        0x4B => Some(116), // Page up
        0x4C => Some(117), // Forward delete
        0x4D => Some(119), // End
        0x4E => Some(121), // Page down
        0x4F => Some(124), // Right arrow
        0x50 => Some(123), // Left arrow
        0x51 => Some(125), // Down arrow
        0x52 => Some(126), // Up arrow
        0x54 => Some(75),  // Keypad divide
        0x55 => Some(67),  // Keypad multiply
        0x56 => Some(78),  // Keypad minus
        0x57 => Some(69),  // Keypad plus
        0x58 => Some(76),  // Keypad enter
        0x59 => Some(83),  // Keypad 1
        0x5A => Some(84),  // Keypad 2
        0x5B => Some(85),  // Keypad 3
        0x5C => Some(86),  // Keypad 4
        0x5D => Some(87),  // Keypad 5
        0x5E => Some(88),  // Keypad 6
        0x5F => Some(89),  // Keypad 7
        0x60 => Some(91),  // Keypad 8
        0x61 => Some(92),  // Keypad 9
        0x62 => Some(82),  // Keypad 0
        0x63 => Some(65),  // Keypad decimal
        0x67 => Some(81),  // Keypad equal
        0xE0 => Some(59),  // Left Control
        0xE1 => Some(56),  // Left Shift
        0xE2 => Some(58),  // Left Option
        0xE3 => Some(55),  // Left Command
        0xE4 => Some(62),  // Right Control
        0xE5 => Some(60),  // Right Shift
        0xE6 => Some(61),  // Right Option
        0xE7 => Some(54),  // Right Command
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_posted_native_event_uses_the_feedback_loop_marker() {
        assert_eq!(
            native_post_user_data(),
            crate::NEXKVM_EVENT_SOURCE_USER_DATA
        );
        assert_ne!(native_post_user_data(), 0);
    }
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
    fn logical_display_bounds_drive_absolute_and_relative_pointer_motion() {
        let bounds = CGRect {
            origin: CGPoint {
                x: -1512.0,
                y: 80.0,
            },
            size: CGSize {
                width: 1512.0,
                height: 982.0,
            },
        };

        assert_eq!(
            normalized_point_in_bounds(bounds, 1.0, 0.0),
            CGPoint { x: -1.0, y: 80.0 }
        );
        assert_eq!(
            moved_point_in_bounds(
                CGPoint {
                    x: -756.0,
                    y: 571.0,
                },
                bounds,
                0.5,
                -1.0,
                true,
            ),
            CGPoint { x: -1.0, y: 80.0 }
        );
        assert_eq!(
            moved_point_in_bounds(CGPoint { x: -10.0, y: 90.0 }, bounds, 40.0, -40.0, false,),
            CGPoint { x: -1.0, y: 80.0 }
        );
    }

    #[test]
    fn pointer_injection_uses_the_union_of_all_active_displays() {
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
        .expect("at least one active display");

        assert_eq!(
            union.origin,
            CGPoint {
                x: -1920.0,
                y: -100.0
            }
        );
        assert_eq!(
            normalized_point_in_bounds(union, 0.0, 0.0),
            CGPoint {
                x: -1920.0,
                y: -100.0
            }
        );
        assert_eq!(
            moved_point_in_bounds(
                CGPoint {
                    x: -1000.0,
                    y: 400.0
                },
                union,
                10.0,
                0.0,
                false,
            ),
            CGPoint {
                x: -990.0,
                y: 400.0
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
    fn held_mouse_buttons_turn_pointer_motion_into_native_drag_events() {
        assert_eq!(
            motion_event_type(None),
            (CgEventType::MouseMoved, CgMouseButton::Left)
        );
        assert_eq!(
            motion_event_type(Some(CgMouseButton::Left)),
            (CgEventType::LeftMouseDragged, CgMouseButton::Left)
        );
        assert_eq!(
            motion_event_type(Some(CgMouseButton::Right)),
            (CgEventType::RightMouseDragged, CgMouseButton::Right)
        );

        let mut held = Vec::new();
        update_pressed_buttons(&mut held, CgEventType::LeftMouseDown, CgMouseButton::Left);
        assert_eq!(held, [CgMouseButton::Left]);
        update_pressed_buttons(&mut held, CgEventType::LeftMouseUp, CgMouseButton::Left);
        assert!(held.is_empty());
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

    #[test]
    fn every_captured_keyboard_hid_maps_back_to_the_same_macos_keycode() {
        let mut mapped = 0_usize;

        for cg_keycode in u16::MIN..=u16::MAX {
            if let Some(hid_keycode) = crate::capture::cg_to_hid_keycode(cg_keycode) {
                assert_eq!(
                    hid_to_cg_keycode(hid_keycode),
                    Some(cg_keycode),
                    "regular key HID {hid_keycode:#04x}"
                );
                mapped += 1;
            }
            if let Some((hid_keycode, _)) = crate::capture::cg_modifier_to_hid_and_flag(cg_keycode)
            {
                assert_eq!(
                    hid_to_cg_keycode(hid_keycode),
                    Some(cg_keycode),
                    "modifier HID {hid_keycode:#04x}"
                );
                mapped += 1;
            }
        }

        assert!(mapped > 70, "capture table unexpectedly shrank");
    }
}
