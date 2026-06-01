//! Mobile companion input translation: phone touchpad and gyro-mouse modes.
//!
//! A phone/tablet acting as a companion does not emit OS pointer events — it
//! emits **touches** and **orientation**. This module is the sans-IO bridge that
//! turns those into the platform-neutral [`InputEvent`]s the rest of the
//! pipeline already understands (relative motion, scroll, clicks), so the future
//! mobile app reuses the exact same share/injection path as a desktop peer.
//!
//! Two modes are modeled:
//! - **Touchpad** ([`TouchpadTranslator`]): one finger drags the cursor
//!   (relative motion), two fingers scroll, and a quick low-travel tap is a
//!   left click.
//! - **Gyro mouse** ([`GyroMouse`]): device yaw/pitch deltas move the cursor,
//!   with a deadzone to reject hand tremor.
//!
//! Both are pure: the caller feeds samples (with monotonic timestamps for tap
//! detection) and forwards the returned events. No clock, no OS calls — the
//! mobile platform backend supplies raw touch/sensor data.

use serde::{Deserialize, Serialize};

use crate::{InputEvent, MouseButton};

/// Which companion input mode a mobile device is driving.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MobileInputMode {
    /// Touch surface drives a relative trackpad.
    Touchpad,
    /// Device orientation drives the cursor.
    Gyro,
}

/// A normalized touch coordinate in `[0.0, 1.0]` per axis.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TouchPoint {
    /// Horizontal position.
    pub x: f64,
    /// Vertical position.
    pub y: f64,
}

impl TouchPoint {
    /// Construct a touch point.
    #[must_use]
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

/// Lifecycle phase of a touch gesture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TouchPhase {
    /// First contact.
    Begin,
    /// Continued contact with movement.
    Move,
    /// Lift-off.
    End,
}

/// One touch sample from the mobile surface.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TouchSample {
    /// Gesture phase.
    pub phase: TouchPhase,
    /// Number of fingers in contact (1 = pointer, 2 = scroll).
    pub fingers: u8,
    /// Normalized contact point.
    pub point: TouchPoint,
    /// Monotonic timestamp in microseconds.
    pub time_micros: u64,
}

/// Touchpad sensitivity, scroll, and tap-detection tuning.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TouchpadConfig {
    /// Multiplier applied to one-finger motion before emitting relative moves.
    pub pointer_sensitivity: f64,
    /// Multiplier applied to two-finger motion before emitting scroll.
    pub scroll_sensitivity: f64,
    /// Max gesture duration (micros) still counted as a tap-click.
    pub tap_max_micros: u64,
    /// Max normalized travel still counted as a tap (rejects drags).
    pub tap_max_travel: f64,
    /// Invert two-finger scroll direction (natural scrolling).
    pub natural_scroll: bool,
}

impl TouchpadConfig {
    /// Balanced defaults tuned for a phone-sized surface.
    #[must_use]
    pub const fn phone_default() -> Self {
        Self {
            pointer_sensitivity: 1.5,
            scroll_sensitivity: 8.0,
            tap_max_micros: 200_000,
            tap_max_travel: 0.03,
            natural_scroll: true,
        }
    }
}

impl Default for TouchpadConfig {
    fn default() -> Self {
        Self::phone_default()
    }
}

/// Stateful translator from touch samples to [`InputEvent`]s.
#[derive(Debug, Clone)]
pub struct TouchpadTranslator {
    config: TouchpadConfig,
    last_point: Option<TouchPoint>,
    gesture_start: Option<(TouchPoint, u64)>,
    travel: f64,
    max_fingers: u8,
}

impl TouchpadTranslator {
    /// Create a translator.
    #[must_use]
    pub fn new(config: TouchpadConfig) -> Self {
        Self {
            config,
            last_point: None,
            gesture_start: None,
            travel: 0.0,
            max_fingers: 0,
        }
    }

    /// Translate one touch sample into zero or more input events.
    ///
    /// One finger emits [`InputEvent::RelativeMove`]; two fingers emit
    /// [`InputEvent::Scroll`]; a quick low-travel single-finger tap emits a
    /// left [`InputEvent::ButtonPress`]/[`InputEvent::ButtonRelease`] pair.
    pub fn translate(&mut self, sample: TouchSample) -> Vec<InputEvent> {
        match sample.phase {
            TouchPhase::Begin => {
                self.last_point = Some(sample.point);
                self.gesture_start = Some((sample.point, sample.time_micros));
                self.travel = 0.0;
                self.max_fingers = sample.fingers.max(1);
                Vec::new()
            }
            TouchPhase::Move => self.translate_move(sample),
            TouchPhase::End => self.translate_end(sample),
        }
    }

