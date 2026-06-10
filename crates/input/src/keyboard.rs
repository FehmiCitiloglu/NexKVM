//! Keyboard-sharing controller: routes key events to the active device while
//! keeping modifier state coherent across focus switches.
//!
//! Pointer hand-off ([`MouseShareController`](crate::MouseShareController))
//! decides *where* input goes; this controller decides *what keyboard state*
//! travels with it. It owns three things the raw [`InputEvent::KeyPress`] /
//! [`InputEvent::KeyRelease`] stream does not carry on its own:
//!
//! 1. **Active focus** — which paired device currently receives keystrokes.
//! 2. **Held-key tracking** — the set of keys physically down right now, so a
//!    focus switch can synthesize releases for them. Without this, switching
//!    devices while holding `Ctrl`/`Cmd` leaves a **stuck modifier** on the old
//!    target — the single most common keyboard-sharing bug.
//! 3. **Layout + modifier context** — each forwarded key carries the target's
//!    resolved [`KeyboardLayout`] and a [`ModifierState`] snapshot, so the
//!    receiver injects with the right layout (TR/US/DE…) and modifier semantics.
//!
//! Modifier keys are identified by their **USB HID usage IDs** (`0xE0..=0xE7`),
//! the cross-platform-neutral keycode convention the wire format uses; see
//! [`Modifier::from_keycode`]. Pure logic: no OS calls, no network, no clock —
//! the platform/network driver feeds events and acts on the returned
//! [`KeyForward`]s.

use std::collections::BTreeSet;

use nexkvm_core::identity::DeviceId;
use serde::{Deserialize, Serialize};

use crate::InputEvent;
use crate::profile::{DeviceProfileStore, KeyboardLayout};

/// HID usage IDs for the eight modifier keys (left/right of each pair).
const HID_CTRL_LEFT: u32 = 0xE0;
const HID_SHIFT_LEFT: u32 = 0xE1;
const HID_ALT_LEFT: u32 = 0xE2;
const HID_META_LEFT: u32 = 0xE3;
const HID_CTRL_RIGHT: u32 = 0xE4;
const HID_SHIFT_RIGHT: u32 = 0xE5;
const HID_ALT_RIGHT: u32 = 0xE6;
const HID_META_RIGHT: u32 = 0xE7;

/// A logical modifier, independent of which physical (left/right) key is held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Modifier {
    /// Control.
    Ctrl,
    /// Alt / Option.
    Alt,
    /// Shift.
    Shift,
    /// Meta — Command on macOS, Windows/Super elsewhere.
    Meta,
}

impl Modifier {
    /// Classify a keycode as a modifier, if it is one.
    ///
    /// Recognizes the USB HID modifier usage IDs `0xE0..=0xE7`; any other
    /// keycode is a normal key and yields `None`.
    #[must_use]
    pub fn from_keycode(keycode: u32) -> Option<Self> {
        match keycode {
            HID_CTRL_LEFT | HID_CTRL_RIGHT => Some(Self::Ctrl),
            HID_SHIFT_LEFT | HID_SHIFT_RIGHT => Some(Self::Shift),
            HID_ALT_LEFT | HID_ALT_RIGHT => Some(Self::Alt),
            HID_META_LEFT | HID_META_RIGHT => Some(Self::Meta),
            _ => None,
        }
    }
}

/// Which logical modifiers are currently held, derived from the held keys.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModifierState {
    /// Control is held.
    pub ctrl: bool,
    /// Alt / Option is held.
    pub alt: bool,
    /// Shift is held.
    pub shift: bool,
    /// Meta (Cmd/Super) is held.
    pub meta: bool,
}

impl ModifierState {
    /// Whether no modifier is currently held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !(self.ctrl || self.alt || self.shift || self.meta)
    }

    fn set(&mut self, modifier: Modifier, down: bool) {
        match modifier {
            Modifier::Ctrl => self.ctrl = down,
            Modifier::Alt => self.alt = down,
            Modifier::Shift => self.shift = down,
            Modifier::Meta => self.meta = down,
        }
    }
}

/// A key event to deliver to a specific device, with the context the receiver
/// needs to inject it faithfully.
#[derive(Debug, Clone, PartialEq)]
pub struct KeyForward {
    /// The device that should receive the key event.
    pub peer: DeviceId,
    /// The key event ([`InputEvent::KeyPress`] / [`InputEvent::KeyRelease`]).
    pub event: InputEvent,
    /// The target device's resolved keyboard layout (TR/US/DE…).
    pub layout: KeyboardLayout,
    /// Modifier state *after* this event is applied.
    pub modifiers: ModifierState,
}

