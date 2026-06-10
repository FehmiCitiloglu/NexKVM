//! Windows injection mapping: [`InjectionCommand`] → `SendInput` plan.
//!
//! The real backend builds `INPUT` structures and calls `SendInput` (Win32). It
//! is gated by User Interface Privilege Isolation (UIPI): injection into windows
//! owned by higher-integrity processes is silently dropped, which nexkvm surfaces
//! via capabilities rather than failing blindly. That FFI lands in a later phase;
//! this module is the pure, testable translation it consumes — turning a neutral
//! command into the `INPUT` kind + `dwFlags` + payload, with no `unsafe` and no
//! Win32 dependency.
//!
//! # Keycode caveat
//! [`InjectionCommand::Key`] carries a USB HID usage id. Windows keyboard input
//! uses virtual-key codes or hardware scancodes. The FFI layer maps HID →
//! scancode and sets `KEYEVENTF_SCANCODE`; this module passes the keycode
//! through and records the up/down flag so the table applies at one seam.

use nexkvm_input::{InjectionCommand, MouseButton};

/// `MOUSEEVENTF_*` flags.
pub mod mouse_flag {
    /// `MOUSEEVENTF_MOVE`.
    pub const MOVE: u32 = 0x0001;
    /// `MOUSEEVENTF_ABSOLUTE`.
    pub const ABSOLUTE: u32 = 0x8000;
    /// `MOUSEEVENTF_LEFTDOWN`.
    pub const LEFTDOWN: u32 = 0x0002;
    /// `MOUSEEVENTF_LEFTUP`.
    pub const LEFTUP: u32 = 0x0004;
    /// `MOUSEEVENTF_RIGHTDOWN`.
    pub const RIGHTDOWN: u32 = 0x0008;
    /// `MOUSEEVENTF_RIGHTUP`.
    pub const RIGHTUP: u32 = 0x0010;
    /// `MOUSEEVENTF_MIDDLEDOWN`.
    pub const MIDDLEDOWN: u32 = 0x0020;
    /// `MOUSEEVENTF_MIDDLEUP`.
    pub const MIDDLEUP: u32 = 0x0040;
    /// `MOUSEEVENTF_WHEEL`.
    pub const WHEEL: u32 = 0x0800;
    /// `MOUSEEVENTF_HWHEEL`.
    pub const HWHEEL: u32 = 0x1000;
}

/// `KEYEVENTF_*` flags.
pub mod key_flag {
    /// `KEYEVENTF_KEYUP`.
    pub const KEYUP: u32 = 0x0002;
    /// `KEYEVENTF_SCANCODE`.
    pub const SCANCODE: u32 = 0x0008;
}

/// One scroll "click" in `WHEEL_DELTA` units, per the Win32 wheel API.
pub const WHEEL_DELTA: i32 = 120;

/// Normalized-absolute coordinate scale (`0..=65535`) for `MOUSEEVENTF_ABSOLUTE`.
pub const ABS_SCALE: f64 = 65_535.0;

/// A concrete `SendInput` instruction derived from an [`InjectionCommand`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SendInputPlan {
    /// `INPUT_MOUSE` absolute move; `x`/`y` are in `0..=65535` virtual-desktop
    /// coordinates. Flags include `MOVE | ABSOLUTE`.
    MouseMoveAbsolute {
        /// Horizontal coordinate (`0..=65535`).
        x: i32,
        /// Vertical coordinate (`0..=65535`).
        y: i32,
        /// `dwFlags`.
        flags: u32,
    },
    /// `INPUT_MOUSE` relative move carrying a screen-fraction delta, scaled to
    /// pixels by the FFI layer using the target geometry. Flags = `MOVE`.
    MouseMoveRelative {
        /// Horizontal fraction delta.
        dx: f64,
        /// Vertical fraction delta.
        dy: f64,
        /// `dwFlags`.
        flags: u32,
    },
    /// `INPUT_MOUSE` raw device-count relative move. Flags = `MOVE`.
    MouseMoveRaw {
        /// Horizontal delta in device units.
        dx: i32,
        /// Vertical delta in device units.
        dy: i32,
        /// `dwFlags`.
        flags: u32,
    },
    /// `INPUT_MOUSE` button transition (flag encodes button + up/down).
    MouseButton {
        /// `dwFlags`.
        flags: u32,
    },
    /// `INPUT_MOUSE` wheel/hwheel; `amount` is in `WHEEL_DELTA` units.
    MouseScroll {
        /// `dwFlags` (`WHEEL` or `HWHEEL`).
        flags: u32,
        /// `mouseData` scroll amount.
        amount: i32,
    },
    /// `INPUT_KEYBOARD` key transition. `keycode` is still a USB HID usage id;
    /// the FFI maps it to a scancode and ORs in `KEYEVENTF_SCANCODE`.
    Key {
        /// HID usage id to translate to a scancode.
        keycode: u32,
        /// `dwFlags` (`0` for down, `KEYUP` for up).
        flags: u32,
    },
}

