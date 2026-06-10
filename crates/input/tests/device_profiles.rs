//! End-to-end per-device profiles: distinct devices resolve to distinct
//! performance profiles, keyboard layouts, and hotkey mappings through the
//! public API, with a clean fallback to the default profile.

use nexkvm_core::identity::DeviceId;
use nexkvm_input::{
    DeviceProfileStore, DeviceUxProfile, Hotkey, HotkeyAction, KeyboardLayout, ModifierState,
    PointerMode,
};

const HID_TAB: u32 = 0x2B;
const HID_ESC: u32 = 0x29;

#[test]
fn devices_resolve_independent_profiles() {
    let gamer = DeviceId::generate();
    let laptop = DeviceId::generate();
    let unknown = DeviceId::generate();

    let mut store = DeviceProfileStore::new();

    // Gaming rig: low-latency raw-pointer profile + a quick-switch hotkey.
    let mut gaming = DeviceUxProfile::gaming();
    let switch_chord = Hotkey::new(
        ModifierState {
            meta: true,
            ..Default::default()
        },
        HID_TAB,
    );
    gaming
        .hotkeys
        .bind(switch_chord.clone(), HotkeyAction::SwitchNext);
    store.set_profile(gamer, gaming);

    // Laptop: normal desktop profile with an AZERTY layout + a command hotkey.
    let mut desktop = DeviceUxProfile::desktop();
    desktop.keyboard = KeyboardLayout {
        locale: "fr-FR".into(),
        layout: "azerty".into(),
        variant: None,
    };
    let cancel_chord = Hotkey::new(ModifierState::default(), HID_ESC);
    desktop
        .hotkeys
        .bind(cancel_chord.clone(), HotkeyAction::Command("cancel".into()));
    store.set_profile(laptop, desktop);

    // Performance profiles differ per device.
    assert_eq!(store.profile_for(gamer).input.mode, PointerMode::Raw);
    assert_eq!(store.profile_for(laptop).input.mode, PointerMode::Absolute);

    // Keyboard layout is per device.
    assert_eq!(store.keyboard_for(laptop).layout, "azerty");
    assert_eq!(store.keyboard_for(gamer).layout, "qwerty");

    // Hotkeys are per device.
    assert_eq!(
        store.resolve_hotkey(gamer, &switch_chord),
        Some(&HotkeyAction::SwitchNext)
    );
    assert_eq!(
        store.resolve_hotkey(laptop, &cancel_chord),
        Some(&HotkeyAction::Command("cancel".into()))
    );
    // Each device only sees its own bindings.
    assert!(store.resolve_hotkey(gamer, &cancel_chord).is_none());

    // Unknown devices fall back to the default profile (no overrides).
    assert_eq!(store.profile_for(unknown).input.mode, PointerMode::Absolute);
    assert!(store.hotkeys_for(unknown).is_empty());
}