/// Routes keyboard input to the active device and keeps modifier state coherent
/// across focus switches.
#[derive(Debug, Clone)]
pub struct KeyboardShareController {
    profiles: DeviceProfileStore,
    active: Option<DeviceId>,
    /// Keys currently held on the active target (deterministic order for
    /// reproducible synthetic-release sequences).
    held: BTreeSet<u32>,
    modifiers: ModifierState,
}

impl KeyboardShareController {
    /// Create a controller resolving per-device layouts from `profiles`.
    #[must_use]
    pub fn new(profiles: DeviceProfileStore) -> Self {
        Self {
            profiles,
            active: None,
            held: BTreeSet::new(),
            modifiers: ModifierState::default(),
        }
    }

    /// The device currently receiving keystrokes, if any.
    #[must_use]
    pub fn active(&self) -> Option<DeviceId> {
        self.active
    }

    /// The current modifier state on the active target.
    #[must_use]
    pub fn modifiers(&self) -> ModifierState {
        self.modifiers
    }

    /// Switch keyboard focus to `device`.
    ///
    /// Returns synthetic [`InputEvent::KeyRelease`] forwards for every key that
    /// was still held on the **previous** target, so no modifier or key is left
    /// stuck down there. Switching to the already-active device is a no-op and
    /// returns an empty vec.
    pub fn set_active(&mut self, device: DeviceId) -> Vec<KeyForward> {
        if self.active == Some(device) {
            return Vec::new();
        }
        let releases = self.drain_held_releases();
        self.active = Some(device);
        releases
    }

    /// Clear the active device, releasing any held keys on it first.
    ///
    /// Returns the synthetic releases to flush to the (now former) target, e.g.
    /// on disconnect.
    pub fn clear_active(&mut self) -> Vec<KeyForward> {
        let releases = self.drain_held_releases();
        self.active = None;
        releases
    }

    /// Route a key event to the active device.
    ///
    /// Tracks held keys and updates [`modifiers`](Self::modifiers), then returns
    /// the [`KeyForward`] to send (carrying the post-event modifier snapshot and
    /// the target layout). Returns `None` when there is no active device or when
    /// `event` is not a key event (pointer events are handled elsewhere).
    pub fn on_key(&mut self, event: InputEvent) -> Option<KeyForward> {
        let active = self.active?;
        match event {
            InputEvent::KeyPress(code) => {
                self.held.insert(code);
                if let Some(m) = Modifier::from_keycode(code) {
                    self.modifiers.set(m, true);
                }
            }
            InputEvent::KeyRelease(code) => {
                self.held.remove(&code);
                if let Some(m) = Modifier::from_keycode(code) {
                    // A modifier is only "up" once neither side is held.
                    if !self.is_modifier_still_held(m) {
                        self.modifiers.set(m, false);
                    }
                }
            }
            _ => return None,
        }
        Some(KeyForward {
            peer: active,
            event,
            layout: self.profiles.keyboard_for(active).clone(),
            modifiers: self.modifiers,
        })
    }

    /// Whether any physical key mapping to `modifier` is still held.
    fn is_modifier_still_held(&self, modifier: Modifier) -> bool {
        self.held
            .iter()
            .any(|&code| Modifier::from_keycode(code) == Some(modifier))
    }

