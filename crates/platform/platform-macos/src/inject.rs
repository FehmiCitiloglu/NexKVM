//! macOS injection mapping: [`InjectionCommand`] → Quartz `CGEvent` plan.
//!
//! The real backend posts events with `CGEventPost`/`CGEventCreateMouseEvent`/
//! `CGEventCreateKeyboardEvent` once Accessibility permission is granted. That
//! FFI lands in a later phase; this module is the pure, testable translation it
//! consumes — turning a neutral command into the exact `CGEventType` + payload
//! to synthesize, with no `unsafe` and no Quartz dependency.
//!
//! # Keycode caveat
//! [`InjectionCommand::Key`] carries an OS-neutral USB HID usage id. macOS
//! `CGEvent` keyboard events use `CGKeyCode` (ANSI virtual keycodes), which are
//! a different namespace. The FFI layer must map HID → `CGKeyCode` via a lookup
//! table (plus layout via `UCKeyTranslate`); this module passes the keycode
//! through unchanged and records the intended event type.

use nexkvm_input::{InjectionCommand, MouseButton};

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
