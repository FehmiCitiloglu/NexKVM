//! Cursor Transition Engine ⭐🔥
//!
//! When the cursor leaves one device and appears on another, three things have
//! to feel right or the handoff reads as broken:
//!
//! - **jitter filtering** — raw pointer samples carry sensor/quantization noise;
//!   feeding that straight into velocity estimation makes throw detection twitchy
//!   and the rendered cursor shimmer. [`JitterFilter`] removes sub-pixel jitter
//!   with a per-axis deadband plus an exponential low-pass.
//! - **velocity-based movement** — a hard flick should feel snappier than a slow
//!   drag. [`CursorTransitionPolicy`] maps entry velocity to an animation
//!   duration so faster throws settle faster.
//! - **smooth animation between devices** — the cursor shouldn't teleport onto
//!   the receiver; [`CursorTransition`] eases from the seam to the landing point
//!   with a smoothstep curve.
//!
//! The existing [`CursorThrowPlanner`](crate::CursorThrowPlanner) /
//! [`MomentumTransfer`](crate::MomentumTransfer) decide *whether* and *where* a
//! throw happens; this engine animates the *visual crossing* afterward. It is
//! distinct from [`CursorInterpolator`](crate::CursorInterpolator), which eases
//! per-packet network playback rather than a one-shot device-to-device hop.
//!
//! Everything here is pure and clock-injected: callers pass `now` and read the
//! position to render, so it is fully testable without an OS or wall clock.

use std::time::{Duration, Instant};

/// Per-axis deadband + exponential low-pass over normalized cursor coordinates.
///
/// Coordinates are normalized `[0,1]`. The deadband suppresses micro-movements
/// below a threshold (pure jitter), and the smoothing factor eases through the
/// movements that survive it.
#[derive(Debug, Clone)]
pub struct JitterFilter {
    deadband: f64,
    smoothing: f64,
    last: Option<(f64, f64)>,
}

impl JitterFilter {
    /// Create a filter.
    ///
    /// `deadband` is the per-axis normalized threshold below which a change is
    /// treated as noise and dropped. `smoothing` is the EMA factor in `(0,1]`:
    /// `1.0` passes movement through instantly, smaller values smooth harder.
    /// Both inputs are clamped to sane ranges.
    #[must_use]
    pub fn new(deadband: f64, smoothing: f64) -> Self {
        Self {
            deadband: deadband.clamp(0.0, 0.5),
            smoothing: clamp_smoothing(smoothing),
            last: None,
        }
    }

    /// Sensible LAN default: ~0.1% deadband with light smoothing.
    #[must_use]
    pub fn lan_default() -> Self {
        Self::new(0.001, 0.5)
    }

    /// Filter one raw normalized sample, returning the smoothed position.
    ///
    /// The first sample passes through unchanged to seed the filter.
    pub fn filter(&mut self, x: f64, y: f64) -> (f64, f64) {
        let Some((lx, ly)) = self.last else {
            let seeded = (x, y);
            self.last = Some(seeded);
            return seeded;
        };
        let nx = self.filter_axis(lx, x);
        let ny = self.filter_axis(ly, y);
        self.last = Some((nx, ny));
        (nx, ny)
    }

    /// Reset the filter so the next sample re-seeds it (e.g. after a handoff).
    pub fn reset(&mut self) {
        self.last = None;
    }

    fn filter_axis(&self, last: f64, raw: f64) -> f64 {
        if (raw - last).abs() <= self.deadband {
            // Below the noise floor: hold the previous value.
            last
        } else {
            last + self.smoothing * (raw - last)
        }
    }
}

fn clamp_smoothing(value: f64) -> f64 {
    if value.is_finite() {
        value.clamp(f64::EPSILON, 1.0)
    } else {
        1.0
    }
}

/// Maps throw/entry velocity to a transition animation duration.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CursorTransitionPolicy {
    /// Duration for a slow crossing (low velocity).
    pub slow_duration: Duration,
    /// Duration for a fast flick (at or above `reference_velocity_px_s`).
    pub fast_duration: Duration,
    /// Velocity (px/s) at which the animation reaches `fast_duration`.
    pub reference_velocity_px_s: f64,
}

