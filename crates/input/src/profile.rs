//! Device-specific UX profiles and quick switching.
//!
//! Advanced continuity UX is not one-size-fits-all: a tablet may use a
//! different keyboard layout, smoothing window, and pointer acceleration than a
//! desktop monitor wall. This module stores per-device preferences and a small
//! quick-switch state machine for cycling active targets.

use std::collections::HashMap;

use nexkvm_core::identity::DeviceId;
use serde::{Deserialize, Serialize};

use crate::acceleration::SmartCursorAcceleration;
use crate::keyboard::ModifierState;
use crate::mode::InputProfile;

/// Keyboard layout applied when routing keyboard input to a specific device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyboardLayout {
    /// BCP-47-ish locale tag, e.g. `en-US`, `de-DE`, `ja-JP`.
    pub locale: String,
    /// Physical/logical layout name, e.g. `qwerty`, `azerty`, `jis`.
    pub layout: String,
    /// Optional variant such as `intl`.
    pub variant: Option<String>,
}

impl KeyboardLayout {
    /// US QWERTY default.
    #[must_use]
    pub fn us_qwerty() -> Self {
        Self {
            locale: "en-US".into(),
            layout: "qwerty".into(),
            variant: None,
        }
    }
}

impl Default for KeyboardLayout {
    fn default() -> Self {
        Self::us_qwerty()
    }
}

/// A keyboard chord: a non-modifier key plus the modifiers held with it.
///
/// `keycode` is the **USB HID usage ID** of the triggering key (the same
/// cross-platform-neutral convention the wire format and
/// [`Modifier`](crate::Modifier) use), and must itself be a non-modifier key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hotkey {
    /// Modifiers that must be held for the chord to match.
    pub modifiers: ModifierState,
    /// HID usage ID of the non-modifier key.
    pub keycode: u32,
}

impl Hotkey {
    /// Build a chord from explicit modifiers and a key.
    #[must_use]
    pub fn new(modifiers: ModifierState, keycode: u32) -> Self {
        Self { modifiers, keycode }
    }
}

/// What a per-device [`Hotkey`] does when matched.
///
/// The input crate only resolves the *intent*; the daemon interprets it against
/// live state (quick-switch order, active mode, command runtime).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HotkeyAction {
    /// Cycle quick-switch focus to the next device.
    SwitchNext,
    /// Cycle quick-switch focus to the previous device.
    SwitchPrevious,
    /// Switch focus directly to a specific device.
    SwitchTo(DeviceId),
    /// Toggle the pointer mode (e.g. absolute ↔ raw) for the target.
    TogglePointerMode,
    /// Run a named command resolved by the daemon's command runtime.
    Command(String),
}

/// A single `(chord → action)` binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HotkeyBinding {
    /// The chord that triggers this binding.
    pub hotkey: Hotkey,
    /// The action to perform.
    pub action: HotkeyAction,
}

/// Per-device hotkey mapping.
///
/// Backed by a small `Vec` rather than a `HashMap`: bindings number in the
/// handful, linear lookup is cache-friendly at that size, and it serializes as
/// a plain sequence (so it survives non-string-key formats like TOML).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HotkeyMap {
    bindings: Vec<HotkeyBinding>,
}

impl HotkeyMap {
    /// Create an empty map.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind `hotkey` to `action`, replacing any existing binding for the same
    /// chord. Returns the previous action, if any.
    pub fn bind(&mut self, hotkey: Hotkey, action: HotkeyAction) -> Option<HotkeyAction> {
        if let Some(existing) = self.bindings.iter_mut().find(|b| b.hotkey == hotkey) {
            Some(std::mem::replace(&mut existing.action, action))
        } else {
            self.bindings.push(HotkeyBinding { hotkey, action });
            None
        }
    }

    /// Remove the binding for `hotkey`. Returns the action that was bound.
    pub fn unbind(&mut self, hotkey: &Hotkey) -> Option<HotkeyAction> {
        let index = self.bindings.iter().position(|b| &b.hotkey == hotkey)?;
        Some(self.bindings.remove(index).action)
    }

    /// Resolve the action bound to `hotkey`, if any.
    #[must_use]
    pub fn resolve(&self, hotkey: &Hotkey) -> Option<&HotkeyAction> {
        self.bindings
            .iter()
            .find(|b| &b.hotkey == hotkey)
            .map(|b| &b.action)
    }