    /// Emit releases for all held keys on the current active target and reset
    /// held/modifier state.
    fn drain_held_releases(&mut self) -> Vec<KeyForward> {
        let Some(active) = self.active else {
            self.held.clear();
            self.modifiers = ModifierState::default();
            return Vec::new();
        };
        let layout = self.profiles.keyboard_for(active).clone();
        // Release in reverse order (LIFO) so modifiers pressed first are
        // released last, mirroring natural key-up ordering.
        let releases = self
            .held
            .iter()
            .rev()
            .map(|&code| KeyForward {
                peer: active,
                event: InputEvent::KeyRelease(code),
                layout: layout.clone(),
                modifiers: ModifierState::default(),
            })
            .collect();
        self.held.clear();
        self.modifiers = ModifierState::default();
        releases
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{DeviceUxProfile, KeyboardLayout};

    const KEY_A: u32 = 0x04; // HID usage for 'a'

    fn controller() -> KeyboardShareController {
        KeyboardShareController::new(DeviceProfileStore::new())
    }

    #[test]
    fn no_active_device_drops_keys() {
        let mut ctrl = controller();
        assert!(ctrl.on_key(InputEvent::KeyPress(KEY_A)).is_none());
    }

    #[test]
    fn forwards_key_to_active_device_with_layout() {
        let device = DeviceId::generate();
        let mut store = DeviceProfileStore::new();
        let mut profile = DeviceUxProfile::desktop();
        profile.keyboard = KeyboardLayout {
            locale: "tr-TR".into(),
            layout: "qwerty".into(),
            variant: Some("turkish-q".into()),
        };
        store.set_profile(device, profile);

        let mut ctrl = KeyboardShareController::new(store);
        ctrl.set_active(device);

        let fwd = ctrl.on_key(InputEvent::KeyPress(KEY_A)).unwrap();
        assert_eq!(fwd.peer, device);
        assert_eq!(fwd.event, InputEvent::KeyPress(KEY_A));
        assert_eq!(fwd.layout.locale, "tr-TR");
        assert_eq!(fwd.layout.variant.as_deref(), Some("turkish-q"));
    }

    #[test]
    fn tracks_modifier_state_across_press_and_release() {
        let device = DeviceId::generate();
        let mut ctrl = controller();
        ctrl.set_active(device);

        let fwd = ctrl.on_key(InputEvent::KeyPress(HID_CTRL_LEFT)).unwrap();
        assert!(fwd.modifiers.ctrl);
        assert!(ctrl.modifiers().ctrl);

        let fwd = ctrl.on_key(InputEvent::KeyRelease(HID_CTRL_LEFT)).unwrap();
        assert!(!fwd.modifiers.ctrl);
        assert!(ctrl.modifiers().is_empty());
    }

    #[test]
    fn modifier_stays_held_until_both_sides_released() {
        let device = DeviceId::generate();
        let mut ctrl = controller();
        ctrl.set_active(device);

        ctrl.on_key(InputEvent::KeyPress(HID_CTRL_LEFT));
        ctrl.on_key(InputEvent::KeyPress(HID_CTRL_RIGHT));
        // Releasing one side keeps Ctrl active.
        let fwd = ctrl.on_key(InputEvent::KeyRelease(HID_CTRL_LEFT)).unwrap();
        assert!(fwd.modifiers.ctrl, "other ctrl key still down");
        // Releasing the second clears it.
        let fwd = ctrl.on_key(InputEvent::KeyRelease(HID_CTRL_RIGHT)).unwrap();
        assert!(!fwd.modifiers.ctrl);
    }

    #[test]
    fn switching_focus_releases_held_keys_on_old_target() {
        let first = DeviceId::generate();
        let second = DeviceId::generate();
        let mut ctrl = controller();
        ctrl.set_active(first);

        // Hold Cmd + A on the first device, then switch.
        ctrl.on_key(InputEvent::KeyPress(HID_META_LEFT));
        ctrl.on_key(InputEvent::KeyPress(KEY_A));

        let releases = ctrl.set_active(second);
        // Both held keys are released, addressed to the *first* device.
        assert_eq!(releases.len(), 2);
        assert!(releases.iter().all(|r| r.peer == first));
        assert!(
            releases
                .iter()
                .all(|r| matches!(r.event, InputEvent::KeyRelease(_)))
        );
        let released: Vec<_> = releases
            .iter()
            .map(|r| match r.event {
                InputEvent::KeyRelease(c) => c,
                _ => unreachable!(),
            })
            .collect();
        assert!(released.contains(&HID_META_LEFT));
        assert!(released.contains(&KEY_A));

        // New target starts with a clean modifier slate.
        assert_eq!(ctrl.active(), Some(second));
        assert!(ctrl.modifiers().is_empty());
    }

    #[test]
    fn reswitching_to_same_device_is_noop() {
        let device = DeviceId::generate();
        let mut ctrl = controller();
        ctrl.set_active(device);
        ctrl.on_key(InputEvent::KeyPress(HID_CTRL_LEFT));
        // Same device: no releases, modifier stays held.
        assert!(ctrl.set_active(device).is_empty());
        assert!(ctrl.modifiers().ctrl);
    }

    #[test]
    fn clear_active_flushes_releases() {
        let device = DeviceId::generate();
        let mut ctrl = controller();
        ctrl.set_active(device);
        ctrl.on_key(InputEvent::KeyPress(HID_ALT_LEFT));

        let releases = ctrl.clear_active();
        assert_eq!(releases.len(), 1);
        assert_eq!(releases[0].event, InputEvent::KeyRelease(HID_ALT_LEFT));
        assert_eq!(ctrl.active(), None);
        assert!(ctrl.modifiers().is_empty());
    }
}
