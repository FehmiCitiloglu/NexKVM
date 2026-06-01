//! Motion event coalescing (event batching).
//!
//! At high poll rates the pointer can emit hundreds of motion events per second.
//! Forwarding each as its own packet wastes bandwidth and CPU. [`InputCoalescer`]
//! merges *consecutive* motion events while preserving the ordering of
//! non-motion events (clicks, keys, scrolls), which must not be reordered
//! relative to motion (a click belongs at a specific cursor position).
//!
//! Coalescing rules:
//! - Absolute [`PointerMove`](crate::InputEvent::PointerMove): latest wins.
//! - Relative [`RelativeMove`](crate::InputEvent::RelativeMove): deltas summed.
//! - Raw [`RawMotion`](crate::InputEvent::RawMotion): deltas summed (saturating).
//! - Any non-motion event flushes pending motion first, then is appended.
//!
//! This is the batching half of the latency/bandwidth tradeoff; the gaming
//! profile disables it (see [`crate::InputProfile`]). It is pure and sans-IO.

use std::collections::VecDeque;

use crate::InputEvent;

/// Merges consecutive pointer-motion events, preserving event order otherwise.
#[derive(Debug, Default)]
pub struct InputCoalescer {
    out: VecDeque<InputEvent>,
    last_abs: Option<(f64, f64)>,
    rel: Option<(f64, f64)>,
    raw: Option<(i32, i32)>,
}

impl InputCoalescer {
    /// Create an empty coalescer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one event into the coalescer.
    pub fn push(&mut self, event: InputEvent) {
        match event {
            InputEvent::PointerMove { x, y } => self.last_abs = Some((x, y)),
            InputEvent::RelativeMove { dx, dy } => {
                let (ax, ay) = self.rel.unwrap_or((0.0, 0.0));
                self.rel = Some((ax + dx, ay + dy));
            }
            InputEvent::RawMotion { dx, dy } => {
                let (ax, ay) = self.raw.unwrap_or((0, 0));
                self.raw = Some((ax.saturating_add(dx), ay.saturating_add(dy)));
            }
            // Non-motion: flush accumulated motion, then preserve this event.
            other => {
                self.flush_motion();
                self.out.push_back(other);
            }
        }
    }

    /// Push any accumulated motion into the output queue, in a stable order
    /// (absolute, then relative, then raw — at most one of each per flush).
    fn flush_motion(&mut self) {
        if let Some((x, y)) = self.last_abs.take() {
            self.out.push_back(InputEvent::PointerMove { x, y });
        }
        if let Some((dx, dy)) = self.rel.take() {
            self.out.push_back(InputEvent::RelativeMove { dx, dy });
        }
        if let Some((dx, dy)) = self.raw.take() {
            self.out.push_back(InputEvent::RawMotion { dx, dy });
        }
    }

    /// Drain all ready events, flushing any pending coalesced motion last.
    ///
    /// Returns events in send order: queued non-motion events interleaved with
    /// the coalesced motion that preceded them, followed by trailing motion.
    pub fn drain(&mut self) -> Vec<InputEvent> {
        self.flush_motion();
        self.out.drain(..).collect()
    }

    /// Whether nothing is buffered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.out.is_empty() && self.last_abs.is_none() && self.rel.is_none() && self.raw.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MouseButton;

    #[test]
    fn merges_consecutive_relative_motion() {
        let mut c = InputCoalescer::new();
        c.push(InputEvent::RelativeMove { dx: 1.0, dy: 2.0 });
        c.push(InputEvent::RelativeMove { dx: 3.0, dy: -1.0 });
        let out = c.drain();
        assert_eq!(out, vec![InputEvent::RelativeMove { dx: 4.0, dy: 1.0 }]);
    }

    #[test]
    fn absolute_motion_keeps_latest() {
        let mut c = InputCoalescer::new();
        c.push(InputEvent::PointerMove { x: 0.1, y: 0.1 });
        c.push(InputEvent::PointerMove { x: 0.9, y: 0.8 });
        assert_eq!(c.drain(), vec![InputEvent::PointerMove { x: 0.9, y: 0.8 }]);
    }

    #[test]
    fn raw_motion_sums() {
        let mut c = InputCoalescer::new();
        c.push(InputEvent::RawMotion { dx: 5, dy: -3 });
        c.push(InputEvent::RawMotion { dx: 2, dy: 1 });
        assert_eq!(c.drain(), vec![InputEvent::RawMotion { dx: 7, dy: -2 }]);
    }

    #[test]
    fn click_flushes_motion_and_preserves_order() {
        let mut c = InputCoalescer::new();
        c.push(InputEvent::RelativeMove { dx: 1.0, dy: 0.0 });
        c.push(InputEvent::RelativeMove { dx: 1.0, dy: 0.0 });
        c.push(InputEvent::ButtonPress(MouseButton::Left));
        c.push(InputEvent::RelativeMove { dx: 5.0, dy: 0.0 });
        let out = c.drain();
        assert_eq!(
            out,
            vec![
                InputEvent::RelativeMove { dx: 2.0, dy: 0.0 },
                InputEvent::ButtonPress(MouseButton::Left),
                InputEvent::RelativeMove { dx: 5.0, dy: 0.0 },
            ]
        );
    }

    #[test]
    fn empty_after_drain() {
        let mut c = InputCoalescer::new();
        c.push(InputEvent::KeyPress(42));
        let _ = c.drain();
        assert!(c.is_empty());
    }
}
