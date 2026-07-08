//! Mouse-sharing focus controller: the state machine that turns the pure
//! geometry primitives ([`BoundaryDetector`], [`MonitorLayout`]) into a working
//! cross-device cursor hand-off.
//!
//! The existing modules answer isolated questions — "did the cursor cross a
//! linked edge?" ([`BoundaryDetector`]), "what is this pixel in normalized
//! space?" ([`MonitorLayout`]) — but nothing tracks *where the cursor currently
//! lives*. [`MouseShareController`] owns that focus state and drives the full
//! Synergy/Universal-Control loop for one local device:
//!
//! 1. **Local focus** — the cursor is on this machine. Each local cursor sample
//!    is checked against the linked edges; crossing one hands focus to the peer
//!    and emits the peer's *entry point* so the remote cursor appears at the
//!    matching spot.
//! 2. **Remote focus** — the cursor is on a peer. Local motion is consumed as
//!    normalized deltas, forwarded to that peer as absolute [`InputEvent::PointerMove`]s,
//!    and tracked on a virtual `[0,1]` cursor. When that virtual cursor slides
//!    back across the entry edge, focus returns to this device and the controller
//!    reports the local pixel at which to re-home the cursor.
//!
//! Pure logic: clock-free, OS-free, network-free. The platform/network driver
//! feeds it samples and acts on the returned [`ShareOutput`] (release/grab the
//! local cursor, forward events to the peer). The receiver side reuses the
//! existing [`MonitorLayout::denormalize`] + [`CursorInterpolator`](crate::CursorInterpolator)
//! to place and smooth the incoming cursor, so no new receiver logic is needed.

use nexkvm_core::identity::DeviceId;

use crate::InputEvent;
use crate::boundary::{BoundaryDetector, Edge, Transition};
use crate::monitor::MonitorLayout;

/// Where the shared cursor currently is, from this device's perspective.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorFocus {
    /// The cursor is on this device; local input is used locally.
    Local,
    /// The cursor has been handed off to `peer`; local input is forwarded.
    Remote {
        /// The device the cursor is currently on.
        peer: DeviceId,
        /// The local edge it left through (used to compute the return).
        via: Edge,
    },
}

/// The point at which a peer's cursor should appear when focus moves to it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PeerEntry {
    /// The device the cursor is entering.
    pub peer: DeviceId,
    /// The local edge that was crossed (sender perspective).
    pub edge: Edge,
    /// Normalized `[0,1]` x on the peer where the cursor should appear.
    pub x: f64,
    /// Normalized `[0,1]` y on the peer where the cursor should appear.
    pub y: f64,
}

impl PeerEntry {
    /// The absolute pointer event to seed the peer's cursor position.
    #[must_use]
    pub fn entry_event(&self) -> InputEvent {
        InputEvent::PointerMove {
            x: self.x,
            y: self.y,
        }
    }
}

/// What the driver should do after feeding a sample to the controller.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ShareOutput {
    /// Nothing to do; cursor stays where it is (focus unchanged).
    Idle,
    /// Focus just moved to a peer: release the local cursor and forward the
    /// carried entry event so the peer drops its cursor in at the right spot.
    EnterRemote(PeerEntry),
    /// Focus is on a peer: forward this event to `peer`.
    Forward {
        /// The peer the event is destined for.
        peer: DeviceId,
        /// The absolute pointer position to apply on the peer.
        event: InputEvent,
    },
    /// Focus just returned to this device: re-home the local cursor at these
    /// global pixels and resume using input locally.
    ReturnLocal {
        /// Global-desktop x to place the local cursor at.
        x: i32,
        /// Global-desktop y to place the local cursor at.
        y: i32,
    },
}

/// Drives cursor hand-off between this device and its edge-linked peers.
#[derive(Debug, Clone)]
pub struct MouseShareController {
    boundary: BoundaryDetector,
    local_layout: MonitorLayout,
    focus: CursorFocus,
    /// Virtual normalized cursor on the peer while focus is `Remote`.
    virtual_pos: (f64, f64),
}

impl MouseShareController {
    /// Create a controller for the local device.
    ///
    /// `boundary` carries the edge links to peers; `local_layout` is used to map
    /// a return crossing back to local pixels.
    #[must_use]
    pub fn new(boundary: BoundaryDetector, local_layout: MonitorLayout) -> Self {
        Self {
            boundary,
            local_layout,
            focus: CursorFocus::Local,
            virtual_pos: (0.0, 0.0),
        }
    }

    /// The current focus state.
    #[must_use]
    pub fn focus(&self) -> CursorFocus {
        self.focus
    }

    /// The peer the cursor is currently on, if any.
    #[must_use]
    pub fn active_peer(&self) -> Option<DeviceId> {
        match self.focus {
            CursorFocus::Remote { peer, .. } => Some(peer),
            CursorFocus::Local => None,
        }
    }

