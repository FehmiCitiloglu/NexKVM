//! Fully decentralized mesh routing policy.
//!
//! Mesh mode lets trusted devices form a peer-to-peer graph instead of relying
//! on one server-shaped coordinator. This module is deliberately sans-IO: it
//! chooses routes from authenticated topology observations, while discovery,
//! crypto, and transports still own peer discovery, identity proof, encryption,
//! and replay protection.

use std::collections::{HashMap, HashSet, VecDeque};

use nexkvm_core::identity::DeviceId;

/// Trust level assigned to a mesh node by the local trust/policy layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeshTrustLevel {
    /// Unknown or unauthenticated; must not route traffic.
    Untrusted,
    /// Paired device allowed to send/receive its own traffic.
    TrustedDevice,
    /// Explicitly permitted to forward encrypted traffic for others.
    TrustedRouter,
}

/// Link class between two mesh nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeshLinkClass {
    /// Direct LAN or local peer-to-peer link.
    DirectLan,
    /// Internet peer-to-peer path such as WebRTC ICE.
    InternetP2p,
    /// Relay-assisted link.
    Relay,
}

/// One device in the mesh topology.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeshNode {
    /// Device id.
    pub device: DeviceId,
    /// Trust level.
    pub trust: MeshTrustLevel,
    /// Whether this node is currently online.
    pub online: bool,
}

/// One authenticated mesh edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeshEdge {
    /// First endpoint.
    pub a: DeviceId,
    /// Second endpoint.
    pub b: DeviceId,
    /// Link class.
    pub class: MeshLinkClass,
    /// Lower is preferred.
    pub cost: u16,
    /// Whether app-layer encryption is confirmed for this edge.
    pub encrypted: bool,
}

impl MeshEdge {
    /// Construct a bidirectional edge.
    #[must_use]
    pub const fn new(
        a: DeviceId,
        b: DeviceId,
        class: MeshLinkClass,
        cost: u16,
        encrypted: bool,
    ) -> Self {
        Self {
            a,
            b,
            class,
            cost,
            encrypted,
        }
    }

    fn other(&self, device: DeviceId) -> Option<DeviceId> {
        if self.a == device {
            Some(self.b)
        } else if self.b == device {
            Some(self.a)
        } else {
            None
        }
    }
}

/// Planned route through the decentralized mesh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeshRoute {
    /// Ordered devices from source to destination.
    pub path: Vec<DeviceId>,
    /// Aggregate route cost.
    pub total_cost: u32,
}

/// In-memory mesh route planner.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MeshRouter {
    nodes: HashMap<DeviceId, MeshNode>,
    edges: Vec<MeshEdge>,
}

impl MeshRouter {
    /// Create an empty mesh router.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add or replace a node.
    pub fn upsert_node(&mut self, node: MeshNode) {
        self.nodes.insert(node.device, node);
    }

    /// Add or replace an edge between two devices.
    pub fn upsert_edge(&mut self, edge: MeshEdge) {
        if let Some(existing) = self
            .edges
            .iter_mut()
            .find(|existing| same_pair(existing, &edge))
        {
            *existing = edge;
        } else {
            self.edges.push(edge);
        }
    }

    /// Plan the lowest-cost secure route from `from` to `to`.
    #[must_use]
    pub fn route(&self, from: DeviceId, to: DeviceId) -> Option<MeshRoute> {
        self.route_with_policy(from, to, true)
    }

    /// Plan a route, optionally requiring every edge to be encrypted.
    #[must_use]
    pub fn route_with_policy(
        &self,
        from: DeviceId,
        to: DeviceId,
        require_encryption: bool,
    ) -> Option<MeshRoute> {
        if !self.node_can_endpoint(from) || !self.node_can_endpoint(to) {
            return None;
        }

        let mut best_cost: HashMap<DeviceId, u32> = HashMap::new();
        let mut previous: HashMap<DeviceId, DeviceId> = HashMap::new();
        let mut queue = VecDeque::from([from]);
        best_cost.insert(from, 0);

        while let Some(current) = queue.pop_front() {
            let current_cost = best_cost[&current];
            for edge in self.usable_edges(current, require_encryption) {
                let next = edge.other(current)?;
                if next != to && !self.node_can_forward(next) {
                    continue;
                }
                let next_cost = current_cost.saturating_add(edge.cost as u32);
                if best_cost.get(&next).is_none_or(|known| next_cost < *known) {
                    best_cost.insert(next, next_cost);
                    previous.insert(next, current);
                    queue.push_back(next);
                }
            }
        }

        let total_cost = *best_cost.get(&to)?;
        Some(MeshRoute {
            path: reconstruct_path(from, to, &previous)?,
            total_cost,
        })
    }

