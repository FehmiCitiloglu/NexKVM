//! Device topology editor, spatial desktop map, and monitor preview models.
//!
//! Advanced UX needs a single source of truth for where devices live relative
//! to each other. This module is that pure model: the desktop UI can edit it,
//! platform crates can refresh live monitor layouts, and the input boundary
//! logic can derive edge links from it.
//!
//! No OS calls happen here. macOS/Windows/Linux monitor enumeration stays in the
//! `platform-*` crates; Wayland may expose reduced monitor data through portals,
//! so callers should tolerate stale or partial live previews.

use coklu_core::identity::DeviceId;
use serde::{Deserialize, Serialize};

use crate::boundary::{Edge, EdgeLink};
use crate::monitor::{DisplayRect, MonitorId, MonitorLayout};

/// Device origin in the shared spatial desktop map.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesktopPoint {
    /// Horizontal map coordinate in virtual pixels.
    pub x: i32,
    /// Vertical map coordinate in virtual pixels.
    pub y: i32,
}

impl DesktopPoint {
    /// Construct a point.
    #[must_use]
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

/// Floating-point rectangle for scaled monitor previews.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PreviewRect {
    /// Left preview coordinate.
    pub x: f64,
    /// Top preview coordinate.
    pub y: f64,
    /// Preview width.
    pub width: f64,
    /// Preview height.
    pub height: f64,
}

/// One monitor as rendered in a live topology preview.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonitorPreview {
    /// Device owning this monitor.
    pub device: DeviceId,
    /// Monitor id within the device layout.
    pub monitor: MonitorId,
    /// Monitor rectangle in spatial-map coordinates.
    pub map_rect: DisplayRect,
    /// Scaled rectangle in preview/canvas coordinates.
    pub preview_rect: PreviewRect,
    /// Whether the owning device is currently online.
    pub online: bool,
}

/// A device placed in the spatial desktop map.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DevicePlacement {
    /// Device identifier.
    pub device: DeviceId,
    /// Human-readable device label for editor surfaces.
    pub label: String,
    /// Top-left origin of the device's virtual desktop in map coordinates.
    pub origin: DesktopPoint,
    /// Last known monitor layout for this device.
    pub layout: MonitorLayout,
    /// Whether monitor data is live from an active session.
    pub online: bool,
}

impl DevicePlacement {
    /// Construct a placement.
    #[must_use]
    pub fn new(
        device: DeviceId,
        label: impl Into<String>,
        origin: DesktopPoint,
        layout: MonitorLayout,
    ) -> Self {
        Self {
            device,
            label: label.into(),
            origin,
            layout,
            online: false,
        }
    }

    /// Mark whether this placement is backed by a currently connected device.
    #[must_use]
    pub fn with_online(mut self, online: bool) -> Self {
        self.online = online;
        self
    }

    /// Bounding box of this device in map coordinates.
    #[must_use]
    pub fn map_bounds(&self) -> Option<DisplayRect> {
        let bounds = self.layout.bounding_box()?;
        Some(offset_rect(bounds, self.origin))
    }

    /// Iterator of this device's monitors in map coordinates.
    pub fn map_monitors(&self) -> impl Iterator<Item = (MonitorId, DisplayRect)> + '_ {
        self.layout
            .monitors()
            .iter()
            .map(|(id, rect)| (*id, offset_rect(*rect, self.origin)))
    }
}

/// Complete editable map of devices and their monitor layouts.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SpatialDesktopMap {
    placements: Vec<DevicePlacement>,
}

impl SpatialDesktopMap {
    /// Create an empty map.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// All placements in editor order.
    #[must_use]
    pub fn placements(&self) -> &[DevicePlacement] {
        &self.placements
    }

    /// Get a placement by device id.
    #[must_use]
    pub fn placement(&self, device: DeviceId) -> Option<&DevicePlacement> {
        self.placements.iter().find(|p| p.device == device)
    }

    /// Add or replace a placement.
    pub fn upsert(&mut self, placement: DevicePlacement) {
        if let Some(existing) = self
            .placements
            .iter_mut()
            .find(|p| p.device == placement.device)
        {
            *existing = placement;
        } else {
            self.placements.push(placement);
        }
    }

    /// Move an existing device. Returns whether it existed.
    pub fn move_device(&mut self, device: DeviceId, origin: DesktopPoint) -> bool {
        if let Some(placement) = self.placements.iter_mut().find(|p| p.device == device) {
            placement.origin = origin;
            true
        } else {
            false
        }
    }

    /// Remove a device. Returns whether it existed.
    pub fn remove(&mut self, device: DeviceId) -> bool {
        if let Some(index) = self.placements.iter().position(|p| p.device == device) {
            self.placements.remove(index);
            true
        } else {
            false
        }
    }

