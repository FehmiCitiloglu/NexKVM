//! Linux injection mapping: [`InjectionCommand`] → evdev/`uinput` events.
//!
//! On X11 the real backend uses `XTEST`; on native Wayland it must go through
//! the `RemoteDesktop` portal (no raw global injection is allowed). The most
//! portable low-level path is a virtual `uinput` device that emits evdev events.
//! This module is the pure, testable translation that path consumes: it turns a
//! neutral command into the `(type, code, value)` evdev tuples to write, with no
//! `unsafe` and no kernel dependency.
//!
//! A single command can expand to several evdev events (e.g. a relative move is
//! `REL_X` + `REL_Y`). The caller is responsible for terminating each batch with
//! an `EV_SYN`/`SYN_REPORT` after writing the returned events.
//!
//! # Keycode caveat
//! [`InjectionCommand::Key`] carries a USB HID usage id; the Linux input layer
//! uses `KEY_*` codes (`linux/input-event-codes.h`), a different namespace. The
//! FFI layer maps HID → evdev `KEY_*` before writing; this module passes the
//! keycode through as the evdev `code` so the table can be applied at one seam.

use coklu_input::{InjectionCommand, MouseButton};

/// evdev event types (`EV_*`).
pub mod ev_type {
    /// `EV_SYN`.
    pub const SYN: u16 = 0x00;
    /// `EV_KEY`.
    pub const KEY: u16 = 0x01;
    /// `EV_REL`.
    pub const REL: u16 = 0x02;
    /// `EV_ABS`.
    pub const ABS: u16 = 0x03;
}

/// evdev relative axis codes (`REL_*`).
pub mod rel_code {
    /// `REL_X`.
    pub const X: u16 = 0x00;
    /// `REL_Y`.
    pub const Y: u16 = 0x01;
    /// `REL_HWHEEL`.
    pub const HWHEEL: u16 = 0x06;
    /// `REL_WHEEL`.
    pub const WHEEL: u16 = 0x08;
}

/// evdev absolute axis codes (`ABS_*`).
pub mod abs_code {
    /// `ABS_X`.
    pub const X: u16 = 0x00;
    /// `ABS_Y`.
    pub const Y: u16 = 0x01;
}

/// evdev button codes (`BTN_*`).
pub mod btn_code {
    /// `BTN_LEFT`.
    pub const LEFT: u16 = 0x110;
    /// `BTN_RIGHT`.
    pub const RIGHT: u16 = 0x111;
    /// `BTN_MIDDLE`.
    pub const MIDDLE: u16 = 0x112;
}

/// Full-scale value used for normalized absolute axes on the virtual device.
pub const ABS_MAX: i32 = 65535;

/// A single evdev event to write to the `uinput` device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UinputEvent {
    /// `EV_*` event type.
    pub type_: u16,
    /// Axis/button/key code within `type_`.
    pub code: u16,
    /// Event value (axis delta/position, or `1`/`0` for press/release).
    pub value: i32,
}

impl UinputEvent {
    const fn new(type_: u16, code: u16, value: i32) -> Self {
        Self { type_, code, value }
    }
}

fn button_code(button: MouseButton) -> u16 {
    match button {
        MouseButton::Left => btn_code::LEFT,
        MouseButton::Right => btn_code::RIGHT,
        MouseButton::Middle => btn_code::MIDDLE,
    }
}

/// Translate a neutral [`InjectionCommand`] into the evdev events to write.
///
/// The returned events must be followed by an `EV_SYN`/`SYN_REPORT` by the
/// caller to commit them to the input subsystem.
#[must_use]
pub fn plan(command: InjectionCommand) -> Vec<UinputEvent> {
    match command {
        InjectionCommand::MoveAbsolute { x, y } => {
            let to_abs = |v: f64| (v.clamp(0.0, 1.0) * ABS_MAX as f64).round() as i32;
            vec![
                UinputEvent::new(ev_type::ABS, abs_code::X, to_abs(x)),
                UinputEvent::new(ev_type::ABS, abs_code::Y, to_abs(y)),
            ]
        }
        // Relative fractions are scaled to device units by the FFI layer using
        // the target geometry; here we forward rounded integer deltas.
        InjectionCommand::MoveRelative { dx, dy } => vec![
            UinputEvent::new(ev_type::REL, rel_code::X, dx.round() as i32),
            UinputEvent::new(ev_type::REL, rel_code::Y, dy.round() as i32),
        ],
        InjectionCommand::MoveRaw { dx, dy } => vec![
            UinputEvent::new(ev_type::REL, rel_code::X, dx),
            UinputEvent::new(ev_type::REL, rel_code::Y, dy),
        ],
        InjectionCommand::Button { button, pressed } => vec![UinputEvent::new(
            ev_type::KEY,
            button_code(button),
            i32::from(pressed),
        )],
        InjectionCommand::Scroll { dx, dy } => {
            let mut events = Vec::new();
            if dy != 0.0 {
                events.push(UinputEvent::new(
                    ev_type::REL,
                    rel_code::WHEEL,
                    dy.round() as i32,
                ));
            }
            if dx != 0.0 {
                events.push(UinputEvent::new(
                    ev_type::REL,
                    rel_code::HWHEEL,
                    dx.round() as i32,
                ));
            }
            events
        }
        InjectionCommand::Key { keycode, pressed } => vec![UinputEvent::new(
            ev_type::KEY,
            keycode as u16,
            i32::from(pressed),
        )],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_move_scales_to_full_range() {
        assert_eq!(
            plan(InjectionCommand::MoveAbsolute { x: 1.0, y: 0.0 }),
            vec![
                UinputEvent::new(ev_type::ABS, abs_code::X, ABS_MAX),
                UinputEvent::new(ev_type::ABS, abs_code::Y, 0),
            ]
        );
    }

    #[test]
    fn raw_move_emits_two_rel_axes() {
        assert_eq!(
            plan(InjectionCommand::MoveRaw { dx: 3, dy: -2 }),
            vec![
                UinputEvent::new(ev_type::REL, rel_code::X, 3),
                UinputEvent::new(ev_type::REL, rel_code::Y, -2),
            ]
        );
    }

    #[test]
    fn left_button_press_release_uses_btn_left() {
        assert_eq!(
            plan(InjectionCommand::Button {
                button: MouseButton::Left,
                pressed: true
            }),
            vec![UinputEvent::new(ev_type::KEY, btn_code::LEFT, 1)]
        );
        assert_eq!(
            plan(InjectionCommand::Button {
                button: MouseButton::Left,
                pressed: false
            }),
            vec![UinputEvent::new(ev_type::KEY, btn_code::LEFT, 0)]
        );
    }

    #[test]
    fn vertical_scroll_only_emits_wheel() {
        assert_eq!(
            plan(InjectionCommand::Scroll { dx: 0.0, dy: 1.0 }),
            vec![UinputEvent::new(ev_type::REL, rel_code::WHEEL, 1)]
        );
    }

    #[test]
    fn key_event_forwards_code_and_value() {
        assert_eq!(
            plan(InjectionCommand::Key {
                keycode: 0x06,
                pressed: true
            }),
            vec![UinputEvent::new(ev_type::KEY, 0x06, 1)]
        );
    }
}