    /// Force remote focus back to the local device.
    ///
    /// Returns `true` when this call actually released a remote focus.
    pub fn release_remote(&mut self) -> bool {
        let was_remote = matches!(self.focus, CursorFocus::Remote { .. });
        self.focus = CursorFocus::Local;
        self.virtual_pos = (0.0, 0.0);
        was_remote
    }

    /// Replace the edge links (e.g. after the device topology changes).
    pub fn set_links(&mut self, links: Vec<crate::boundary::EdgeLink>) {
        self.boundary.set_links(links);
    }

    /// Feed a local cursor sample in global pixels (used while focus is local).
    ///
    /// Returns [`ShareOutput::EnterRemote`] if the sample crossed a linked edge,
    /// otherwise [`ShareOutput::Idle`]. Has no effect while focus is remote — use
    /// [`on_remote_motion`](Self::on_remote_motion) then.
    pub fn on_local_cursor(&mut self, px: i32, py: i32) -> ShareOutput {
        if self.focus != CursorFocus::Local {
            return ShareOutput::Idle;
        }
        let Some(transition) = self.boundary.detect(px, py) else {
            return ShareOutput::Idle;
        };
        let entry = peer_entry_for(&transition);
        self.focus = CursorFocus::Remote {
            peer: transition.peer,
            via: transition.edge,
        };
        self.virtual_pos = (entry.x, entry.y);
        ShareOutput::EnterRemote(entry)
    }

    /// Feed normalized motion deltas while focus is on a peer.
    ///
    /// `dx`/`dy` are fractions of the desktop per axis (the same units as
    /// [`InputEvent::RelativeMove`]). The controller advances the virtual peer
    /// cursor and either forwards an absolute position to the peer
    /// ([`ShareOutput::Forward`]) or, if the cursor slid back over the entry
    /// edge, returns focus locally ([`ShareOutput::ReturnLocal`]). Returns
    /// [`ShareOutput::Idle`] while focus is local.
    pub fn on_remote_motion(&mut self, dx: f64, dy: f64) -> ShareOutput {
        let CursorFocus::Remote { peer, via } = self.focus else {
            return ShareOutput::Idle;
        };

        let nx = self.virtual_pos.0 + dx;
        let ny = self.virtual_pos.1 + dy;

        if let Some((rx, ry)) = return_point(via, nx, ny) {
            // Cursor crossed back over the entry edge: re-home locally.
            self.focus = CursorFocus::Local;
            self.virtual_pos = (0.0, 0.0);
            let (px, py) = self.local_layout.denormalize(rx, ry).unwrap_or((0, 0));
            return ShareOutput::ReturnLocal { x: px, y: py };
        }

        self.virtual_pos = (nx.clamp(0.0, 1.0), ny.clamp(0.0, 1.0));
        ShareOutput::Forward {
            peer,
            event: InputEvent::PointerMove {
                x: self.virtual_pos.0,
                y: self.virtual_pos.1,
            },
        }
    }
}

/// Map a local edge crossing to the peer's entry point.
///
/// The cursor leaves one edge of the local desktop and arrives on the *opposite*
/// edge of the peer at the same position along that edge (`transition.entry`).
fn peer_entry_for(t: &Transition) -> PeerEntry {
    let (x, y) = match t.edge {
        // Off the right → in from the peer's left.
        Edge::Right => (0.0, t.entry),
        // Off the left → in from the peer's right.
        Edge::Left => (1.0, t.entry),
        // Off the top → in from the peer's bottom.
        Edge::Top => (t.entry, 1.0),
        // Off the bottom → in from the peer's top.
        Edge::Bottom => (t.entry, 0.0),
    };
    PeerEntry {
        peer: t.peer,
        edge: t.edge,
        x,
        y,
    }
}