    /// Whether no bindings are present.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }

    /// Number of bindings.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bindings.len()
    }

    /// Iterate over the bindings.
    pub fn iter(&self) -> impl Iterator<Item = &HotkeyBinding> {
        self.bindings.iter()
    }
}

/// Complete per-device UX profile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceUxProfile {
    /// Input pipeline profile (mode, smoothing/coalescing/send interval).
    pub input: InputProfile,
    /// Keyboard layout for key translation on this target.
    pub keyboard: KeyboardLayout,
    /// Smart cursor acceleration for relative pointer deltas.
    pub acceleration: SmartCursorAcceleration,
    /// Per-device hotkey mapping.
    #[serde(default)]
    pub hotkeys: HotkeyMap,
}

impl DeviceUxProfile {
    /// General desktop continuity profile.
    #[must_use]
    pub fn desktop() -> Self {
        Self {
            input: InputProfile::desktop(),
            keyboard: KeyboardLayout::default(),
            acceleration: SmartCursorAcceleration::desktop_default(),
            hotkeys: HotkeyMap::new(),
        }
    }

    /// Low-latency gaming profile.
    #[must_use]
    pub fn gaming() -> Self {
        Self {
            input: InputProfile::gaming(),
            keyboard: KeyboardLayout::default(),
            acceleration: SmartCursorAcceleration::disabled(),
            hotkeys: HotkeyMap::new(),
        }
    }
}

impl Default for DeviceUxProfile {
    fn default() -> Self {
        Self::desktop()
    }
}

/// In-memory profile store keyed by device id.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DeviceProfileStore {
    default_profile: DeviceUxProfile,
    profiles: HashMap<DeviceId, DeviceUxProfile>,
}

impl DeviceProfileStore {
    /// Create a store with the default desktop profile.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Profile used for devices without an override.
    #[must_use]
    pub fn default_profile(&self) -> &DeviceUxProfile {
        &self.default_profile
    }

    /// Replace the default profile.
    pub fn set_default_profile(&mut self, profile: DeviceUxProfile) {
        self.default_profile = profile;
    }

    /// Set an override for `device`.
    pub fn set_profile(&mut self, device: DeviceId, profile: DeviceUxProfile) {
        self.profiles.insert(device, profile);
    }

    /// Remove an override. Returns whether one existed.
    pub fn remove_profile(&mut self, device: DeviceId) -> bool {
        self.profiles.remove(&device).is_some()
    }

    /// Resolve a profile, falling back to the default.
    #[must_use]
    pub fn profile_for(&self, device: DeviceId) -> &DeviceUxProfile {
        self.profiles.get(&device).unwrap_or(&self.default_profile)
    }

    /// Resolve keyboard layout for a target device.
    #[must_use]
    pub fn keyboard_for(&self, device: DeviceId) -> &KeyboardLayout {
        &self.profile_for(device).keyboard
    }

    /// Resolve the hotkey map for a target device.
    #[must_use]
    pub fn hotkeys_for(&self, device: DeviceId) -> &HotkeyMap {
        &self.profile_for(device).hotkeys
    }

    /// Resolve the action a chord triggers for a target device, if bound.
    #[must_use]
    pub fn resolve_hotkey(&self, device: DeviceId, hotkey: &Hotkey) -> Option<&HotkeyAction> {
        self.profile_for(device).hotkeys.resolve(hotkey)
    }
}

/// Ordered target selector for multi-device quick switch.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuickSwitch {
    order: Vec<DeviceId>,
    active: Option<DeviceId>,
}

impl QuickSwitch {
    /// Create an empty quick switcher.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Current ordered candidate list.
    #[must_use]
    pub fn order(&self) -> &[DeviceId] {
        &self.order
    }

    /// Active target, if any.
    #[must_use]
    pub fn active(&self) -> Option<DeviceId> {
        self.active
    }

    /// Replace candidate order, de-duplicating while preserving first entries.
    pub fn set_order(&mut self, devices: Vec<DeviceId>) {
        self.order.clear();
        for device in devices {
            if !self.order.contains(&device) {
                self.order.push(device);
            }
        }
        if self
            .active
            .is_none_or(|active| !self.order.contains(&active))
        {
            self.active = self.order.first().copied();
        }
    }

