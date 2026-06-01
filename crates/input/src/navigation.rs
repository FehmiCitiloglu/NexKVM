//! High-delight cursor navigation primitives.
//!
//! This module contains pure planning logic for the "wow" input mechanics:
//! throw-cursor handoff, infinite desktop navigation, gesture-based switching,
//! and mouse momentum transfer. Native gesture capture and pointer confinement
//! remain platform concerns (`CGEventTap`/Accessibility on macOS, raw input on
//! Windows, and portals/libei on Wayland). The state machines here are sans-IO
//! so they can be tested and safely reused by desktop, mobile, and simulations.

use std::time::Instant;

use coklu_core::identity::DeviceId;
use serde::{Deserialize, Serialize};

use crate::InputEvent;
use crate::boundary::{Edge, EdgeLink};
use crate::monitor::DisplayRect;
use crate::profile::QuickSwitch;
use crate::topology::{DesktopPoint, DevicePlacement, SpatialDesktopMap};

/// One sampled cursor position in local virtual-desktop pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorMotionSample {
    /// Horizontal pixel coordinate.
    pub x: i32,
    /// Vertical pixel coordinate.
    pub y: i32,
    /// Sample timestamp.
    pub at: Instant,
}

impl CursorMotionSample {
    /// Construct a sample.
    #[must_use]
    pub const fn new(x: i32, y: i32, at: Instant) -> Self {
        Self { x, y, at }
    }

    fn velocity_from(self, previous: Self) -> Option<(f64, f64)> {
        let elapsed = self.at.saturating_duration_since(previous.at).as_secs_f64();
        if elapsed <= f64::EPSILON {
            return None;
        }
        Some((
            (self.x - previous.x) as f64 / elapsed,
            (self.y - previous.y) as f64 / elapsed,
        ))
    }
}

/// Tuning for throw-cursor intent detection.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CursorThrowPolicy {
    /// Distance from an edge that can arm a throw.
    pub edge_margin_px: u32,
    /// Minimum velocity toward the linked edge, in pixels per second.
    pub min_velocity_px_s: f64,
    /// Maximum relative motion sent to the receiver as landing momentum.
    pub max_relative_delta: f64,
}

impl CursorThrowPolicy {
    /// Fast but deliberate LAN default.
    #[must_use]
    pub const fn lan_default() -> Self {
        Self {
            edge_margin_px: 48,
            min_velocity_px_s: 1_600.0,
            max_relative_delta: 0.08,
        }
    }
}

impl Default for CursorThrowPolicy {
    fn default() -> Self {
        Self::lan_default()
    }
}

/// A planned high-velocity cursor handoff to a linked device.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CursorThrow {
    /// Destination device.
    pub peer: DeviceId,
    /// Local edge the cursor is thrown through.
    pub edge: Edge,
    /// Normalized entry coordinate along the crossed edge.
    pub entry: f64,
    /// Relative motion that preserves some perceived momentum on arrival.
    pub momentum: InputEvent,
}

/// Detects deliberate high-velocity throws near linked screen edges.
#[derive(Debug, Clone)]
pub struct CursorThrowPlanner {
    bounds: DisplayRect,
    links: Vec<EdgeLink>,
    policy: CursorThrowPolicy,
}

impl CursorThrowPlanner {
    /// Create a planner for local desktop bounds and edge links.
    #[must_use]
    pub fn new(bounds: DisplayRect, links: Vec<EdgeLink>, policy: CursorThrowPolicy) -> Self {
        Self {
            bounds,
            links,
            policy,
        }
    }

    /// Plan a throw if the current motion strongly targets a linked edge.
    #[must_use]
    pub fn plan(
        &self,
        previous: CursorMotionSample,
        current: CursorMotionSample,
    ) -> Option<CursorThrow> {
        let (velocity_x, velocity_y) = current.velocity_from(previous)?;
        let edge = self.edge_for_intent(current, velocity_x, velocity_y)?;
        let link = self.links.iter().find(|link| link.edge == edge)?;
        Some(CursorThrow {
            peer: link.peer,
            edge,
            entry: entry_for(self.bounds, edge, current.x, current.y),
            momentum: MomentumTransfer::new(self.policy.max_relative_delta).from_velocity(
                velocity_x,
                velocity_y,
                self.bounds,
            ),
        })
    }