    fn translate_move(&mut self, sample: TouchSample) -> Vec<InputEvent> {
        self.max_fingers = self.max_fingers.max(sample.fingers);
        let Some(previous) = self.last_point else {
            self.last_point = Some(sample.point);
            return Vec::new();
        };
        let dx = sample.point.x - previous.x;
        let dy = sample.point.y - previous.y;
        self.last_point = Some(sample.point);
        self.travel += dx.hypot(dy);

        if sample.fingers >= 2 {
            let direction = if self.config.natural_scroll {
                1.0
            } else {
                -1.0
            };
            vec![InputEvent::Scroll {
                dx: dx * self.config.scroll_sensitivity * direction,
                dy: dy * self.config.scroll_sensitivity * direction,
            }]
        } else {
            vec![InputEvent::RelativeMove {
                dx: dx * self.config.pointer_sensitivity,
                dy: dy * self.config.pointer_sensitivity,
            }]
        }
    }

    fn translate_end(&mut self, sample: TouchSample) -> Vec<InputEvent> {
        let start = self.gesture_start.take();
        let max_fingers = self.max_fingers;
        let travel = self.travel;
        self.last_point = None;
        self.travel = 0.0;
        self.max_fingers = 0;

        let Some((_, started_at)) = start else {
            return Vec::new();
        };
        let duration = sample.time_micros.saturating_sub(started_at);
        let is_tap = max_fingers == 1
            && duration <= self.config.tap_max_micros
            && travel <= self.config.tap_max_travel;
        if is_tap {
            vec![
                InputEvent::ButtonPress(MouseButton::Left),
                InputEvent::ButtonRelease(MouseButton::Left),
            ]
        } else {
            Vec::new()
        }
    }
}

/// Device orientation in radians (right-handed; yaw around vertical axis).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Orientation {
    /// Rotation around the vertical axis (left/right aim).
    pub yaw: f64,
    /// Rotation around the horizontal axis (up/down aim).
    pub pitch: f64,
}

impl Orientation {
    /// Construct an orientation.
    #[must_use]
    pub const fn new(yaw: f64, pitch: f64) -> Self {
        Self { yaw, pitch }
    }
}

/// Gyro-mouse sensitivity and jitter rejection.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GyroConfig {
    /// Scale from radians of rotation to fraction-of-screen motion.
    pub sensitivity: f64,
    /// Per-axis deadzone (radians) below which motion is ignored.
    pub deadzone: f64,
    /// Invert vertical motion.
    pub invert_pitch: bool,
}

impl GyroConfig {
    /// Reasonable defaults for handheld aiming.
    #[must_use]
    pub const fn phone_default() -> Self {
        Self {
            sensitivity: 4.0,
            deadzone: 0.002,
            invert_pitch: false,
        }
    }
}

impl Default for GyroConfig {
    fn default() -> Self {
        Self::phone_default()
    }
}

/// Stateful translator from orientation samples to relative pointer motion.
#[derive(Debug, Clone)]
pub struct GyroMouse {
    config: GyroConfig,
    last: Option<Orientation>,
}

impl GyroMouse {
    /// Create a gyro-mouse translator.
    #[must_use]
    pub fn new(config: GyroConfig) -> Self {
        Self { config, last: None }
    }

    /// Re-seed the reference orientation without emitting motion (recenter).
    pub fn recenter(&mut self, orientation: Orientation) {
        self.last = Some(orientation);
    }

    /// Feed an orientation sample, returning relative motion if it clears the
    /// deadzone. Yaw maps to horizontal, pitch to vertical.
    pub fn update(&mut self, orientation: Orientation) -> Option<InputEvent> {
        let previous = self.last.replace(orientation)?;
        let yaw_delta = deadzone(orientation.yaw - previous.yaw, self.config.deadzone);
        let pitch_delta = deadzone(orientation.pitch - previous.pitch, self.config.deadzone);
        if yaw_delta == 0.0 && pitch_delta == 0.0 {
            return None;
        }
        let invert = if self.config.invert_pitch { -1.0 } else { 1.0 };
        Some(InputEvent::RelativeMove {
            dx: yaw_delta * self.config.sensitivity,
            dy: pitch_delta * self.config.sensitivity * invert,
        })
    }
}

