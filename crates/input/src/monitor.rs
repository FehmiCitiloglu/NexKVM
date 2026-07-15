//! Multi-monitor virtual-desktop geometry and coordinate mapping.
//!
//! A device's desktop is a set of monitor rectangles placed in a single global
//! pixel space (which may have negative origins, as on Windows/macOS virtual
//! screens). This module models that layout and converts between:
//!
//! - **global pixels** — a point in the local virtual desktop, and
//! - **normalized `[0,1]` coordinates** — the resolution-independent form that
//!   travels on the wire (see [`crate::InputEvent::PointerMove`]).
//!
//! Normalization is against the layout's *bounding box*, so a sender's absolute
//! cursor maps onto a receiver of any size/DPI without either side knowing the
//! other's geometry. Per-monitor lookup ([`MonitorLayout::monitor_at`]) supports
//! edge/boundary logic for cursor hand-off.
//!
//! This module is pure geometry: no OS calls. The `platform-*` crates enumerate
//! real monitors and build a [`MonitorLayout`]; everything here is testable.

use serde::{Deserialize, Serialize};

/// Stable identifier for a monitor within a layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MonitorId(pub u32);

/// A monitor's rectangle in global virtual-desktop pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayRect {
    /// Left edge (global x), may be negative.
    pub x: i32,
    /// Top edge (global y), may be negative.
    pub y: i32,
    /// Width in pixels (must be non-zero for a usable monitor).
    pub width: u32,
    /// Height in pixels (must be non-zero for a usable monitor).
    pub height: u32,
}

impl DisplayRect {
    /// Construct a rectangle.
    #[must_use]
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Left edge (inclusive).
    #[must_use]
    pub const fn left(&self) -> i32 {
        self.x
    }

    /// Top edge (inclusive).
    #[must_use]
    pub const fn top(&self) -> i32 {
        self.y
    }

    /// Right edge (exclusive).
    #[must_use]
    pub const fn right(&self) -> i32 {
        self.x + self.width as i32
    }

    /// Bottom edge (exclusive).
    #[must_use]
    pub const fn bottom(&self) -> i32 {
        self.y + self.height as i32
    }

    /// Whether `(px, py)` lies within this rectangle (right/bottom exclusive).
    #[must_use]
    pub const fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.left() && px < self.right() && py >= self.top() && py < self.bottom()
    }
}

/// An ordered set of monitors forming one virtual desktop.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorLayout {
    monitors: Vec<(MonitorId, DisplayRect)>,
}

impl MonitorLayout {
    /// Build a layout from `(id, rect)` pairs.
    #[must_use]
    pub fn new(monitors: Vec<(MonitorId, DisplayRect)>) -> Self {
        Self { monitors }
    }

    /// The monitors in this layout.
    #[must_use]
    pub fn monitors(&self) -> &[(MonitorId, DisplayRect)] {
        &self.monitors
    }

