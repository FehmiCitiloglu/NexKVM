//! DPI / resolution-scaling awareness on top of the pixel-only [`MonitorLayout`].
//!
//! [`MonitorLayout`] models monitor rectangles in *physical* global pixels, which
//! is enough for normalizing absolute cursor position across machines. It is not
//! enough once monitors on the **same** desktop have different scale factors —
//! a 4K monitor at 200% (Retina/HiDPI) sits beside a 1080p monitor at 100%, and
//! a logical point on one is a different number of physical pixels than on the
//! other. Cursor hand-off and click placement must account for that or the
//! pointer lands in the wrong spot when crossing a DPI boundary.
//!
//! [`ScaledLayout`] augments a layout with a per-monitor [`MonitorScale`] and
//! adds three things the platform/handoff layers need:
//!
//! - **logical sizing** — physical px ÷ scale, the size apps actually see;
//! - **per-screen coordinate mapping** — a global physical point resolved to the
//!   monitor it lands on plus coordinates *normalized within that monitor*
//!   ([`local_at`]); and
//! - **DPI conversions** — physical↔logical within a monitor ([`to_logical`],
//!   [`from_logical`]).
//!
//! Like [`MonitorLayout`], this is pure geometry: the `platform-*` crates supply
//! the real scale factors (`NSScreen.backingScaleFactor`, `GetDpiForMonitor`,
//! `wl_output` scale) and everything here is testable without an OS.
//!
//! [`local_at`]: ScaledLayout::local_at
//! [`to_logical`]: ScaledLayout::to_logical
//! [`from_logical`]: ScaledLayout::from_logical

use serde::{Deserialize, Serialize};

use crate::monitor::{DisplayRect, MonitorId, MonitorLayout};

/// A monitor's DPI scale factor: physical pixels per logical pixel.
///
/// `1.0` is a standard-DPI display; `2.0` is a typical Retina/200% display;
/// `1.5` is Windows 150%. The factor is always strictly positive and finite.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MonitorScale(f64);

impl MonitorScale {
    /// Construct a scale factor, clamping to a sane strictly-positive range.
    ///
    /// Non-finite or non-positive inputs fall back to `1.0` so a hostile or
    /// buggy peer can never produce a zero/NaN divisor downstream.
    #[must_use]
    pub fn new(factor: f64) -> Self {
        if factor.is_finite() && factor > 0.0 {
            Self(factor.clamp(0.1, 16.0))
        } else {
            Self(1.0)
        }
    }

    /// Standard-DPI (`1.0`) scale.
    #[must_use]
    pub const fn identity() -> Self {
        Self(1.0)
    }

    /// The underlying factor.
    #[must_use]
    pub const fn factor(self) -> f64 {
        self.0
    }
}

impl Default for MonitorScale {
    fn default() -> Self {
        Self::identity()
    }
}

/// A logical-pixel size (physical size ÷ scale).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LogicalSize {
    /// Logical width.
    pub width: f64,
    /// Logical height.
    pub height: f64,
}

/// A point normalized within a single monitor, paired with that monitor's id.
///
/// `nx`/`ny` are in `[0,1]` relative to the monitor's own rectangle (not the
/// whole desktop), which is what a receiver needs to place the cursor on the
/// corresponding monitor regardless of resolution or DPI.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LocalPoint {
    /// Monitor the point falls on.
    pub monitor: MonitorId,
    /// Horizontal position within the monitor, `[0,1]`.
    pub nx: f64,
    /// Vertical position within the monitor, `[0,1]`.
    pub ny: f64,
}

/// A [`MonitorLayout`] enriched with per-monitor DPI scale factors.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ScaledLayout {
    layout: MonitorLayout,
    scales: Vec<(MonitorId, MonitorScale)>,
}

impl ScaledLayout {
    /// Build a scaled layout from a pixel `layout` and `(id, scale)` pairs.
    ///
    /// Monitors without an explicit scale default to [`MonitorScale::identity`].
    #[must_use]
    pub fn new(layout: MonitorLayout, scales: Vec<(MonitorId, MonitorScale)>) -> Self {
        Self { layout, scales }
    }

