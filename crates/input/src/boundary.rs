//! Cursor boundary transitions between linked devices.
//!
//! In a multi-device layout the user "slides" the cursor off one screen's edge
//! onto a neighbour, Synergy/Universal-Control style. Each outer [`Edge`] of the
//! local virtual desktop may be linked to a peer device via an [`EdgeLink`].
//! When the cursor crosses a linked edge, [`BoundaryDetector::detect`] emits a
//! [`Transition`] naming the peer and the *entry coordinate* — the normalized
//! `[0,1]` position along the shared edge — so the peer can drop the cursor in at
//! the matching spot for seamless continuity.
//!
//! Pure logic, clock-free and OS-free: the platform layer feeds it pixel cursor
//! positions and acts on the returned transitions (releasing local capture,
//! forwarding subsequent input to the peer).

use coklu_core::identity::DeviceId;

use crate::monitor::DisplayRect;

/// One of the four outer edges of the virtual desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    /// Left edge (cursor x below the desktop's left bound).
    Left,
    /// Right edge (cursor x at/beyond the right bound).
    Right,
    /// Top edge (cursor y above the top bound).
    Top,
    /// Bottom edge (cursor y at/beyond the bottom bound).
    Bottom,
}

/// Links an outer edge of the local desktop to a neighbouring device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EdgeLink {
    /// Which edge hands off.
    pub edge: Edge,
    /// The device the cursor moves onto when crossing `edge`.
    pub peer: DeviceId,
}

/// Emitted when the cursor crosses a linked edge.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transition {
    /// The device the cursor is moving to.
    pub peer: DeviceId,
    /// The edge that was crossed (local perspective).
    pub edge: Edge,
    /// Normalized `[0,1]` position along the crossed edge: for left/right edges
    /// this is the vertical position; for top/bottom, the horizontal position.
    /// The peer uses it to place the incoming cursor at the matching point.
    pub entry: f64,
}

/// Detects cursor hand-offs across configured edges of a desktop.
#[derive(Debug, Clone)]
pub struct BoundaryDetector {
    bounds: DisplayRect,
    links: Vec<EdgeLink>,
}

impl BoundaryDetector {
    /// Create a detector over `bounds` (typically the layout bounding box) with
    /// the given edge links.
    #[must_use]
    pub fn new(bounds: DisplayRect, links: Vec<EdgeLink>) -> Self {
        Self { bounds, links }
    }

    /// Update the desktop bounds (e.g. after a monitor hot-plug).
    pub fn set_bounds(&mut self, bounds: DisplayRect) {
        self.bounds = bounds;
    }

    /// Replace the edge links.
    pub fn set_links(&mut self, links: Vec<EdgeLink>) {
        self.links = links;
    }

    /// Given a (possibly out-of-bounds) cursor position, return a transition if
    /// it crossed a *linked* edge.
    ///
    /// The platform feeds raw pixel positions; the OS may clamp the cursor to
    /// the desktop, so callers typically detect intent from the unclamped
    /// position or by sampling against the edge. When a point pushes past more
    /// than one edge (a corner), horizontal edges take priority, matching the
    /// common left/right device arrangement.
    #[must_use]
    pub fn detect(&self, px: i32, py: i32) -> Option<Transition> {
        let edge = if px < self.bounds.left() {
            Edge::Left
        } else if px >= self.bounds.right() {
            Edge::Right
        } else if py < self.bounds.top() {
            Edge::Top
        } else if py >= self.bounds.bottom() {
            Edge::Bottom
        } else {
            return None; // still inside the desktop
        };

        let link = self.links.iter().find(|l| l.edge == edge)?;
        Some(Transition {
            peer: link.peer,
            edge,
            entry: self.entry_for(edge, px, py),
        })
    }

    /// Normalized position along `edge` for the crossing point.
    fn entry_for(&self, edge: Edge, px: i32, py: i32) -> f64 {
        match edge {
            Edge::Left | Edge::Right => {
                let h = self.bounds.height.max(1) as f64;
                ((py - self.bounds.top()) as f64 / h).clamp(0.0, 1.0)
            }
            Edge::Top | Edge::Bottom => {
                let w = self.bounds.width.max(1) as f64;
                ((px - self.bounds.left()) as f64 / w).clamp(0.0, 1.0)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use coklu_core::identity::DeviceId;

    fn detector(links: Vec<EdgeLink>) -> BoundaryDetector {
        BoundaryDetector::new(DisplayRect::new(0, 0, 1920, 1080), links)
    }

    #[test]
    fn inside_bounds_yields_no_transition() {
        let det = detector(vec![]);
        assert!(det.detect(960, 540).is_none());
    }

    #[test]
    fn crossing_linked_right_edge_transitions() {
        let peer = DeviceId::generate();
        let det = detector(vec![EdgeLink {
            edge: Edge::Right,
            peer,
        }]);
        let t = det.detect(1920, 540).expect("should transition");
        assert_eq!(t.peer, peer);
        assert_eq!(t.edge, Edge::Right);
        assert!((t.entry - 0.5).abs() < 1e-9, "mid-height entry");
    }

    #[test]
    fn crossing_unlinked_edge_yields_nothing() {
        // Only the right edge is linked; cross the left edge.
        let det = detector(vec![EdgeLink {
            edge: Edge::Right,
            peer: DeviceId::generate(),
        }]);
        assert!(det.detect(-1, 540).is_none());
    }

    #[test]
    fn entry_uses_horizontal_axis_for_top_edge() {
        let peer = DeviceId::generate();
        let det = detector(vec![EdgeLink {
            edge: Edge::Top,
            peer,
        }]);
        let t = det.detect(480, -1).unwrap();
        assert_eq!(t.edge, Edge::Top);
        assert!((t.entry - 0.25).abs() < 1e-9);
    }

    #[test]
    fn corner_prefers_horizontal_edge() {
        let peer = DeviceId::generate();
        // Both left and top linked; crossing the top-left corner picks Left
        // (horizontal edges checked first).
        let det = detector(vec![
            EdgeLink {
                edge: Edge::Left,
                peer,
            },
            EdgeLink {
                edge: Edge::Top,
                peer,
            },
        ]);
        let t = det.detect(-1, -1).unwrap();
        assert_eq!(t.edge, Edge::Left);
    }
}
