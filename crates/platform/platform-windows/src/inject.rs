//! Windows injection mapping: [`InjectionCommand`] → `SendInput` plan.
//!
//! The backend builds `INPUT` structures and calls `SendInput` (Win32). It is
//! gated by User Interface Privilege Isolation (UIPI): injection into windows
//! owned by higher-integrity processes can be silently dropped by Windows, so
//! nexkvm keeps the translation testable and returns backend errors for cases it
//! cannot inject.
//!
//! # Keycode caveat
//! [`InjectionCommand::Key`] carries a USB HID usage id. Windows keyboard input
//! uses virtual-key codes or hardware scancodes. This module maps HID →
//! scancode and sets `KEYEVENTF_SCANCODE`.

#![allow(unsafe_code)]

use async_trait::async_trait;
use std::fmt;
use std::mem::size_of;
use std::sync::Arc;

use nexkvm_input::{InjectionCommand, InputError, InputEvent, InputInjector, MouseButton};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, MOUSEINPUT, SendInput,
};

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
    /// `KEYEVENTF_EXTENDEDKEY`.
    pub const EXTENDED: u32 = 0x0001;
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

trait InputSender: Send + Sync {
    fn send(&self, plan: SendInputPlan) -> Result<(), InputError>;
}

#[derive(Debug, Clone, Copy, Default)]
struct NativeInputSender;

impl InputSender for NativeInputSender {
    fn send(&self, plan: SendInputPlan) -> Result<(), InputError> {
        send_input_plan(plan)
    }
}

/// Windows input injector backed by Win32 `SendInput`.
#[derive(Clone)]
pub struct WindowsInputInjector {
    sender: Arc<dyn InputSender>,
}

impl WindowsInputInjector {
    /// Create an injector backed by native Win32 `SendInput`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            sender: Arc::new(NativeInputSender),
        }
    }

    #[cfg(test)]
    fn with_sender(sender: Arc<dyn InputSender>) -> Self {
        Self { sender }
    }
}

