//! End-to-end keyboard sharing: route keystrokes to the active device, then
//! switch focus mid-chord and verify no modifier is left stuck on the old
//! target — the canonical keyboard-sharing failure mode — all through the
//! public API.

use coklu_core::identity::DeviceId;
use coklu_input::{
    DeviceProfileStore, DeviceUxProfile, InputEvent, KeyboardLayout, KeyboardShareController,
};

/// HID usage IDs used on the wire.
const HID_META_LEFT: u32 = 0xE3; // Cmd / Super
const KEY_C: u32 = 0x06;

fn store_with_layouts(us: DeviceId, tr: DeviceId) -> DeviceProfileStore {
    let mut store = DeviceProfileStore::new();

    let mut us_profile = DeviceUxProfile::desktop();
    us_profile.keyboard = KeyboardLayout::us_qwerty();
    store.set_profile(us, us_profile);

    let mut tr_profile = DeviceUxProfile::desktop();
    tr_profile.keyboard = KeyboardLayout {
        locale: "tr-TR".into(),
        layout: "qwerty".into(),
        variant: Some("turkish-q".into()),
    };
    store.set_profile(tr, tr_profile);

    store
}

#[test]
fn focus_switch_mid_chord_does_not_leak_modifiers() {
    let laptop_us = DeviceId::generate();
    let desktop_tr = DeviceId::generate();
    let mut kbd = KeyboardShareController::new(store_with_layouts(laptop_us, desktop_tr));

    // 1. Focus the US laptop; type Cmd+C there.
    assert!(kbd.set_active(laptop_us).is_empty());
    let meta = kbd.on_key(InputEvent::KeyPress(HID_META_LEFT)).unwrap();
    assert_eq!(meta.peer, laptop_us);
    assert!(meta.modifiers.meta);
    assert_eq!(meta.layout.locale, "en-US");

    let copy = kbd.on_key(InputEvent::KeyPress(KEY_C)).unwrap();
    assert!(copy.modifiers.meta, "Cmd still held while C goes down");

    // 2. User switches focus to the TR desktop while Cmd+C is still physically
    //    held. The controller must release both keys on the US laptop.
    let releases = kbd.set_active(desktop_tr);
    assert_eq!(releases.len(), 2, "Cmd and C both released");
    assert!(
        releases.iter().all(|r| r.peer == laptop_us),
        "releases go to the device we left"
    );
    assert!(
        releases
            .iter()
            .all(|r| matches!(r.event, InputEvent::KeyRelease(_))),
        "synthetic key-ups only"
    );

    // 3. The new target starts clean and uses its own (TR) layout.
    assert!(kbd.modifiers().is_empty(), "no leaked modifiers");
    let key = kbd.on_key(InputEvent::KeyPress(KEY_C)).unwrap();
    assert_eq!(key.peer, desktop_tr);
    assert_eq!(key.layout.locale, "tr-TR");
    assert_eq!(key.layout.variant.as_deref(), Some("turkish-q"));
    assert!(!key.modifiers.meta, "Cmd was not carried over");
}
