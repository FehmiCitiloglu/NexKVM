//! Smart cursor acceleration.
//!
//! Pointer acceleration is device- and task-specific. A trackpad moving across a
//! spatial desktop map benefits from gentle acceleration, while raw gaming mode
//! must stay hardware-faithful. This module provides a pure curve that can be
//! attached per device profile and disabled for low-latency/raw sessions.

use serde::{Deserialize, Serialize};

/// Curve used to derive an acceleration multiplier from pointer speed.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum AccelerationCurve {
    /// Linear multiplier above the threshold.
    Linear,
    /// Power curve above the threshold.
    Power {
        /// Exponent applied to normalized speed above threshold.
        exponent: f64,
    },
}

/// Per-device smart cursor acceleration settings.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SmartCursorAcceleration {
    /// Master switch.
    pub enabled: bool,
    /// Speed below which motion is left untouched.
    pub threshold: f64,
    /// Gain applied above threshold.
    pub gain: f64,
    /// Maximum multiplier.
    pub max_multiplier: f64,
    /// Curve shape.
    pub curve: AccelerationCurve,
}

impl SmartCursorAcceleration {
    /// Balanced desktop preset.
    #[must_use]
    pub const fn desktop_default() -> Self {
        Self {
            enabled: true,
            threshold: 4.0,
            gain: 0.35,
            max_multiplier: 3.0,
            curve: AccelerationCurve::Power { exponent: 1.25 },
        }
    }

    /// Raw/low-latency preset: no acceleration.
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            enabled: false,
            threshold: 1.0,
            gain: 0.0,
            max_multiplier: 1.0,
            curve: AccelerationCurve::Linear,
        }
    }

    /// Apply the curve to a relative delta.
    #[must_use]
    pub fn apply_delta(&self, delta_x: f64, delta_y: f64) -> (f64, f64) {
        let multiplier = self.multiplier_for_speed(delta_x.hypot(delta_y));
        (delta_x * multiplier, delta_y * multiplier)
    }

    /// Multiplier for a scalar pointer speed.
    #[must_use]
    pub fn multiplier_for_speed(&self, speed: f64) -> f64 {
        if !self.enabled || speed <= self.threshold || self.threshold <= 0.0 {
            return 1.0;
        }
        let normalized = (speed / self.threshold) - 1.0;
        let shaped = match self.curve {
            AccelerationCurve::Linear => normalized,
            AccelerationCurve::Power { exponent } => normalized.powf(exponent.max(0.1)),
        };
        (1.0 + shaped * self.gain).clamp(1.0, self.max_multiplier.max(1.0))
    }
}

impl Default for SmartCursorAcceleration {
    fn default() -> Self {
        Self::desktop_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_leaves_motion_unchanged() {
        let acceleration = SmartCursorAcceleration::disabled();
        assert_eq!(acceleration.apply_delta(10.0, 0.0), (10.0, 0.0));
    }

    #[test]
    fn below_threshold_is_unmodified() {
        let acceleration = SmartCursorAcceleration::desktop_default();
        assert_eq!(acceleration.multiplier_for_speed(2.0), 1.0);
    }

    #[test]
    fn fast_motion_accelerates_but_caps() {
        let acceleration = SmartCursorAcceleration {
            threshold: 1.0,
            gain: 10.0,
            max_multiplier: 2.0,
            curve: AccelerationCurve::Linear,
            enabled: true,
        };
        assert_eq!(acceleration.multiplier_for_speed(100.0), 2.0);
        let (delta_x, delta_y) = acceleration.apply_delta(3.0, 4.0);
        assert_eq!((delta_x, delta_y), (6.0, 8.0));
    }
}
