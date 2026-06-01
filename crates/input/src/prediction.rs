//! Predictive cursor movement.
//!
//! Prediction reduces perceived latency by extrapolating the latest absolute
//! cursor velocity over a short horizon. It is intentionally conservative and
//! bounded; input injection still uses authoritative events, while render/UI
//! layers can use predicted positions for smoother feedback.

use std::time::{Duration, Instant};

/// One absolute cursor sample in normalized `[0, 1]` coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CursorSample {
    /// Horizontal position.
    pub x: f64,
    /// Vertical position.
    pub y: f64,
    /// Sample timestamp.
    pub at: Instant,
}

impl CursorSample {
    /// Construct a cursor sample, clamping coordinates to `[0, 1]`.
    #[must_use]
    pub fn new(x: f64, y: f64, at: Instant) -> Self {
        Self {
            x: x.clamp(0.0, 1.0),
            y: y.clamp(0.0, 1.0),
            at,
        }
    }
}

/// Bounded linear cursor predictor.
#[derive(Debug, Clone)]
pub struct PredictiveCursor {
    max_prediction: Duration,
    previous: Option<CursorSample>,
    latest: Option<CursorSample>,
}

impl PredictiveCursor {
    /// Create a predictor with a maximum extrapolation horizon.
    #[must_use]
    pub fn new(max_prediction: Duration) -> Self {
        Self {
            max_prediction,
            previous: None,
            latest: None,
        }
    }

    /// Low-latency LAN preset.
    #[must_use]
    pub fn lan_default() -> Self {
        Self::new(Duration::from_millis(16))
    }

    /// Push an authoritative cursor sample.
    pub fn push_sample(&mut self, sample: CursorSample) {
        if self.latest.is_some_and(|latest| sample.at >= latest.at) {
            self.previous = self.latest;
            self.latest = Some(sample);
        } else if self.latest.is_none() {
            self.latest = Some(sample);
        }
    }

    /// Predict position at `now`.
    #[must_use]
    pub fn predict(&self, now: Instant) -> Option<(f64, f64)> {
        let latest = self.latest?;
        let Some(previous) = self.previous else {
            return Some((latest.x, latest.y));
        };
        let sample_dt = latest
            .at
            .saturating_duration_since(previous.at)
            .as_secs_f64();
        if sample_dt <= f64::EPSILON {
            return Some((latest.x, latest.y));
        }
        let horizon = now
            .saturating_duration_since(latest.at)
            .min(self.max_prediction)
            .as_secs_f64();
        let vx = (latest.x - previous.x) / sample_dt;
        let vy = (latest.y - previous.y) / sample_dt;
        Some((
            (latest.x + vx * horizon).clamp(0.0, 1.0),
            (latest.y + vy * horizon).clamp(0.0, 1.0),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predicts_forward_with_bounded_horizon() {
        let t0 = Instant::now();
        let mut predictor = PredictiveCursor::new(Duration::from_millis(10));
        predictor.push_sample(CursorSample::new(0.0, 0.5, t0));
        predictor.push_sample(CursorSample::new(0.5, 0.5, t0 + Duration::from_millis(10)));
        let predicted = predictor.predict(t0 + Duration::from_millis(20)).unwrap();
        assert!((predicted.0 - 1.0).abs() < 1e-9);
        assert!((predicted.1 - 0.5).abs() < 1e-9);
    }

    #[test]
    fn single_sample_returns_authoritative_position() {
        let t0 = Instant::now();
        let mut predictor = PredictiveCursor::lan_default();
        predictor.push_sample(CursorSample::new(0.25, 0.75, t0));
        assert_eq!(
            predictor.predict(t0 + Duration::from_millis(8)),
            Some((0.25, 0.75))
        );
    }
}