    fn usable_edges(
        &self,
        device: DeviceId,
        require_encryption: bool,
    ) -> impl Iterator<Item = &MeshEdge> {
        self.edges.iter().filter(move |edge| {
            (!require_encryption || edge.encrypted)
                && (edge.a == device || edge.b == device)
                && edge
                    .other(device)
                    .is_some_and(|other| self.node_is_online(other))
        })
    }

    fn node_can_endpoint(&self, device: DeviceId) -> bool {
        self.nodes.get(&device).is_some_and(|node| {
            node.online
                && matches!(
                    node.trust,
                    MeshTrustLevel::TrustedDevice | MeshTrustLevel::TrustedRouter
                )
        })
    }

    fn node_can_forward(&self, device: DeviceId) -> bool {
        self.nodes
            .get(&device)
            .is_some_and(|node| node.online && matches!(node.trust, MeshTrustLevel::TrustedRouter))
    }

    fn node_is_online(&self, device: DeviceId) -> bool {
        self.nodes.get(&device).is_some_and(|node| node.online)
    }
}

fn same_pair(left: &MeshEdge, right: &MeshEdge) -> bool {
    (left.a == right.a && left.b == right.b) || (left.a == right.b && left.b == right.a)
}

fn reconstruct_path(
    from: DeviceId,
    to: DeviceId,
    previous: &HashMap<DeviceId, DeviceId>,
) -> Option<Vec<DeviceId>> {
    let mut seen = HashSet::new();
    let mut path = vec![to];
    let mut current = to;
    while current != from {
        if !seen.insert(current) {
            return None;
        }
        current = *previous.get(&current)?;
        path.push(current);
    }
    path.reverse();
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(device: DeviceId, trust: MeshTrustLevel) -> MeshNode {
        MeshNode {
            device,
            trust,
            online: true,
        }
    }

    #[test]
    fn routes_through_trusted_router() {
        let a = DeviceId::generate();
        let router = DeviceId::generate();
        let b = DeviceId::generate();
        let mut mesh = MeshRouter::new();
        mesh.upsert_node(node(a, MeshTrustLevel::TrustedDevice));
        mesh.upsert_node(node(router, MeshTrustLevel::TrustedRouter));
        mesh.upsert_node(node(b, MeshTrustLevel::TrustedDevice));
        mesh.upsert_edge(MeshEdge::new(a, router, MeshLinkClass::DirectLan, 10, true));
        mesh.upsert_edge(MeshEdge::new(
            router,
            b,
            MeshLinkClass::InternetP2p,
            20,
            true,
        ));

        let route = mesh.route(a, b).unwrap();
        assert_eq!(route.path, vec![a, router, b]);
        assert_eq!(route.total_cost, 30);
    }

    #[test]
    fn refuses_untrusted_forwarder_and_unencrypted_edges() {
        let a = DeviceId::generate();
        let middle = DeviceId::generate();
        let b = DeviceId::generate();
        let mut mesh = MeshRouter::new();
        mesh.upsert_node(node(a, MeshTrustLevel::TrustedDevice));
        mesh.upsert_node(node(middle, MeshTrustLevel::TrustedDevice));
        mesh.upsert_node(node(b, MeshTrustLevel::TrustedDevice));
        mesh.upsert_edge(MeshEdge::new(a, middle, MeshLinkClass::DirectLan, 1, true));
        mesh.upsert_edge(MeshEdge::new(middle, b, MeshLinkClass::DirectLan, 1, false));

        assert!(mesh.route(a, b).is_none());
    }
}