    /// Build a uniform-scale layout (every monitor shares `scale`).
    #[must_use]
    pub fn uniform(layout: MonitorLayout, scale: MonitorScale) -> Self {
        let scales = layout
            .monitors()
            .iter()
            .map(|(id, _)| (*id, scale))
            .collect();
        Self { layout, scales }
    }

    /// The underlying pixel layout.
    #[must_use]
    pub fn layout(&self) -> &MonitorLayout {
        &self.layout
    }

    /// The scale factor for `monitor` (identity if unknown).
    #[must_use]
    pub fn scale_of(&self, monitor: MonitorId) -> MonitorScale {
        self.scales
            .iter()
            .find(|(id, _)| *id == monitor)
            .map_or_else(MonitorScale::identity, |(_, s)| *s)
    }

    /// The physical rectangle for `monitor`, if present.
    #[must_use]
    pub fn rect_of(&self, monitor: MonitorId) -> Option<DisplayRect> {
        self.layout
            .monitors()
            .iter()
            .find(|(id, _)| *id == monitor)
            .map(|(_, r)| *r)
    }

    /// The logical (scale-adjusted) size apps see on `monitor`.
    #[must_use]
    pub fn logical_size(&self, monitor: MonitorId) -> Option<LogicalSize> {
        let rect = self.rect_of(monitor)?;
        let s = self.scale_of(monitor).factor();
        Some(LogicalSize {
            width: f64::from(rect.width) / s,
            height: f64::from(rect.height) / s,
        })
    }

    /// **Per-screen coordinate mapping**: resolve a global physical point to the
    /// monitor it lands on plus coordinates normalized within that monitor.
    ///
    /// Returns `None` if the point lies in a gap between monitors.
    #[must_use]
    pub fn local_at(&self, px: i32, py: i32) -> Option<LocalPoint> {
        let monitor = self.layout.monitor_at(px, py)?;
        let rect = self.rect_of(monitor)?;
        // width/height are non-zero for a usable monitor; guard anyway.
        if rect.width == 0 || rect.height == 0 {
            return None;
        }
        let nx = f64::from(px - rect.left()) / f64::from(rect.width);
        let ny = f64::from(py - rect.top()) / f64::from(rect.height);
        Some(LocalPoint {
            monitor,
            nx: nx.clamp(0.0, 1.0),
            ny: ny.clamp(0.0, 1.0),
        })
    }

    /// Map a within-monitor normalized point back to a global physical point.
    ///
    /// Inverse of [`local_at`](Self::local_at). Returns `None` for an unknown
    /// monitor.
    #[must_use]
    pub fn global_from_local(&self, local: LocalPoint) -> Option<(i32, i32)> {
        let rect = self.rect_of(local.monitor)?;
        let nx = local.nx.clamp(0.0, 1.0);
        let ny = local.ny.clamp(0.0, 1.0);
        let px = rect.left() + (nx * f64::from(rect.width)).round() as i32;
        let py = rect.top() + (ny * f64::from(rect.height)).round() as i32;
        Some((px, py))
    }

    /// Convert a global physical point to logical coordinates *within* its
    /// monitor (physical ÷ scale, relative to the monitor's top-left).
    ///
    /// Returns `None` if the point is not on any monitor.
    #[must_use]
    pub fn to_logical(&self, px: i32, py: i32) -> Option<(MonitorId, f64, f64)> {
        let monitor = self.layout.monitor_at(px, py)?;
        let rect = self.rect_of(monitor)?;
        let s = self.scale_of(monitor).factor();
        let lx = f64::from(px - rect.left()) / s;
        let ly = f64::from(py - rect.top()) / s;
        Some((monitor, lx, ly))
    }

