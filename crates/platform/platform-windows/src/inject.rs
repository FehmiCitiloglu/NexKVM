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
        0x29 => Some(0x01), // Escape
        0x2C => Some(0x39), // Space
        _ => None,
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