    /// Whether the layout has no monitors.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.monitors.is_empty()
    }

    /// The smallest rectangle enclosing every monitor, or `None` if empty.
    #[must_use]
    pub fn bounding_box(&self) -> Option<DisplayRect> {
        let mut iter = self.monitors.iter().map(|(_, r)| *r);
        let first = iter.next()?;
        let mut min_x = first.left();
        let mut min_y = first.top();
        let mut max_x = first.right();
        let mut max_y = first.bottom();
        for r in iter {
            min_x = min_x.min(r.left());
            min_y = min_y.min(r.top());
            max_x = max_x.max(r.right());
            max_y = max_y.max(r.bottom());
        }
        Some(DisplayRect::new(
            min_x,
            min_y,
            (max_x - min_x) as u32,
            (max_y - min_y) as u32,
        ))
    }

    /// The id of the monitor containing `(px, py)`, if any.
    #[must_use]
    pub fn monitor_at(&self, px: i32, py: i32) -> Option<MonitorId> {
        self.monitors
            .iter()
            .find(|(_, r)| r.contains(px, py))
            .map(|(id, _)| *id)
    }

    /// Map a global pixel point to normalized `[0,1]` coords against the
    /// bounding box. Returns `None` for an empty (zero-area) layout.
    #[must_use]
    pub fn normalize(&self, px: i32, py: i32) -> Option<(f64, f64)> {
        let bb = self.bounding_box()?;
        if bb.width == 0 || bb.height == 0 {
            return None;
        }
        let nx = (px - bb.left()) as f64 / bb.width as f64;
        let ny = (py - bb.top()) as f64 / bb.height as f64;
        Some((nx.clamp(0.0, 1.0), ny.clamp(0.0, 1.0)))
    }

    /// Map normalized `[0,1]` coords back to a global pixel point against the
    /// bounding box. Returns `None` for an empty or zero-area layout.
    #[must_use]
    pub fn denormalize(&self, nx: f64, ny: f64) -> Option<(i32, i32)> {
        let bb = self.bounding_box()?;
        if bb.width == 0 || bb.height == 0 {
            return None;
        }
        let nx = nx.clamp(0.0, 1.0);
        let ny = ny.clamp(0.0, 1.0);
        let px =
            (bb.left() + (nx * bb.width as f64).round() as i32).min(bb.right().saturating_sub(1));
        let py =
            (bb.top() + (ny * bb.height as f64).round() as i32).min(bb.bottom().saturating_sub(1));
        Some((px, py))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two 1920x1080 monitors side by side, the left one at the origin.
    fn dual() -> MonitorLayout {
        MonitorLayout::new(vec![
            (MonitorId(0), DisplayRect::new(0, 0, 1920, 1080)),
            (MonitorId(1), DisplayRect::new(1920, 0, 1920, 1080)),
        ])
    }

    #[test]
    fn bounding_box_spans_all_monitors() {
        let bb = dual().bounding_box().unwrap();
        assert_eq!(bb, DisplayRect::new(0, 0, 3840, 1080));
    }

    #[test]
    fn bounding_box_handles_negative_origins() {
        let layout = MonitorLayout::new(vec![
            (MonitorId(0), DisplayRect::new(-1920, 0, 1920, 1080)),
            (MonitorId(1), DisplayRect::new(0, 0, 1920, 1080)),
        ]);
        let bb = layout.bounding_box().unwrap();
        assert_eq!(bb, DisplayRect::new(-1920, 0, 3840, 1080));
    }

    #[test]
    fn monitor_at_selects_correct_screen() {
        let layout = dual();
        assert_eq!(layout.monitor_at(100, 100), Some(MonitorId(0)));
        assert_eq!(layout.monitor_at(2000, 100), Some(MonitorId(1)));
        assert_eq!(layout.monitor_at(5000, 100), None);
    }

    #[test]
    fn normalize_denormalize_round_trips_center() {
        let layout = dual();
        // Center of the right monitor: global x = 2880, y = 540.
        let (nx, ny) = layout.normalize(2880, 540).unwrap();
        assert!((nx - 0.75).abs() < 1e-9);
        assert!((ny - 0.5).abs() < 1e-9);
        let (px, py) = layout.denormalize(nx, ny).unwrap();
        assert_eq!((px, py), (2880, 540));
    }

    #[test]
    fn denormalize_maximum_stays_inside_virtual_desktop() {
        let layout = MonitorLayout::new(vec![
            (MonitorId(0), DisplayRect::new(-1920, -100, 1920, 1080)),
            (MonitorId(1), DisplayRect::new(0, -100, 2560, 1440)),
        ]);

        let bounds = layout.bounding_box().unwrap();
        let (px, py) = layout.denormalize(1.0, 1.0).unwrap();

        assert_eq!(px, bounds.right() - 1);
        assert_eq!(py, bounds.bottom() - 1);
        assert!(bounds.contains(px, py));
    }

    #[test]
    fn empty_layout_has_no_bounds() {
        let layout = MonitorLayout::default();
        assert!(layout.is_empty());
        assert!(layout.bounding_box().is_none());
        assert!(layout.normalize(0, 0).is_none());
        assert!(layout.denormalize(0.0, 0.0).is_none());
    }

    #[test]
    fn zero_area_layout_cannot_denormalize() {
        let layout = MonitorLayout::new(vec![(MonitorId(0), DisplayRect::new(10, 20, 0, 1080))]);

        assert!(layout.denormalize(0.5, 0.5).is_none());
    }
}