    fn edge_for_intent(
        &self,
        sample: CursorMotionSample,
        velocity_x: f64,
        velocity_y: f64,
    ) -> Option<Edge> {
        let margin = self.policy.edge_margin_px as i32;
        let threshold = self.policy.min_velocity_px_s;
        let horizontal_dominant = velocity_x.abs() >= velocity_y.abs();

        if horizontal_dominant
            && sample.x <= self.bounds.left().saturating_add(margin)
            && velocity_x <= -threshold
        {
            Some(Edge::Left)
        } else if horizontal_dominant
            && sample.x >= self.bounds.right().saturating_sub(margin)
            && velocity_x >= threshold
        {
            Some(Edge::Right)
        } else if !horizontal_dominant
            && sample.y <= self.bounds.top().saturating_add(margin)
            && velocity_y <= -threshold
        {
            Some(Edge::Top)
        } else if !horizontal_dominant
            && sample.y >= self.bounds.bottom().saturating_sub(margin)
            && velocity_y >= threshold
        {
            Some(Edge::Bottom)
        } else {
            None
        }
    }
}

/// Converts incoming cursor velocity into a bounded relative movement event.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MomentumTransfer {
    max_relative_delta: f64,
}

impl MomentumTransfer {
    /// Create a transfer function with a bounded normalized delta.
    #[must_use]
    pub const fn new(max_relative_delta: f64) -> Self {
        Self { max_relative_delta }
    }

    /// Convert pixel/second velocity into normalized relative input.
    #[must_use]
    pub fn from_velocity(
        self,
        velocity_x: f64,
        velocity_y: f64,
        bounds: DisplayRect,
    ) -> InputEvent {
        let width = bounds.width.max(1) as f64;
        let height = bounds.height.max(1) as f64;
        InputEvent::RelativeMove {
            dx: (velocity_x / width / 60.0)
                .clamp(-self.max_relative_delta, self.max_relative_delta),
            dy: (velocity_y / height / 60.0)
                .clamp(-self.max_relative_delta, self.max_relative_delta),
        }
    }
}

/// A planned infinite-desktop transition to another device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InfiniteDesktopTransition {
    /// Destination device.
    pub peer: DeviceId,
    /// Local edge that was crossed.
    pub edge: Edge,
    /// Suggested landing point in spatial desktop coordinates.
    pub landing: DesktopPoint,
}

/// Plans edge traversal over the full spatial desktop map.
#[derive(Debug, Clone, PartialEq)]
pub struct InfiniteDesktopNavigator {
    map: SpatialDesktopMap,
    local: DeviceId,
}

impl InfiniteDesktopNavigator {
    /// Construct a navigator from the latest topology snapshot.
    #[must_use]
    pub const fn new(map: SpatialDesktopMap, local: DeviceId) -> Self {
        Self { map, local }
    }

    /// Return the transition implied by an out-of-bounds point, if any.
    #[must_use]
    pub fn transition_for(&self, point: DesktopPoint) -> Option<InfiniteDesktopTransition> {
        let local = self.map.placement(self.local)?;
        let local_bounds = local.map_bounds()?;
        let edge = crossed_edge(local_bounds, point)?;
        let peer = nearest_online_neighbor(&self.map, self.local, local, edge)?;
        let peer_bounds = peer.map_bounds()?;
        Some(InfiniteDesktopTransition {
            peer: peer.device,
            edge,
            landing: landing_point(peer_bounds, edge, point),
        })
    }
}

/// Direction inferred from a high-level OS gesture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GestureDirection {
    /// Gesture moved toward the previous target.
    Previous,
    /// Gesture moved toward the next target.
    Next,
}