impl Default for WindowsInputInjector {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for WindowsInputInjector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WindowsInputInjector")
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl InputInjector for WindowsInputInjector {
    async fn inject(&self, event: InputEvent) -> Result<(), InputError> {
        let command = event.to_injection_command();
        let plan = plan(command);
        validate_supported(plan)?;
        self.sender.send(plan)
    }
}

fn validate_supported(plan: SendInputPlan) -> Result<(), InputError> {
    match plan {
        SendInputPlan::Key { keycode, .. } if hid_to_scancode(keycode).is_none() => Err(
            InputError::Backend(format!("unsupported Windows HID keycode: {keycode}")),
        ),
        _ => Ok(()),
    }
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

fn send_input_plan(plan: SendInputPlan) -> Result<(), InputError> {
    let input = input_from_plan(plan)?;
    // SAFETY: `input` is a valid Win32 INPUT structure initialized by
    // `input_from_plan`; the slice length is one and cbSize matches INPUT.
    let sent = unsafe { SendInput(1, &input, size_of::<INPUT>() as i32) };
    if sent == 1 {
        Ok(())
    } else {
        Err(InputError::Backend(
            "SendInput did not inject the requested event".into(),
        ))
    }
}

fn input_from_plan(plan: SendInputPlan) -> Result<INPUT, InputError> {
    match plan {
        SendInputPlan::MouseMoveAbsolute { x, y, flags } => Ok(mouse_input(x, y, 0, flags)),
        SendInputPlan::MouseMoveRelative { .. } => Err(InputError::Backend(
            "Windows relative screen-fraction movement is not wired yet".into(),
        )),
        SendInputPlan::MouseMoveRaw { dx, dy, flags } => Ok(mouse_input(dx, dy, 0, flags)),
        SendInputPlan::MouseButton { flags } => Ok(mouse_input(0, 0, 0, flags)),
        SendInputPlan::MouseScroll { flags, amount } => Ok(mouse_input(0, 0, amount, flags)),
        SendInputPlan::Key { keycode, flags } => {
            let Some(scan) = hid_to_scancode(keycode) else {
                return Err(InputError::Backend(format!(
                    "unsupported Windows HID keycode: {keycode}"
                )));
            };
            let flags = if hid_uses_extended_scancode(keycode) {
                flags | key_flag::EXTENDED
            } else {
                flags
            };
            Ok(keyboard_input(scan, flags))
        }
    }
}

fn mouse_input(dx: i32, dy: i32, mouse_data: i32, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: mouse_data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn keyboard_input(scan: u16, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: 0,
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn hid_to_scancode(keycode: u32) -> Option<u16> {
    match keycode {
        0x04 => Some(0x1e), // A
        0x05 => Some(0x30), // B
        0x06 => Some(0x2e), // C
        0x07 => Some(0x20), // D
        0x08 => Some(0x12), // E
        0x09 => Some(0x21), // F
        0x0A => Some(0x22), // G
        0x0B => Some(0x23), // H
        0x0C => Some(0x17), // I
        0x0D => Some(0x24), // J
        0x0E => Some(0x25), // K
        0x0F => Some(0x26), // L
        0x10 => Some(0x32), // M
        0x11 => Some(0x31), // N
        0x12 => Some(0x18), // O
        0x13 => Some(0x19), // P
        0x14 => Some(0x10), // Q
        0x15 => Some(0x13), // R
        0x16 => Some(0x1f), // S
        0x17 => Some(0x14), // T
        0x18 => Some(0x16), // U
        0x19 => Some(0x2f), // V
        0x1A => Some(0x11), // W
        0x1B => Some(0x2d), // X
        0x1C => Some(0x15), // Y
        0x1D => Some(0x2c), // Z
        0x1E => Some(0x02), // 1
        0x1F => Some(0x03), // 2
        0x20 => Some(0x04), // 3
        0x21 => Some(0x05), // 4
        0x22 => Some(0x06), // 5
        0x23 => Some(0x07), // 6
        0x24 => Some(0x08), // 7
        0x25 => Some(0x09), // 8
        0x26 => Some(0x0A), // 9
        0x27 => Some(0x0B), // 0
        0x28 => Some(0x1C), // Return
        0x29 => Some(0x01), // Escape
        0x2A => Some(0x0E), // Backspace
        0x2B => Some(0x0F), // Tab
        0x2C => Some(0x39), // Space
        0x2D => Some(0x0C), // Minus
        0x2E => Some(0x0D), // Equal
        0x2F => Some(0x1A), // Left bracket
        0x30 => Some(0x1B), // Right bracket
        0x31 => Some(0x2B), // Backslash
        0x33 => Some(0x27), // Semicolon
        0x34 => Some(0x28), // Apostrophe
        0x35 => Some(0x29), // Grave
        0x36 => Some(0x33), // Comma
        0x37 => Some(0x34), // Period
        0x38 => Some(0x35), // Slash
        0x39 => Some(0x3A), // Caps lock
        0x3A => Some(0x3B), // F1
        0x3B => Some(0x3C), // F2
        0x3C => Some(0x3D), // F3
        0x3D => Some(0x3E), // F4
        0x3E => Some(0x3F), // F5
        0x3F => Some(0x40), // F6
        0x40 => Some(0x41), // F7
        0x41 => Some(0x42), // F8
        0x42 => Some(0x43), // F9
        0x43 => Some(0x44), // F10
        0x44 => Some(0x57), // F11
        0x45 => Some(0x58), // F12
        0x4A => Some(0x47), // Home
        0x4B => Some(0x49), // Page up
        0x4C => Some(0x53), // Delete
        0x4D => Some(0x4F), // End
        0x4E => Some(0x51), // Page down
        0x4F => Some(0x4D), // Right arrow
        0x50 => Some(0x4B), // Left arrow
        0x51 => Some(0x50), // Down arrow
        0x52 => Some(0x48), // Up arrow
        0x54 => Some(0x35), // Keypad divide
        0x55 => Some(0x37), // Keypad multiply
        0x56 => Some(0x4A), // Keypad minus
        0x57 => Some(0x4E), // Keypad plus
        0x58 => Some(0x1C), // Keypad enter
        0x59 => Some(0x4F), // Keypad 1
        0x5A => Some(0x50), // Keypad 2
        0x5B => Some(0x51), // Keypad 3
        0x5C => Some(0x4B), // Keypad 4
        0x5D => Some(0x4C), // Keypad 5
        0x5E => Some(0x4D), // Keypad 6
        0x5F => Some(0x47), // Keypad 7
        0x60 => Some(0x48), // Keypad 8
        0x61 => Some(0x49), // Keypad 9
        0x62 => Some(0x52), // Keypad 0
        0x63 => Some(0x53), // Keypad decimal
        0xE0 => Some(0x1D), // Left control
        0xE1 => Some(0x2A), // Left shift
        0xE2 => Some(0x38), // Left alt
        0xE3 => Some(0x5B), // Left GUI
        0xE4 => Some(0x1D), // Right control
        0xE5 => Some(0x36), // Right shift
        0xE6 => Some(0x38), // Right alt
        0xE7 => Some(0x5C), // Right GUI
        _ => None,
    }
}

fn hid_uses_extended_scancode(keycode: u32) -> bool {
    matches!(keycode, 0x4A..=0x54 | 0x58 | 0xE3 | 0xE4 | 0xE6 | 0xE7)
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

    #[test]
    fn common_hid_keys_map_to_windows_scancodes() {
        assert_eq!(hid_to_scancode(0x1E), Some(0x02)); // 1
        assert_eq!(hid_to_scancode(0x28), Some(0x1C)); // Return
        assert_eq!(hid_to_scancode(0x2A), Some(0x0E)); // Backspace
        assert_eq!(hid_to_scancode(0xE1), Some(0x2A)); // Left shift
    }

    #[test]
    fn right_control_and_navigation_keys_use_extended_scancodes() {
        for (keycode, expected_scan) in [(0xE4, 0x1D), (0x4F, 0x4D)] {
            let input = input_from_plan(SendInputPlan::Key {
                keycode,
                flags: key_flag::SCANCODE,
            })
            .unwrap();

            // SAFETY: `input_from_plan` initialized the keyboard union member.
            let keyboard = unsafe { input.Anonymous.ki };
            assert_eq!(keyboard.wScan, expected_scan);
            assert_eq!(keyboard.dwFlags, key_flag::SCANCODE | key_flag::EXTENDED);
        }
    }

    #[tokio::test]
    async fn injector_sends_supported_events() {
        let sender = std::sync::Arc::new(RecordingSender::default());
        let injector = WindowsInputInjector::with_sender(sender.clone());

        injector.inject(InputEvent::KeyPress(0x04)).await.unwrap();
        injector
            .inject(InputEvent::ButtonPress(MouseButton::Left))
            .await
            .unwrap();

        assert_eq!(
            sender.plans(),
            vec![
                SendInputPlan::Key {
                    keycode: 0x04,
                    flags: key_flag::SCANCODE
                },
                SendInputPlan::MouseButton {
                    flags: mouse_flag::LEFTDOWN
                }
            ]
        );
    }

    #[tokio::test]
    async fn injector_rejects_unsupported_keycode() {
        let sender = std::sync::Arc::new(RecordingSender::default());
        let injector = WindowsInputInjector::with_sender(sender);

        let error = injector
            .inject(InputEvent::KeyPress(0xff))
            .await
            .unwrap_err();

        assert!(matches!(error, InputError::Backend(_)));
    }

    #[derive(Debug, Default)]
    struct RecordingSender {
        plans: std::sync::Mutex<Vec<SendInputPlan>>,
    }

    impl RecordingSender {
        fn plans(&self) -> Vec<SendInputPlan> {
            self.plans.lock().unwrap().clone()
        }
    }

    impl InputSender for RecordingSender {
        fn send(&self, plan: SendInputPlan) -> Result<(), InputError> {
            self.plans.lock().unwrap().push(plan);
            Ok(())
        }
    }
}