/// If `(nx, ny)` slid back across the edge the cursor entered through, return the
/// normalized local re-entry point; otherwise `None` (still on the peer).
fn return_point(via: Edge, nx: f64, ny: f64) -> Option<(f64, f64)> {
    match via {
        // Entered the peer from its left (x=0); returns when it goes back left.
        Edge::Right if nx < 0.0 => Some((1.0, ny.clamp(0.0, 1.0))),
        // Entered from the peer's right (x=1); returns when it goes back right.
        Edge::Left if nx > 1.0 => Some((0.0, ny.clamp(0.0, 1.0))),
        // Entered from the peer's bottom (y=1); returns when it goes back down.
        Edge::Top if ny > 1.0 => Some((nx.clamp(0.0, 1.0), 0.0)),
        // Entered from the peer's top (y=0); returns when it goes back up.
        Edge::Bottom if ny < 0.0 => Some((nx.clamp(0.0, 1.0), 1.0)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boundary::EdgeLink;
    use crate::monitor::{DisplayRect, MonitorId};

    fn single_monitor() -> MonitorLayout {
        MonitorLayout::new(vec![(MonitorId(0), DisplayRect::new(0, 0, 1920, 1080))])
    }

    fn controller(links: Vec<EdgeLink>) -> MouseShareController {
        let boundary = BoundaryDetector::new(DisplayRect::new(0, 0, 1920, 1080), links);
        MouseShareController::new(boundary, single_monitor())
    }

    #[test]
    fn stays_local_inside_desktop() {
        let mut ctrl = controller(vec![]);
        assert_eq!(ctrl.on_local_cursor(960, 540), ShareOutput::Idle);
        assert_eq!(ctrl.focus(), CursorFocus::Local);
        assert_eq!(ctrl.active_peer(), None);
    }

    #[test]
    fn crossing_right_edge_enters_peer_from_its_left() {
        let peer = DeviceId::generate();
        let mut ctrl = controller(vec![EdgeLink {
            edge: Edge::Right,
            peer,
        }]);

        let out = ctrl.on_local_cursor(1920, 540);
        match out {
            ShareOutput::EnterRemote(entry) => {
                assert_eq!(entry.peer, peer);
                assert_eq!(entry.edge, Edge::Right);
                assert!((entry.x - 0.0).abs() < 1e-9, "enters peer left edge");
                assert!((entry.y - 0.5).abs() < 1e-9, "at mid-height");
            }
            other => panic!("expected EnterRemote, got {other:?}"),
        }
        assert_eq!(ctrl.active_peer(), Some(peer));
    }

    #[test]
    fn remote_motion_is_forwarded_as_absolute_position() {
        let peer = DeviceId::generate();
        let mut ctrl = controller(vec![EdgeLink {
            edge: Edge::Right,
            peer,
        }]);
        ctrl.on_local_cursor(1920, 540); // enter peer at (0.0, 0.5)

        // Move right and up on the peer.
        let out = ctrl.on_remote_motion(0.25, -0.1);
        match out {
            ShareOutput::Forward {
                peer: p,
                event: InputEvent::PointerMove { x, y },
            } => {
                assert_eq!(p, peer);
                assert!((x - 0.25).abs() < 1e-9);
                assert!((y - 0.4).abs() < 1e-9);
            }
            other => panic!("expected Forward PointerMove, got {other:?}"),
        }
    }

    #[test]
    fn sliding_back_over_entry_edge_returns_local() {
        let peer = DeviceId::generate();
        let mut ctrl = controller(vec![EdgeLink {
            edge: Edge::Right,
            peer,
        }]);
        ctrl.on_local_cursor(1920, 540); // enter peer at (0.0, 0.5)

        // Move further into the peer, then back past its left edge.
        assert!(matches!(
            ctrl.on_remote_motion(0.3, 0.0),
            ShareOutput::Forward { .. }
        ));
        let out = ctrl.on_remote_motion(-0.5, 0.0); // 0.3 - 0.5 = -0.2 < 0 → return
        match out {
            ShareOutput::ReturnLocal { x, y } => {
                // Re-home at the local right edge (x≈1919), mid-height.
                assert_eq!(x, 1920); // denormalize rounds 1.0*1920
                assert_eq!(y, 540);
            }
            other => panic!("expected ReturnLocal, got {other:?}"),
        }
        assert_eq!(ctrl.focus(), CursorFocus::Local);
        assert_eq!(ctrl.active_peer(), None);
    }

    #[test]
    fn local_cursor_ignored_while_remote() {
        let peer = DeviceId::generate();
        let mut ctrl = controller(vec![EdgeLink {
            edge: Edge::Right,
            peer,
        }]);
        ctrl.on_local_cursor(1920, 540);
        // Already remote: local samples are a no-op until we return.
        assert_eq!(ctrl.on_local_cursor(100, 100), ShareOutput::Idle);
        assert_eq!(ctrl.active_peer(), Some(peer));
    }

    #[test]
    fn remote_motion_ignored_while_local() {
        let mut ctrl = controller(vec![]);
        assert_eq!(ctrl.on_remote_motion(0.5, 0.5), ShareOutput::Idle);
    }

    #[test]
    fn release_remote_returns_focus_to_local() {
        let peer = DeviceId::generate();
        let mut ctrl = controller(vec![EdgeLink {
            edge: Edge::Right,
            peer,
        }]);
        ctrl.on_local_cursor(1920, 540);

        assert!(ctrl.release_remote());
        assert_eq!(ctrl.focus(), CursorFocus::Local);
        assert_eq!(ctrl.active_peer(), None);
        assert!(!ctrl.release_remote());
    }
}