/// One normalized gesture sample from a platform backend.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GestureFrame {
    /// Number of fingers/contacts participating.
    pub contacts: u8,
    /// Horizontal delta, positive for next-device intent.
    pub delta_x: f64,
    /// Whether the platform reports the gesture completed.
    pub ended: bool,
}

/// Policy for gesture-based device switching.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GestureSwitchPolicy {
    /// Required contact count; `0` accepts platform-specific defaults.
    pub required_contacts: u8,
    /// Accumulated horizontal delta needed to switch.
    pub activation_delta: f64,
}

impl GestureSwitchPolicy {
    /// Default three-finger horizontal switch gesture.
    #[must_use]
    pub const fn three_finger_swipe() -> Self {
        Self {
            required_contacts: 3,
            activation_delta: 0.35,
        }
    }
}

impl Default for GestureSwitchPolicy {
    fn default() -> Self {
        Self::three_finger_swipe()
    }
}

/// Result of a gesture switching decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GestureSwitchDecision {
    /// Gesture direction that activated.
    pub direction: GestureDirection,
    /// Newly active device.
    pub active: DeviceId,
}

/// Stateful recognizer that converts gesture frames into quick-switch actions.
#[derive(Debug, Clone, PartialEq)]
pub struct GestureSwitchRecognizer {
    policy: GestureSwitchPolicy,
    switcher: QuickSwitch,
    accumulated_x: f64,
}

impl GestureSwitchRecognizer {
    /// Create a recognizer over a quick-switch target set.
    #[must_use]
    pub const fn new(policy: GestureSwitchPolicy, switcher: QuickSwitch) -> Self {
        Self {
            policy,
            switcher,
            accumulated_x: 0.0,
        }
    }

    /// Active target after the latest recognized switch.
    #[must_use]
    pub fn active(&self) -> Option<DeviceId> {
        self.switcher.active()
    }

    /// Feed one frame and return a decision once the gesture activates.
    pub fn push(&mut self, frame: GestureFrame) -> Option<GestureSwitchDecision> {
        if self.policy.required_contacts != 0 && frame.contacts != self.policy.required_contacts {
            self.accumulated_x = 0.0;
            return None;
        }

        self.accumulated_x += frame.delta_x;
        let decision = if self.accumulated_x >= self.policy.activation_delta {
            self.switcher
                .next_device()
                .map(|active| GestureSwitchDecision {
                    direction: GestureDirection::Next,
                    active,
                })
        } else if self.accumulated_x <= -self.policy.activation_delta {
            self.switcher
                .previous_device()
                .map(|active| GestureSwitchDecision {
                    direction: GestureDirection::Previous,
                    active,
                })
        } else {
            None
        };

        if decision.is_some() || frame.ended {
            self.accumulated_x = 0.0;
        }
        decision
    }
}

fn entry_for(bounds: DisplayRect, edge: Edge, px: i32, py: i32) -> f64 {
    match edge {
        Edge::Left | Edge::Right => {
            ((py - bounds.top()) as f64 / bounds.height.max(1) as f64).clamp(0.0, 1.0)
        }
        Edge::Top | Edge::Bottom => {
            ((px - bounds.left()) as f64 / bounds.width.max(1) as f64).clamp(0.0, 1.0)
        }
    }
}

fn crossed_edge(bounds: DisplayRect, point: DesktopPoint) -> Option<Edge> {
    if point.x < bounds.left() {
        Some(Edge::Left)
    } else if point.x >= bounds.right() {
        Some(Edge::Right)
    } else if point.y < bounds.top() {
        Some(Edge::Top)
    } else if point.y >= bounds.bottom() {
        Some(Edge::Bottom)
    } else {
        None
    }
}

fn nearest_online_neighbor<'a>(
    map: &'a SpatialDesktopMap,
    local_id: DeviceId,
    local: &DevicePlacement,
    edge: Edge,
) -> Option<&'a DevicePlacement> {
    map.placements()
        .iter()
        .filter(|placement| placement.device != local_id && placement.online)
        .filter_map(|placement| {
            directional_gap(local.map_bounds()?, placement.map_bounds()?, edge)
                .map(|gap| (gap, placement))
        })
        .min_by_key(|(gap, _)| *gap)
        .map(|(_, placement)| placement)
}

