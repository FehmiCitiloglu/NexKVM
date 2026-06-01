//! Pointer modes and input profiles (incl. gaming-optimized) ⭐🔥⚡
//!
//! Different use-cases trade smoothness against latency:
//!
//! - **Absolute** ([`PointerMode::Absolute`]) — normalized cursor positions; the
//!   default for desktop continuity. Pairs with interpolation and coalescing for
//!   smooth, bandwidth-efficient motion.
//! - **Relative** ([`PointerMode::Relative`]) — accelerated deltas; used when the
//!   remote pointer is captured/hidden (e.g. a 3D viewport).
//! - **Raw** ([`PointerMode::Raw`]) — unaccelerated device counts; required for
//!   games and FPS aiming so the receiver sees hardware-faithful motion.
//!
//! An [`InputProfile`] bundles a mode with the latency/throughput knobs the rest
//! of the pipeline reads: whether to interpolate ([`crate::CursorInterpolator`]),
//! whether to coalesce motion ([`crate::InputCoalescer`]), and the minimum send
//! interval. [`InputProfile::gaming`] is the low-latency preset: raw motion, no
//! interpolation, no coalescing, send-immediately.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// How pointer motion is expressed on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PointerMode {
    /// Absolute normalized `[0,1]` positions.
    Absolute,
    /// Accelerated relative deltas (fraction of screen per axis).
    Relative,
    /// Raw, unaccelerated device-count deltas.
    Raw,
}

/// Tunable input pipeline behaviour for a session.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct InputProfile {
    /// Motion representation for this session.
    pub mode: PointerMode,
    /// Ease the rendered cursor between samples (adds ~one window of latency).
    pub interpolate: bool,
    /// Merge consecutive motion events before sending to cut packet rate.
    pub coalesce: bool,
    /// Minimum spacing between outbound motion sends. `ZERO` means send every
    /// event immediately (lowest latency, highest packet rate).
    pub min_send_interval: Duration,
    /// Render-side cursor animation smoothing window. Ignored when
    /// `interpolate` is `false`.
    pub smoothing_window: Duration,
}

impl InputProfile {
    /// Desktop continuity preset: smooth and bandwidth-friendly.
    ///
    /// Absolute positions, interpolation on, coalescing on, ~125 Hz send cap.
    #[must_use]
    pub const fn desktop() -> Self {
        Self {
            mode: PointerMode::Absolute,
            interpolate: true,
            coalesce: true,
            min_send_interval: Duration::from_millis(8),
            smoothing_window: Duration::from_millis(8),
        }
    }

    /// Gaming-optimized preset: lowest latency, hardware-faithful motion. ⚡
    ///
    /// Raw deltas, no interpolation, no coalescing, send-immediately.
    #[must_use]
    pub const fn gaming() -> Self {
        Self {
            mode: PointerMode::Raw,
            interpolate: false,
            coalesce: false,
            min_send_interval: Duration::ZERO,
            smoothing_window: Duration::ZERO,
        }
    }

    /// Captured-pointer preset for relative control surfaces.
    ///
    /// Relative deltas, no interpolation (deltas are integrated locally),
    /// coalescing on, ~125 Hz send cap.
    #[must_use]
    pub const fn relative() -> Self {
        Self {
            mode: PointerMode::Relative,
            interpolate: false,
            coalesce: true,
            min_send_interval: Duration::from_millis(8),
            smoothing_window: Duration::ZERO,
        }
    }

    /// Whether this profile prioritizes latency over smoothness/bandwidth.
    #[must_use]
    pub const fn is_low_latency(&self) -> bool {
        !self.interpolate && !self.coalesce && self.min_send_interval.is_zero()
    }
}

impl Default for InputProfile {
    fn default() -> Self {
        Self::desktop()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_desktop() {
        assert_eq!(InputProfile::default(), InputProfile::desktop());
    }

    #[test]
    fn gaming_is_raw_and_low_latency() {
        let g = InputProfile::gaming();
        assert_eq!(g.mode, PointerMode::Raw);
        assert!(g.is_low_latency());
        assert!(!g.interpolate);
        assert!(!g.coalesce);
        assert!(g.min_send_interval.is_zero());
    }

    #[test]
    fn desktop_is_not_low_latency() {
        assert!(!InputProfile::desktop().is_low_latency());
    }

    #[test]
    fn relative_uses_relative_mode() {
        assert_eq!(InputProfile::relative().mode, PointerMode::Relative);
    }
}