impl CursorTransitionPolicy {
    /// Snappy LAN default: 60 ms slow, 16 ms fast at ~2000 px/s.
    #[must_use]
    pub const fn lan_default() -> Self {
        Self {
            slow_duration: Duration::from_millis(60),
            fast_duration: Duration::from_millis(16),
            reference_velocity_px_s: 2_000.0,
        }
    }

    /// Animation duration for an entry `velocity_px_s` (sign ignored).
    ///
    /// Faster throws settle faster, interpolating linearly between
    /// `slow_duration` and `fast_duration`.
    #[must_use]
    pub fn duration_for(&self, velocity_px_s: f64) -> Duration {
        let reference = self.reference_velocity_px_s.max(1.0);
        let frac = (velocity_px_s.abs() / reference).clamp(0.0, 1.0);
        let slow = self.slow_duration.as_secs_f64();
        let fast = self.fast_duration.as_secs_f64();
        // frac == 1 → fast, frac == 0 → slow.
        Duration::from_secs_f64(slow + (fast - slow) * frac)
    }
}

impl Default for CursorTransitionPolicy {
    fn default() -> Self {
        Self::lan_default()
    }
}

/// A one-shot eased animation of the cursor crossing to another device.
///
/// Coordinates are normalized `[0,1]` on the destination surface.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CursorTransition {
    from: (f64, f64),
    to: (f64, f64),
    start: Instant,
    duration: Duration,
}

impl CursorTransition {
    /// Begin a transition from `from` to `to` over `duration` starting at `now`.
    #[must_use]
    pub fn new(from: (f64, f64), to: (f64, f64), duration: Duration, now: Instant) -> Self {
        Self {
            from,
            to,
            start: now,
            duration,
        }
    }

    /// Eased progress in `[0,1]` (smoothstep) at `now`.
    #[must_use]
    pub fn progress(&self, now: Instant) -> f64 {
        if self.duration.is_zero() {
            return 1.0;
        }
        let elapsed = now.saturating_duration_since(self.start).as_secs_f64();
        let t = (elapsed / self.duration.as_secs_f64()).clamp(0.0, 1.0);
        smoothstep(t)
    }

    /// Rendered position at `now`.
    #[must_use]
    pub fn position(&self, now: Instant) -> (f64, f64) {
        let p = self.progress(now);
        (
            lerp(self.from.0, self.to.0, p),
            lerp(self.from.1, self.to.1, p),
        )
    }

    /// Whether the animation has finished at `now`.
    #[must_use]
    pub fn is_complete(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.start) >= self.duration
    }
}

/// Orchestrates jitter filtering and the active device-crossing animation.
///
/// Typical use: feed every raw local sample through [`observe`](Self::observe)
/// for a denoised position, and when a throw is committed call
/// [`begin`](Self::begin) to start the visual hop, then [`sample`](Self::sample)
/// each render frame.
#[derive(Debug, Clone)]
pub struct CursorTransitionEngine {
    filter: JitterFilter,
    policy: CursorTransitionPolicy,
    active: Option<CursorTransition>,
}

impl CursorTransitionEngine {
    /// Create an engine from a jitter filter and a velocity→duration policy.
    #[must_use]
    pub fn new(filter: JitterFilter, policy: CursorTransitionPolicy) -> Self {
        Self {
            filter,
            policy,
            active: None,
        }
    }

    /// LAN-tuned engine.
    #[must_use]
    pub fn lan_default() -> Self {
        Self::new(
            JitterFilter::lan_default(),
            CursorTransitionPolicy::default(),
        )
    }

    /// Filter a raw normalized sample, returning the denoised position.
    pub fn observe(&mut self, x: f64, y: f64) -> (f64, f64) {
        self.filter.filter(x, y)
    }

    /// Begin an animated crossing from `from` to `to` at the given entry
    /// velocity. The animation duration is derived from `velocity_px_s`.
    ///
    /// The jitter filter is reset so post-handoff samples re-seed cleanly.
    pub fn begin(&mut self, from: (f64, f64), to: (f64, f64), velocity_px_s: f64, now: Instant) {
        let duration = self.policy.duration_for(velocity_px_s);
        self.active = Some(CursorTransition::new(from, to, duration, now));
        self.filter.reset();
    }