    /// Select a specific candidate. Returns whether it existed.
    pub fn select(&mut self, device: DeviceId) -> bool {
        if self.order.contains(&device) {
            self.active = Some(device);
            true
        } else {
            false
        }
    }

    /// Advance to the next candidate, wrapping around.
    pub fn next_device(&mut self) -> Option<DeviceId> {
        self.step(1)
    }

    /// Move to the previous candidate, wrapping around.
    pub fn previous_device(&mut self) -> Option<DeviceId> {
        self.step(-1)
    }

    fn step(&mut self, direction: isize) -> Option<DeviceId> {
        if self.order.is_empty() {
            self.active = None;
            return None;
        }
        let current_index = self
            .active
            .and_then(|active| self.order.iter().position(|device| *device == active))
            .unwrap_or(0);
        let len = self.order.len() as isize;
        let next_index = (current_index as isize + direction).rem_euclid(len) as usize;
        self.active = Some(self.order[next_index]);
        self.active
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PointerMode;

    #[test]
    fn profile_store_falls_back_and_overrides() {
        let device = DeviceId::generate();
        let mut store = DeviceProfileStore::new();
        assert_eq!(store.profile_for(device).input.mode, PointerMode::Absolute);
        store.set_profile(device, DeviceUxProfile::gaming());
        assert_eq!(store.profile_for(device).input.mode, PointerMode::Raw);
        assert!(store.remove_profile(device));
        assert_eq!(store.profile_for(device).input.mode, PointerMode::Absolute);
    }

    #[test]
    fn per_device_keyboard_layout_is_resolved() {
        let device = DeviceId::generate();
        let mut store = DeviceProfileStore::new();
        let mut profile = DeviceUxProfile::desktop();
        profile.keyboard = KeyboardLayout {
            locale: "fr-FR".into(),
            layout: "azerty".into(),
            variant: None,
        };
        store.set_profile(device, profile);
        assert_eq!(store.keyboard_for(device).layout, "azerty");
    }

    #[test]
    fn per_device_hotkey_is_resolved_and_rebound() {
        let device = DeviceId::generate();
        let chord = Hotkey::new(
            ModifierState {
                meta: true,
                ..Default::default()
            },
            0x2B, // HID Tab
        );

        let mut profile = DeviceUxProfile::desktop();
        assert!(
            profile
                .hotkeys
                .bind(chord.clone(), HotkeyAction::SwitchNext)
                .is_none()
        );
        // Rebinding the same chord returns the previous action.
        assert_eq!(
            profile
                .hotkeys
                .bind(chord.clone(), HotkeyAction::SwitchPrevious),
            Some(HotkeyAction::SwitchNext)
        );

        let mut store = DeviceProfileStore::new();
        store.set_profile(device, profile);

        assert_eq!(
            store.resolve_hotkey(device, &chord),
            Some(&HotkeyAction::SwitchPrevious)
        );
        // Unknown device falls back to the (empty) default profile.
        let other = DeviceId::generate();
        assert!(store.resolve_hotkey(other, &chord).is_none());
    }

    #[test]
    fn hotkey_map_unbind_removes_binding() {
        let chord = Hotkey::new(ModifierState::default(), 0x29); // HID Esc
        let mut map = HotkeyMap::new();
        map.bind(chord.clone(), HotkeyAction::Command("cancel".into()));
        assert_eq!(map.len(), 1);
        assert_eq!(
            map.unbind(&chord),
            Some(HotkeyAction::Command("cancel".into()))
        );
        assert!(map.is_empty());
        assert!(map.unbind(&chord).is_none());
    }

    #[test]
    fn quick_switch_cycles_and_dedups() {
        let first = DeviceId::generate();
        let second = DeviceId::generate();
        let third = DeviceId::generate();
        let mut switcher = QuickSwitch::new();
        switcher.set_order(vec![first, second, first, third]);
        assert_eq!(switcher.order(), &[first, second, third]);
        assert_eq!(switcher.active(), Some(first));
        assert_eq!(switcher.next_device(), Some(second));
        assert_eq!(switcher.next_device(), Some(third));
        assert_eq!(switcher.next_device(), Some(first));
        assert_eq!(switcher.previous_device(), Some(third));
    }
}