fn directional_gap(from: DisplayRect, to: DisplayRect, edge: Edge) -> Option<u32> {
    match edge {
        Edge::Left if to.right() <= from.left() => Some((from.left() - to.right()) as u32),
        Edge::Right if to.left() >= from.right() => Some((to.left() - from.right()) as u32),
        Edge::Top if to.bottom() <= from.top() => Some((from.top() - to.bottom()) as u32),
        Edge::Bottom if to.top() >= from.bottom() => Some((to.top() - from.bottom()) as u32),
        _ => None,
    }
}

fn landing_point(peer: DisplayRect, edge: Edge, point: DesktopPoint) -> DesktopPoint {
    match edge {
        Edge::Left => DesktopPoint::new(
            peer.right().saturating_sub(1),
            point.y.clamp(peer.top(), peer.bottom().saturating_sub(1)),
        ),
        Edge::Right => DesktopPoint::new(
            peer.left(),
            point.y.clamp(peer.top(), peer.bottom().saturating_sub(1)),
        ),
        Edge::Top => DesktopPoint::new(
            point.x.clamp(peer.left(), peer.right().saturating_sub(1)),
            peer.bottom().saturating_sub(1),
        ),
        Edge::Bottom => DesktopPoint::new(
            point.x.clamp(peer.left(), peer.right().saturating_sub(1)),
            peer.top(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::{MonitorId, MonitorLayout};

    fn layout(width: u32, height: u32) -> MonitorLayout {
        MonitorLayout::new(vec![(MonitorId(0), DisplayRect::new(0, 0, width, height))])
    }

    #[test]
    fn throw_planner_detects_fast_edge_intent() {
        let peer = DeviceId::generate();
        let now = Instant::now();
        let planner = CursorThrowPlanner::new(
            DisplayRect::new(0, 0, 1000, 800),
            vec![EdgeLink {
                edge: Edge::Right,
                peer,
            }],
            CursorThrowPolicy::lan_default(),
        );
        let throw = planner
            .plan(
                CursorMotionSample::new(900, 400, now),
                CursorMotionSample::new(990, 400, now + Duration::from_millis(10)),
            )
            .unwrap();
        assert_eq!(throw.peer, peer);
        assert_eq!(throw.edge, Edge::Right);
        assert!(matches!(throw.momentum, InputEvent::RelativeMove { dx, .. } if dx > 0.0));
    }

    #[test]
    fn infinite_navigation_crosses_to_online_neighbor() {
        let local = DeviceId::generate();
        let peer = DeviceId::generate();
        let mut map = SpatialDesktopMap::new();
        map.upsert(
            DevicePlacement::new(local, "Local", DesktopPoint::new(0, 0), layout(100, 100))
                .with_online(true),
        );
        map.upsert(
            DevicePlacement::new(peer, "Peer", DesktopPoint::new(100, 0), layout(100, 100))
                .with_online(true),
        );

        let nav = InfiniteDesktopNavigator::new(map, local);
        let transition = nav.transition_for(DesktopPoint::new(101, 50)).unwrap();
        assert_eq!(transition.peer, peer);
        assert_eq!(transition.landing, DesktopPoint::new(100, 50));
    }

    #[test]
    fn gesture_switcher_advances_quick_switch() {
        let first = DeviceId::generate();
        let second = DeviceId::generate();
        let mut switcher = QuickSwitch::new();
        switcher.set_order(vec![first, second]);
        let mut recognizer =
            GestureSwitchRecognizer::new(GestureSwitchPolicy::three_finger_swipe(), switcher);

        let decision = recognizer
            .push(GestureFrame {
                contacts: 3,
                delta_x: 0.4,
                ended: false,
            })
            .unwrap();
        assert_eq!(decision.direction, GestureDirection::Next);
        assert_eq!(decision.active, second);
    }
}