    /// Sample the animated position at `now`, or `None` if idle.
    ///
    /// Clears the active transition once it completes so the next frame is idle.
    pub fn sample(&mut self, now: Instant) -> Option<(f64, f64)> {
        let transition = self.active.as_ref()?;
        let position = transition.position(now);
        if transition.is_complete(now) {
            self.active = None;
        }
        Some(position)
    }

    /// Whether an animation is in progress at `now`.
    #[must_use]
    pub fn is_animating(&self, now: Instant) -> bool {
        self.active.as_ref().is_some_and(|t| !t.is_complete(now))
    }
}

fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

fn smoothstep(t: f64) -> f64 {
    t * t * (3.0 - 2.0 * t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_sample_seeds_filter_unchanged() {
        let mut f = JitterFilter::new(0.01, 0.5);
        assert_eq!(f.filter(0.3, 0.7), (0.3, 0.7));
    }

    #[test]
    fn deadband_drops_subthreshold_jitter() {
        let mut f = JitterFilter::new(0.01, 1.0);
        f.filter(0.5, 0.5);
        // Movement smaller than the deadband is suppressed entirely.
        assert_eq!(f.filter(0.505, 0.498), (0.5, 0.5));
    }

    #[test]
    fn smoothing_eases_large_moves() {
        let mut f = JitterFilter::new(0.0, 0.5);
        f.filter(0.0, 0.0);
        // Half-way EMA toward the new target.
        let (x, y) = f.filter(1.0, 1.0);
        assert!((x - 0.5).abs() < 1e-9);
        assert!((y - 0.5).abs() < 1e-9);
    }

    #[test]
    fn reset_reseeds_next_sample() {
        let mut f = JitterFilter::new(0.0, 0.5);
        f.filter(0.0, 0.0);
        f.reset();
        assert_eq!(f.filter(0.9, 0.1), (0.9, 0.1));
    }

    #[test]
    fn faster_throw_yields_shorter_animation() {
        let policy = CursorTransitionPolicy::lan_default();
        let slow = policy.duration_for(100.0);
        let fast = policy.duration_for(5_000.0);
        assert!(fast < slow);
        // At/above reference velocity we hit the fast bound exactly.
        assert_eq!(policy.duration_for(2_000.0), policy.fast_duration);
        // Near-zero velocity hits the slow bound.
        assert_eq!(policy.duration_for(0.0), policy.slow_duration);
    }

    #[test]
    fn transition_eases_from_start_to_end() {
        let t0 = Instant::now();
        let tr = CursorTransition::new((0.0, 0.0), (1.0, 1.0), Duration::from_millis(20), t0);
        assert_eq!(tr.position(t0), (0.0, 0.0));
        // Smoothstep at t=0.5 is exactly 0.5.
        let mid = tr.position(t0 + Duration::from_millis(10));
        assert!((mid.0 - 0.5).abs() < 1e-9);
        let end = tr.position(t0 + Duration::from_millis(20));
        assert_eq!(end, (1.0, 1.0));
        assert!(tr.is_complete(t0 + Duration::from_millis(20)));
    }

    #[test]
    fn engine_animates_then_goes_idle() {
        let mut engine = CursorTransitionEngine::lan_default();
        let t0 = Instant::now();
        engine.begin((0.0, 0.5), (1.0, 0.5), 5_000.0, t0);
        assert!(engine.is_animating(t0));
        let dur = CursorTransitionPolicy::lan_default().duration_for(5_000.0);

        // Mid-animation yields an interpolated position.
        let mid = engine.sample(t0 + dur / 2).unwrap();
        assert!(mid.0 > 0.0 && mid.0 < 1.0);

        // After completion the final frame lands on the target, then idles.
        let last = engine.sample(t0 + dur).unwrap();
        assert!((last.0 - 1.0).abs() < 1e-9);
        assert!(engine.sample(t0 + dur).is_none());
        assert!(!engine.is_animating(t0 + dur));
    }

    #[test]
    fn engine_observe_filters_samples() {
        let mut engine = CursorTransitionEngine::new(
            JitterFilter::new(0.05, 1.0),
            CursorTransitionPolicy::default(),
        );
        assert_eq!(engine.observe(0.4, 0.4), (0.4, 0.4));
        // Within deadband → held.
        assert_eq!(engine.observe(0.42, 0.39), (0.4, 0.4));
    }
}
