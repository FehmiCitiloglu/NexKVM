//! Smooth cursor interpolation ⚡
//!
//! Absolute pointer updates arrive over the network at a finite, jittery rate
//! (e.g. one packet every few milliseconds, with variance). Snapping the local
//! cursor to each received sample looks choppy. [`CursorInterpolator`] smooths
//! playback by easing from the cursor's current position toward each new target
//! over a short window, so motion reads as continuous even when packets bunch up
//! or arrive late.
//!
//! It is render-driven and clock-injected: the consumer calls
//! [`sample`](CursorInterpolator::sample) each frame with the current time and
//! gets the position to draw. This decouples network cadence from display
//! cadence and adds at most one interpolation-window of latency — which is why
//! the gaming/raw path disables it (see [`crate::InputProfile`]).

use std::time::{Duration, Instant};

/// Eases the rendered cursor toward incoming absolute targets.
#[derive(Debug, Clone)]
pub struct CursorInterpolator {
    /// How long to ease from one sample to the next.
    window: Duration,
    from: (f64, f64),
    to: (f64, f64),
    start: Option<Instant>,
}

impl CursorInterpolator {
    /// Create an interpolator easing over `window`. A window roughly equal to
    /// the inter-packet interval gives smooth motion with minimal added latency.
    #[must_use]
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            from: (0.0, 0.0),
            to: (0.0, 0.0),
            start: None,
        }
    }

    /// Sensible default easing window for LAN play (~8 ms).
    #[must_use]
    pub fn lan_default() -> Self {
        Self::new(Duration::from_millis(8))
    }

    /// Record a newly received absolute target (normalized `[0,1]`) at `now`.
    ///
    /// Easing restarts from wherever the cursor is *currently* interpolated to,
    /// so bursts of samples chain smoothly rather than snapping.
    pub fn push_target(&mut self, x: f64, y: f64, now: Instant) {
        self.from = self.sample(now);
        self.to = (x, y);
        self.start = Some(now);
    }

    /// The interpolated position to render at `now`.
    ///
    /// Before any target is pushed this returns the origin; once easing
    /// completes it holds at the latest target.
    #[must_use]
    pub fn sample(&self, now: Instant) -> (f64, f64) {
        let Some(start) = self.start else {
            return self.to;
        };
        if self.window.is_zero() {
            return self.to;
        }
        let elapsed = now.saturating_duration_since(start).as_secs_f64();
        let t = (elapsed / self.window.as_secs_f64()).clamp(0.0, 1.0);
        (
            lerp(self.from.0, self.to.0, t),
            lerp(self.from.1, self.to.1, t),
        )
    }

    /// Whether easing toward the latest target has finished at `now`.
    #[must_use]
    pub fn is_settled(&self, now: Instant) -> bool {
        match self.start {
            None => true,
            Some(start) => now.saturating_duration_since(start) >= self.window,
        }
    }
}

fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_origin_before_any_target() {
        let interp = CursorInterpolator::new(Duration::from_millis(10));
        assert_eq!(interp.sample(Instant::now()), (0.0, 0.0));
    }

    #[test]
    fn eases_from_start_to_target() {
        let mut interp = CursorInterpolator::new(Duration::from_millis(10));
        let t0 = Instant::now();
        interp.push_target(1.0, 0.5, t0);

        // Halfway through the window → halfway to the target.
        let mid = interp.sample(t0 + Duration::from_millis(5));
        assert!((mid.0 - 0.5).abs() < 1e-3, "x mid: {}", mid.0);
        assert!((mid.1 - 0.25).abs() < 1e-3, "y mid: {}", mid.1);

        // At/after the window → exactly the target.
        let done = interp.sample(t0 + Duration::from_millis(10));
        assert!((done.0 - 1.0).abs() < 1e-9);
        assert!((done.1 - 0.5).abs() < 1e-9);
    }

    #[test]
    fn chained_targets_continue_smoothly() {
        let mut interp = CursorInterpolator::new(Duration::from_millis(10));
        let t0 = Instant::now();
        interp.push_target(1.0, 0.0, t0);
        // Push a second target mid-ease; easing should start from the current
        // interpolated position (~0.5), not snap back to 0.
        let t1 = t0 + Duration::from_millis(5);
        interp.push_target(0.0, 0.0, t1);
        let just_after = interp.sample(t1);
        assert!(
            just_after.0 > 0.4 && just_after.0 < 0.6,
            "x: {}",
            just_after.0
        );
    }

    #[test]
    fn zero_window_snaps_immediately() {
        let mut interp = CursorInterpolator::new(Duration::ZERO);
        let t0 = Instant::now();
        interp.push_target(0.7, 0.3, t0);
        assert_eq!(interp.sample(t0), (0.7, 0.3));
        assert!(interp.is_settled(t0));
    }
}