fn deadzone(value: f64, threshold: f64) -> f64 {
    if value.abs() <= threshold { 0.0 } else { value }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touchpad() -> TouchpadTranslator {
        TouchpadTranslator::new(TouchpadConfig::phone_default())
    }

    #[test]
    fn one_finger_drag_emits_relative_move() {
        let mut tp = touchpad();
        assert!(
            tp.translate(TouchSample {
                phase: TouchPhase::Begin,
                fingers: 1,
                point: TouchPoint::new(0.5, 0.5),
                time_micros: 0,
            })
            .is_empty()
        );
        let events = tp.translate(TouchSample {
            phase: TouchPhase::Move,
            fingers: 1,
            point: TouchPoint::new(0.6, 0.5),
            time_micros: 10_000,
        });
        let [InputEvent::RelativeMove { dx, dy }] = events.as_slice() else {
            panic!("expected one relative move, got {events:?}");
        };
        assert!((*dx - 0.1 * 1.5).abs() < 1e-9);
        assert!(dy.abs() < 1e-9);
    }

    #[test]
    fn two_finger_drag_emits_scroll() {
        let mut tp = touchpad();
        tp.translate(TouchSample {
            phase: TouchPhase::Begin,
            fingers: 2,
            point: TouchPoint::new(0.5, 0.5),
            time_micros: 0,
        });
        let events = tp.translate(TouchSample {
            phase: TouchPhase::Move,
            fingers: 2,
            point: TouchPoint::new(0.5, 0.6),
            time_micros: 10_000,
        });
        assert!(matches!(events.as_slice(), [InputEvent::Scroll { .. }]));
    }

    #[test]
    fn quick_low_travel_tap_is_left_click() {
        let mut tp = touchpad();
        tp.translate(TouchSample {
            phase: TouchPhase::Begin,
            fingers: 1,
            point: TouchPoint::new(0.5, 0.5),
            time_micros: 0,
        });
        let events = tp.translate(TouchSample {
            phase: TouchPhase::End,
            fingers: 1,
            point: TouchPoint::new(0.505, 0.5),
            time_micros: 100_000,
        });
        assert_eq!(
            events,
            vec![
                InputEvent::ButtonPress(MouseButton::Left),
                InputEvent::ButtonRelease(MouseButton::Left),
            ]
        );
    }

    #[test]
    fn slow_long_drag_is_not_a_tap() {
        let mut tp = touchpad();
        tp.translate(TouchSample {
            phase: TouchPhase::Begin,
            fingers: 1,
            point: TouchPoint::new(0.5, 0.5),
            time_micros: 0,
        });
        tp.translate(TouchSample {
            phase: TouchPhase::Move,
            fingers: 1,
            point: TouchPoint::new(0.7, 0.5),
            time_micros: 50_000,
        });
        let events = tp.translate(TouchSample {
            phase: TouchPhase::End,
            fingers: 1,
            point: TouchPoint::new(0.7, 0.5),
            time_micros: 100_000,
        });
        assert!(events.is_empty());
    }

    #[test]
    fn gyro_first_sample_does_not_move() {
        let mut gyro = GyroMouse::new(GyroConfig::phone_default());
        assert!(gyro.update(Orientation::new(0.0, 0.0)).is_none());
    }

    #[test]
    fn gyro_yaw_maps_to_horizontal_motion() {
        let mut gyro = GyroMouse::new(GyroConfig::phone_default());
        gyro.update(Orientation::new(0.0, 0.0));
        let event = gyro.update(Orientation::new(0.1, 0.0)).unwrap();
        let InputEvent::RelativeMove { dx, dy } = event else {
            panic!("expected relative move");
        };
        assert!(dx > 0.0);
        assert!(dy.abs() < 1e-9);
    }

    #[test]
    fn gyro_deadzone_rejects_tremor() {
        let mut gyro = GyroMouse::new(GyroConfig::phone_default());
        gyro.update(Orientation::new(0.0, 0.0));
        // Sub-deadzone wobble on both axes.
        assert!(gyro.update(Orientation::new(0.001, -0.001)).is_none());
    }

    #[test]
    fn gyro_recenter_suppresses_next_delta() {
        let mut gyro = GyroMouse::new(GyroConfig::phone_default());
        gyro.update(Orientation::new(0.0, 0.0));
        gyro.recenter(Orientation::new(1.0, 1.0));
        // Next sample matches the recenter point → no motion.
        assert!(gyro.update(Orientation::new(1.0, 1.0)).is_none());
    }
}
