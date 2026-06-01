//! Input event model + capture/injection boundary.
//!
//! Defines the platform-neutral [`InputEvent`] that flows across the wire and
//! the [`InputCapture`]/[`InputInjector`] traits implemented by the `platform-*`
//! crates. Actual OS hooks (`CGEventTap`, `SendInput`, `libei`/evdev) live in
//! those crates behind these safe boundaries.
//!
//! # Platform constraints (flagged early)
//! - **macOS** needs Accessibility permission for both capture and injection.
//! - **Wayland** forbids global capture/injection; it must go through portals
//!   (`libei`). Callers should consult
//!   [`PlatformCapabilities`](coklu_core::PlatformCapabilities) first.
//! - **Windows** uses raw input + `SendInput`; injection into elevated windows
//!   may be blocked by UIPI.
//!
//! Coordinates are normalized `f64` in `[0.0, 1.0]` per axis so motion maps
//! across heterogeneous resolutions/DPI without the sender knowing the
//! receiver's geometry.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

mod acceleration;
mod batching;
mod boundary;
mod coalesce;
mod injection;
mod interpolation;
mod keyboard;
mod mobile;
mod mode;
mod monitor;
mod monitor_scale;
mod navigation;
mod polling;
mod prediction;
mod profile;
mod share;
mod topology;
mod transition;

pub use acceleration::{AccelerationCurve, SmartCursorAcceleration};
pub use batching::{InputBatchPolicy, InputBatcher};
pub use boundary::{BoundaryDetector, Edge, EdgeLink, Transition};
pub use coalesce::InputCoalescer;
pub use injection::{InjectionCommand, InjectionEngine};
pub use interpolation::CursorInterpolator;
pub use keyboard::{KeyForward, KeyboardShareController, Modifier, ModifierState};
pub use mobile::{
    GyroConfig, GyroMouse, MobileInputMode, Orientation, TouchPhase, TouchPoint, TouchSample,
    TouchpadConfig, TouchpadTranslator,
};
pub use mode::{InputProfile, PointerMode};
pub use monitor::{DisplayRect, MonitorId, MonitorLayout};
pub use monitor_scale::{LocalPoint, LogicalSize, MonitorScale, ScaledLayout};
pub use navigation::{
    CursorMotionSample, CursorThrow, CursorThrowPlanner, CursorThrowPolicy, GestureDirection,
    GestureFrame, GestureSwitchDecision, GestureSwitchPolicy, GestureSwitchRecognizer,
    InfiniteDesktopNavigator, InfiniteDesktopTransition, MomentumTransfer,
};
pub use polling::{AdaptivePoller, PollingPolicy};
pub use prediction::{CursorSample, PredictiveCursor};
pub use profile::{
    DeviceProfileStore, DeviceUxProfile, Hotkey, HotkeyAction, HotkeyBinding, HotkeyMap,
    KeyboardLayout, QuickSwitch,
};
pub use share::{CursorFocus, MouseShareController, PeerEntry, ShareOutput};
pub use topology::{
    DesktopPoint, DevicePlacement, DeviceTopologyEditor, MonitorPreview, PreviewRect,
    SpatialDesktopMap,
};
pub use transition::{
    CursorTransition, CursorTransitionEngine, CursorTransitionPolicy, JitterFilter,
};

/// Errors from input capture/injection.
#[derive(Debug, Error)]
pub enum InputError {
    /// The platform denied or lacks permission for the operation.
    #[error("input permission denied")]
    PermissionDenied,

    /// The backend failed to capture/inject.
    #[error("input backend error: {0}")]
    Backend(String),
}

/// Mouse buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton {
    /// Primary (usually left).
    Left,
    /// Secondary (usually right).
    Right,
    /// Middle / wheel click.
    Middle,
}

/// A single platform-neutral input event.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum InputEvent {
    /// Absolute pointer move, normalized to `[0.0, 1.0]` per axis.
    PointerMove {
        /// Horizontal position.
        x: f64,
        /// Vertical position.
        y: f64,
    },
    /// Relative pointer motion as a fraction of the screen per axis, with OS
    /// pointer acceleration already applied. Used in relative mode (e.g. when
    /// the remote cursor is hidden/captured).
    RelativeMove {
        /// Horizontal delta.
        dx: f64,
        /// Vertical delta.
        dy: f64,
    },
    /// Raw, unaccelerated device motion in device counts. Used in raw/gaming
    /// mode where the receiver applies its own (or no) acceleration so games
    /// see hardware-faithful input.
    RawMotion {
        /// Horizontal delta in device units.
        dx: i32,
        /// Vertical delta in device units.
        dy: i32,
    },
    /// Pointer button pressed.
    ButtonPress(MouseButton),
    /// Pointer button released.
    ButtonRelease(MouseButton),
    /// Scroll delta (lines).
    Scroll {
        /// Horizontal delta.
        dx: f64,
        /// Vertical delta.
        dy: f64,
    },
    /// Key pressed, identified by an OS-neutral keycode.
    KeyPress(u32),
    /// Key released.
    KeyRelease(u32),
}

impl InputEvent {
    /// Whether this event is a pointer-motion event (absolute, relative, or
    /// raw). Useful for coalescing and routing.
    #[must_use]
    pub fn is_motion(&self) -> bool {
        matches!(
            self,
            InputEvent::PointerMove { .. }
                | InputEvent::RelativeMove { .. }
                | InputEvent::RawMotion { .. }
        )
    }
}

/// Captures local input events to forward to peers.
#[async_trait]
pub trait InputCapture: Send + Sync {
    /// Receive the next captured input event.
    ///
    /// # Errors
    /// Returns [`InputError`] on permission or backend failure.
    async fn next_event(&self) -> Result<InputEvent, InputError>;
}

/// Injects received input events into the local OS.
#[async_trait]
pub trait InputInjector: Send + Sync {
    /// Synthesize `event` on the local machine.
    ///
    /// # Errors
    /// Returns [`InputError`] on permission or backend failure.
    async fn inject(&self, event: InputEvent) -> Result<(), InputError>;
}