    /// Bounding box enclosing all device monitor bounds.
    #[must_use]
    pub fn bounding_box(&self) -> Option<DisplayRect> {
        let mut rects = self
            .placements
            .iter()
            .filter_map(DevicePlacement::map_bounds);
        let first = rects.next()?;
        let mut min_x = first.left();
        let mut min_y = first.top();
        let mut max_x = first.right();
        let mut max_y = first.bottom();
        for rect in rects {
            min_x = min_x.min(rect.left());
            min_y = min_y.min(rect.top());
            max_x = max_x.max(rect.right());
            max_y = max_y.max(rect.bottom());
        }
        Some(DisplayRect::new(
            min_x,
            min_y,
            (max_x - min_x) as u32,
            (max_y - min_y) as u32,
        ))
    }

    /// Scaled live monitor previews for a UI canvas.
    #[must_use]
    pub fn monitor_previews(
        &self,
        viewport_width: f64,
        viewport_height: f64,
        padding: f64,
    ) -> Vec<MonitorPreview> {
        let Some(bounds) = self.bounding_box() else {
            return Vec::new();
        };
        if viewport_width <= padding * 2.0 || viewport_height <= padding * 2.0 {
            return Vec::new();
        }
        let scale_x = (viewport_width - padding * 2.0) / bounds.width.max(1) as f64;
        let scale_y = (viewport_height - padding * 2.0) / bounds.height.max(1) as f64;
        let scale = scale_x.min(scale_y);

        self.placements
            .iter()
            .flat_map(|placement| {
                placement
                    .map_monitors()
                    .map(move |(monitor, map_rect)| MonitorPreview {
                        device: placement.device,
                        monitor,
                        map_rect,
                        preview_rect: PreviewRect {
                            x: padding + (map_rect.left() - bounds.left()) as f64 * scale,
                            y: padding + (map_rect.top() - bounds.top()) as f64 * scale,
                            width: map_rect.width as f64 * scale,
                            height: map_rect.height as f64 * scale,
                        },
                        online: placement.online,
                    })
            })
            .collect()
    }

    /// Suggest edge links from `local` to neighbouring devices in the spatial
    /// map. `max_gap` allows editor layouts with small visual gaps to still be
    /// treated as adjacent.
    #[must_use]
    pub fn suggested_edge_links(&self, local: DeviceId, max_gap: u32) -> Vec<EdgeLink> {
        let Some(local_bounds) = self.placement(local).and_then(DevicePlacement::map_bounds) else {
            return Vec::new();
        };

        [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom]
            .into_iter()
            .filter_map(|edge| self.best_neighbor_for_edge(local, local_bounds, edge, max_gap))
            .map(|(edge, peer)| EdgeLink { edge, peer })
            .collect()
    }

    fn best_neighbor_for_edge(
        &self,
        local: DeviceId,
        local_bounds: DisplayRect,
        edge: Edge,
        max_gap: u32,
    ) -> Option<(Edge, DeviceId)> {
        self.placements
            .iter()
            .filter(|placement| placement.device != local)
            .filter_map(|placement| {
                let peer_bounds = placement.map_bounds()?;
                let candidate = edge_candidate(local_bounds, peer_bounds, edge, max_gap)?;
                Some((candidate, placement.device))
            })
            .max_by_key(|(candidate, _)| (candidate.overlap, std::cmp::Reverse(candidate.gap)))
            .map(|(_, peer)| (edge, peer))
    }
}

/// Topology editor facade used by desktop/mobile UI code.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DeviceTopologyEditor {
    map: SpatialDesktopMap,
}

impl DeviceTopologyEditor {
    /// Create an empty editor.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Read the current spatial map.
    #[must_use]
    pub fn map(&self) -> &SpatialDesktopMap {
        &self.map
    }

    /// Add or update a device placement.
    pub fn set_device(&mut self, placement: DevicePlacement) {
        self.map.upsert(placement);
    }

    /// Move a device in the spatial map.
    pub fn move_device(&mut self, device: DeviceId, origin: DesktopPoint) -> bool {
        self.map.move_device(device, origin)
    }

    /// Remove a device from the map.
    pub fn remove_device(&mut self, device: DeviceId) -> bool {
        self.map.remove(device)
    }

    /// Edge links that boundary detection should use for `local`.
    #[must_use]
    pub fn edge_links_for(&self, local: DeviceId) -> Vec<EdgeLink> {
        self.map.suggested_edge_links(local, 24)
    }