    /// Convert a logical point within `monitor` back to a global physical point
    /// (logical × scale, offset by the monitor's top-left).
    ///
    /// Returns `None` for an unknown monitor.
    #[must_use]
    pub fn from_logical(&self, monitor: MonitorId, lx: f64, ly: f64) -> Option<(i32, i32)> {
        let rect = self.rect_of(monitor)?;
        let s = self.scale_of(monitor).factor();
        let px = rect.left() + (lx * s).round() as i32;
        let py = rect.top() + (ly * s).round() as i32;
        Some((px, py))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Left 1080p @100%, right 4K @200% (Retina) placed beside it.
    fn mixed_dpi() -> ScaledLayout {
        let layout = MonitorLayout::new(vec![
            (MonitorId(0), DisplayRect::new(0, 0, 1920, 1080)),
            (MonitorId(1), DisplayRect::new(1920, 0, 3840, 2160)),
        ]);
        ScaledLayout::new(
            layout,
            vec![
                (MonitorId(0), MonitorScale::new(1.0)),
                (MonitorId(1), MonitorScale::new(2.0)),
            ],
        )
    }

    #[test]
    fn scale_rejects_invalid_factors() {
        assert_eq!(MonitorScale::new(0.0).factor(), 1.0);
        assert_eq!(MonitorScale::new(-3.0).factor(), 1.0);
        assert_eq!(MonitorScale::new(f64::NAN).factor(), 1.0);
        assert_eq!(MonitorScale::new(2.0).factor(), 2.0);
    }

    #[test]
    fn logical_size_divides_by_scale() {
        let l = mixed_dpi();
        // 4K at 200% presents as 1920x1080 logical.
        assert_eq!(
            l.logical_size(MonitorId(1)).unwrap(),
            LogicalSize {
                width: 1920.0,
                height: 1080.0
            }
        );
        // 1080p at 100% is unchanged.
        assert_eq!(
            l.logical_size(MonitorId(0)).unwrap(),
            LogicalSize {
                width: 1920.0,
                height: 1080.0
            }
        );
    }

    #[test]
    fn local_at_maps_within_correct_monitor() {
        let l = mixed_dpi();
        // Center of the right 4K monitor: global (3840, 1080).
        let p = l.local_at(3840, 1080).unwrap();
        assert_eq!(p.monitor, MonitorId(1));
        assert!((p.nx - 0.5).abs() < 1e-9);
        assert!((p.ny - 0.5).abs() < 1e-9);
    }

    #[test]
    fn local_round_trips_to_global() {
        let l = mixed_dpi();
        let p = l.local_at(2400, 600).unwrap();
        let (px, py) = l.global_from_local(p).unwrap();
        assert_eq!((px, py), (2400, 600));
    }

    #[test]
    fn point_in_gap_has_no_monitor() {
        let l = ScaledLayout::new(
            MonitorLayout::new(vec![(MonitorId(0), DisplayRect::new(0, 0, 800, 600))]),
            vec![(MonitorId(0), MonitorScale::new(1.0))],
        );
        assert!(l.local_at(5000, 5000).is_none());
    }

    #[test]
    fn to_logical_applies_dpi_within_monitor() {
        let l = mixed_dpi();
        // 200 physical px into the 200% monitor from its left edge = 100 logical.
        let (mon, lx, ly) = l.to_logical(1920 + 200, 400).unwrap();
        assert_eq!(mon, MonitorId(1));
        assert!((lx - 100.0).abs() < 1e-9);
        assert!((ly - 200.0).abs() < 1e-9);
        // And back.
        assert_eq!(l.from_logical(mon, lx, ly).unwrap(), (1920 + 200, 400));
    }

    #[test]
    fn uniform_applies_one_scale_to_all() {
        let layout = MonitorLayout::new(vec![
            (MonitorId(0), DisplayRect::new(0, 0, 2560, 1440)),
            (MonitorId(1), DisplayRect::new(2560, 0, 2560, 1440)),
        ]);
        let l = ScaledLayout::uniform(layout, MonitorScale::new(1.5));
        assert_eq!(l.scale_of(MonitorId(0)).factor(), 1.5);
        assert_eq!(l.scale_of(MonitorId(1)).factor(), 1.5);
        // Unknown monitor falls back to identity.
        assert_eq!(l.scale_of(MonitorId(9)).factor(), 1.0);
    }
}
