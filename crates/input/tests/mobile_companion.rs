//! End-to-end mobile companion input: a phone drives the cursor as a touchpad
//! and as a gyro mouse, producing the same platform-neutral `InputEvent`s a
//! desktop peer would — all through the public API.

use coklu_input::{
    GyroConfig, GyroMouse, InputEvent, MobileInputMode, MouseButton, Orientation, TouchPhase,
    TouchPoint, TouchSample, TouchpadConfig, TouchpadTranslator,
};

/// A one-finger swipe followed by a quick tap yields relative motion then a
/// left click, exactly as a trackpad peer would emit.
#[test]
fn phone_touchpad_drives_pointer_and_click() {
    assert_eq!(MobileInputMode::Touchpad, MobileInputMode::Touchpad);
    let mut pad = TouchpadTranslator::new(TouchpadConfig::phone_default());

    // Swipe one finger to the right.
    pad.translate(TouchSample {
        phase: TouchPhase::Begin,
        fingers: 1,
        point: TouchPoint::new(0.4, 0.5),
        time_micros: 0,
    });
    let moved = pad.translate(TouchSample {
        phase: TouchPhase::Move,
        fingers: 1,
        point: TouchPoint::new(0.5, 0.5),
        time_micros: 8_000,
    });
    assert!(matches!(
        moved.as_slice(),
        [InputEvent::RelativeMove { dx, .. }] if *dx > 0.0
    ));

    // A separate quick tap registers as a left click.
    pad.translate(TouchSample {
        phase: TouchPhase::Begin,
        fingers: 1,
        point: TouchPoint::new(0.5, 0.5),
        time_micros: 20_000,
    });
    let click = pad.translate(TouchSample {
        phase: TouchPhase::End,
        fingers: 1,
        point: TouchPoint::new(0.5, 0.5),
        time_micros: 80_000,
    });
    assert_eq!(
        click,
        vec![
            InputEvent::ButtonPress(MouseButton::Left),
            InputEvent::ButtonRelease(MouseButton::Left),
        ]
    );
}

/// Tilting the phone in gyro mode steers the cursor; the first sample only
/// establishes a reference and emits nothing.
#[test]
fn phone_gyro_steers_cursor() {
    let mut gyro = GyroMouse::new(GyroConfig::phone_default());
    assert!(gyro.update(Orientation::new(0.0, 0.0)).is_none());

    let event = gyro
        .update(Orientation::new(0.05, -0.03))
        .expect("orientation delta past deadzone moves the cursor");
    let InputEvent::RelativeMove { dx, dy } = event else {
        panic!("expected relative move from gyro");
    };
    assert!(dx > 0.0, "rightward yaw should move cursor right");
    assert!(dy < 0.0, "upward pitch should move cursor up");
}