fn button_flag(button: MouseButton, pressed: bool) -> u32 {
    match (button, pressed) {
        (MouseButton::Left, true) => mouse_flag::LEFTDOWN,
        (MouseButton::Left, false) => mouse_flag::LEFTUP,
        (MouseButton::Right, true) => mouse_flag::RIGHTDOWN,
        (MouseButton::Right, false) => mouse_flag::RIGHTUP,
        (MouseButton::Middle, true) => mouse_flag::MIDDLEDOWN,
        (MouseButton::Middle, false) => mouse_flag::MIDDLEUP,
    }
}

/// Translate a neutral [`InjectionCommand`] into a Windows [`SendInputPlan`].
#[must_use]
pub fn plan(command: InjectionCommand) -> SendInputPlan {
    match command {
        InjectionCommand::MoveAbsolute { x, y } => {
            let to_abs = |v: f64| (v.clamp(0.0, 1.0) * ABS_SCALE).round() as i32;
            SendInputPlan::MouseMoveAbsolute {
                x: to_abs(x),
                y: to_abs(y),
                flags: mouse_flag::MOVE | mouse_flag::ABSOLUTE,
            }
        }
        InjectionCommand::MoveRelative { dx, dy } => SendInputPlan::MouseMoveRelative {
            dx,
            dy,
            flags: mouse_flag::MOVE,
        },
        InjectionCommand::MoveRaw { dx, dy } => SendInputPlan::MouseMoveRaw {
            dx,
            dy,
            flags: mouse_flag::MOVE,
        },
        InjectionCommand::Button { button, pressed } => SendInputPlan::MouseButton {
            flags: button_flag(button, pressed),
        },
        InjectionCommand::Scroll { dx, dy } => {
            // Prefer the vertical axis; horizontal scroll uses HWHEEL.
            if dy != 0.0 {
                SendInputPlan::MouseScroll {
                    flags: mouse_flag::WHEEL,
                    amount: (dy * WHEEL_DELTA as f64).round() as i32,
                }
            } else {
                SendInputPlan::MouseScroll {
                    flags: mouse_flag::HWHEEL,
                    amount: (dx * WHEEL_DELTA as f64).round() as i32,
                }
            }
        }
        InjectionCommand::Key { keycode, pressed } => SendInputPlan::Key {
            keycode,
            flags: if pressed {
                key_flag::SCANCODE
            } else {
                key_flag::SCANCODE | key_flag::KEYUP
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_move_scales_and_sets_absolute_flag() {
        assert_eq!(
            plan(InjectionCommand::MoveAbsolute { x: 1.0, y: 0.0 }),
            SendInputPlan::MouseMoveAbsolute {
                x: 65_535,
                y: 0,
                flags: mouse_flag::MOVE | mouse_flag::ABSOLUTE,
            }
        );
    }

    #[test]
    fn right_button_press_release_map_to_flags() {
        assert_eq!(
            plan(InjectionCommand::Button {
                button: MouseButton::Right,
                pressed: true
            }),
            SendInputPlan::MouseButton {
                flags: mouse_flag::RIGHTDOWN
            }
        );
        assert_eq!(
            plan(InjectionCommand::Button {
                button: MouseButton::Right,
                pressed: false
            }),
            SendInputPlan::MouseButton {
                flags: mouse_flag::RIGHTUP
            }
        );
    }

    #[test]
    fn vertical_scroll_uses_wheel_delta() {
        assert_eq!(
            plan(InjectionCommand::Scroll { dx: 0.0, dy: 1.0 }),
            SendInputPlan::MouseScroll {
                flags: mouse_flag::WHEEL,
                amount: WHEEL_DELTA,
            }
        );
    }

    #[test]
    fn key_release_sets_scancode_and_keyup() {
        assert_eq!(
            plan(InjectionCommand::Key {
                keycode: 0x04,
                pressed: false
            }),
            SendInputPlan::Key {
                keycode: 0x04,
                flags: key_flag::SCANCODE | key_flag::KEYUP,
            }
        );
    }
}
