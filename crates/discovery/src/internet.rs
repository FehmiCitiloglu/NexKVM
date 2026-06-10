//! Internet device discovery metadata.
//!
//! LAN discovery is broadcast/mDNS. Internet discovery is different: a trusted
//! rendezvous service can publish reachability hints (ICE candidates, relay
//! capability, protocol version) so paired devices can attempt WebRTC/NAT
//! traversal. These records are **not authentication**; the remote encrypted
//! session handshake still proves device identity.

use nexkvm_core::identity::{DeviceId, DeviceInfo};
use serde::{Deserialize, Serialize};

/// Internet-discovery candidate kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InternetCandidateKind {
    /// WebRTC host/server-reflexive candidate.
    WebRtc,
    /// Relay fallback is available.
    Relay,
}

/// Candidate advertised through internet discovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InternetDiscoveryCandidate {
    /// Candidate kind.
    pub kind: InternetCandidateKind,
    /// Opaque endpoint/candidate string for the remote connectivity planner.
    pub endpoint: String,
    /// Lower is preferred.
    pub priority: u32,
}

/// Internet-visible record for one device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InternetDiscoveryRecord {
    /// Advertised device metadata.
    pub info: DeviceInfo,
    /// Optional public-key fingerprint for trust-store matching.
    pub fingerprint: Option<String>,
    /// Protocol major version.
    pub proto_major: u16,
    /// Reachability candidates.
    pub candidates: Vec<InternetDiscoveryCandidate>,
    /// Whether a relay fallback is allowed by this device.
    pub relay_allowed: bool,
}

impl InternetDiscoveryRecord {
    /// Build a record.
    #[must_use]
    pub fn new(info: DeviceInfo, proto_major: u16) -> Self {
        Self {
            info,
            fingerprint: None,
            proto_major,
            candidates: Vec::new(),
            relay_allowed: true,
        }
    }

    /// Attach a fingerprint for pre-handshake trust-store lookup.
    #[must_use]
    pub fn with_fingerprint(mut self, fingerprint: impl Into<String>) -> Self {
        self.fingerprint = Some(fingerprint.into());
        self
    }

    /// Add a candidate.
    pub fn add_candidate(&mut self, candidate: InternetDiscoveryCandidate) {
        self.candidates.push(candidate);
        self.candidates.sort_by_key(|candidate| candidate.priority);
    }

    /// Shorthand for device id.
    #[must_use]
    pub fn device_id(&self) -> DeviceId {
        self.info.id
    }

    /// Whether this record has any viable remote candidate.
    #[must_use]
    pub fn is_reachable(&self) -> bool {
        !self.candidates.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexkvm_core::identity::OsKind;

    #[test]
    fn candidates_are_sorted_by_priority() {
        let info = DeviceInfo::new("phone", OsKind::Android);
        let mut record = InternetDiscoveryRecord::new(info, 1);
        record.add_candidate(InternetDiscoveryCandidate {
            kind: InternetCandidateKind::Relay,
            endpoint: "relay.example:443".into(),
            priority: 50,
        });
        record.add_candidate(InternetDiscoveryCandidate {
            kind: InternetCandidateKind::WebRtc,
            endpoint: "candidate:srflx".into(),
            priority: 10,
        });
        assert!(record.is_reachable());
        assert_eq!(record.candidates[0].priority, 10);
    }

    #[test]
    fn device_id_matches_info() {
        let info = DeviceInfo::new("laptop", OsKind::Linux);
        let id = info.id;
        let record = InternetDiscoveryRecord::new(info, 1);
        assert_eq!(record.device_id(), id);
    }
}