    /// Preview rectangles for a UI surface.
    #[must_use]
    pub fn monitor_previews(
        &self,
        viewport_width: f64,
        viewport_height: f64,
        padding: f64,
    ) -> Vec<MonitorPreview> {
        self.map
            .monitor_previews(viewport_width, viewport_height, padding)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EdgeCandidate {
    gap: u32,
    overlap: u32,
}

fn edge_candidate(
    local: DisplayRect,
    peer: DisplayRect,
    edge: Edge,
    max_gap: u32,
) -> Option<EdgeCandidate> {
    let (gap, overlap) = match edge {
        Edge::Left => (
            distance_between(peer.right(), local.left())?,
            axis_overlap(peer.top(), peer.bottom(), local.top(), local.bottom())?,
        ),
        Edge::Right => (
            distance_between(local.right(), peer.left())?,
            axis_overlap(peer.top(), peer.bottom(), local.top(), local.bottom())?,
        ),
        Edge::Top => (
            distance_between(peer.bottom(), local.top())?,
            axis_overlap(peer.left(), peer.right(), local.left(), local.right())?,
        ),
        Edge::Bottom => (
            distance_between(local.bottom(), peer.top())?,
            axis_overlap(peer.left(), peer.right(), local.left(), local.right())?,
        ),
    };

    if gap <= max_gap && overlap > 0 {
        Some(EdgeCandidate { gap, overlap })
    } else {
        None
    }
}

fn distance_between(first_edge: i32, second_edge: i32) -> Option<u32> {
    if first_edge <= second_edge {
        Some((second_edge - first_edge) as u32)
    } else {
        None
    }
}

fn axis_overlap(a_start: i32, a_end: i32, b_start: i32, b_end: i32) -> Option<u32> {
    let start = a_start.max(b_start);
    let end = a_end.min(b_end);
    (end > start).then_some((end - start) as u32)
}

fn offset_rect(rect: DisplayRect, origin: DesktopPoint) -> DisplayRect {
    DisplayRect::new(
        rect.x.saturating_add(origin.x),
        rect.y.saturating_add(origin.y),
        rect.width,
        rect.height,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(width: u32, height: u32) -> MonitorLayout {
        MonitorLayout::new(vec![(MonitorId(0), DisplayRect::new(0, 0, width, height))])
    }

    #[test]
    fn editor_moves_and_removes_devices() {
        let device = DeviceId::generate();
        let mut editor = DeviceTopologyEditor::new();
        editor.set_device(DevicePlacement::new(
            device,
            "Laptop",
            DesktopPoint::new(0, 0),
            layout(100, 100),
        ));
        assert!(editor.move_device(device, DesktopPoint::new(200, 0)));
        assert_eq!(
            editor.map().placement(device).unwrap().origin,
            DesktopPoint::new(200, 0)
        );
        assert!(editor.remove_device(device));
        assert!(editor.map().placement(device).is_none());
    }

    #[test]
    fn preview_scales_multiple_devices() {
        let first = DeviceId::generate();
        let second = DeviceId::generate();
        let mut map = SpatialDesktopMap::new();
        map.upsert(DevicePlacement::new(
            first,
            "Desk",
            DesktopPoint::new(0, 0),
            layout(100, 100),
        ));
        map.upsert(
            DevicePlacement::new(
                second,
                "Tablet",
                DesktopPoint::new(100, 0),
                layout(100, 100),
            )
            .with_online(true),
        );

        let previews = map.monitor_previews(220.0, 120.0, 10.0);
        assert_eq!(previews.len(), 2);
        assert_eq!(previews[0].preview_rect.width, 100.0);
        assert!(previews[1].online);
    }

    #[test]
    fn suggests_spatial_edge_links() {
        let local = DeviceId::generate();
        let right = DeviceId::generate();
        let top = DeviceId::generate();
        let mut map = SpatialDesktopMap::new();
        map.upsert(DevicePlacement::new(
            local,
            "Local",
            DesktopPoint::new(0, 0),
            layout(100, 100),
        ));
        map.upsert(DevicePlacement::new(
            right,
            "Right",
            DesktopPoint::new(100, 0),
            layout(100, 100),
        ));
        map.upsert(DevicePlacement::new(
            top,
            "Top",
            DesktopPoint::new(0, -100),
            layout(100, 100),
        ));

        let links = map.suggested_edge_links(local, 0);
        assert!(links.contains(&EdgeLink {
            edge: Edge::Right,
            peer: right,
        }));
        assert!(links.contains(&EdgeLink {
            edge: Edge::Top,
            peer: top,
        }));
    }

    #[test]
    fn gap_tolerance_allows_nearby_devices() {
        let local = DeviceId::generate();
        let right = DeviceId::generate();
        let mut map = SpatialDesktopMap::new();
        map.upsert(DevicePlacement::new(
            local,
            "Local",
            DesktopPoint::new(0, 0),
            layout(100, 100),
        ));
        map.upsert(DevicePlacement::new(
            right,
            "Right",
            DesktopPoint::new(110, 0),
            layout(100, 100),
        ));

        assert!(map.suggested_edge_links(local, 9).is_empty());
        assert_eq!(map.suggested_edge_links(local, 10).len(), 1);
    }
}
