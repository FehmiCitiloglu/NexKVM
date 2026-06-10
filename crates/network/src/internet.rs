//! Internet connectivity planning: WebRTC, NAT traversal, relay fallback, and
//! remote encrypted session policy.
//!
//! This is the transport control plane for remote mode. It deliberately does not
//! open sockets or perform ICE itself; a later WebRTC backend consumes these
//! plans and candidates behind the existing [`Transport`](crate::Transport)
//! trait. Keeping the policy sans-IO makes it testable and keeps security
//! decisions (trusted devices only, authenticated remote sessions) explicit.

use std::net::SocketAddr;
use std::time::Duration;

use nexkvm_core::identity::DeviceId;
use serde::{Deserialize, Serialize};

/// ICE/STUN/TURN server used by the WebRTC backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IceServer {
    /// URI such as `stun:stun.example.org:3478` or `turns:turn.example.org:5349`.
    pub uri: String,
    /// Optional username for TURN.
    pub username: Option<String>,
    /// Optional credential for TURN. Keep this out of logs in production.
    pub credential: Option<String>,
}

impl IceServer {
    /// Construct a STUN server.
    #[must_use]
    pub fn stun(uri: impl Into<String>) -> Self {
        Self {
            uri: uri.into(),
            username: None,
            credential: None,
        }
    }

    /// Construct a TURN/TURNS server.
    #[must_use]
    pub fn turn(
        uri: impl Into<String>,
        username: impl Into<String>,
        credential: impl Into<String>,
    ) -> Self {
        Self {
            uri: uri.into(),
            username: Some(username.into()),
            credential: Some(credential.into()),
        }
    }

    /// Whether this server is a relay-capable TURN server.
    #[must_use]
    pub fn is_turn(&self) -> bool {
        self.uri.starts_with("turn:") || self.uri.starts_with("turns:")
    }
}

/// NAT behavior estimate from connectivity checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NatType {
    /// No NAT, public endpoint is directly reachable.
    OpenInternet,
    /// Endpoint-independent mapping, generally hole-punch friendly.
    Cone,
    /// Address/port dependent mapping.
    Restricted,
    /// Symmetric NAT; relay is often required.
    Symmetric,
    /// UDP appears blocked.
    UdpBlocked,
    /// Not enough checks completed yet.
    Unknown,
}

/// Type of remote candidate advertised/discovered for a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CandidateKind {
    /// Direct host candidate.
    Host,
    /// Server-reflexive candidate from STUN.
    ServerReflexive,
    /// Relay candidate through TURN/nexkvm relay.
    Relay,
}

/// One remote address candidate for internet-mode connectivity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InternetCandidate {
    /// Candidate type.
    pub kind: CandidateKind,
    /// Address to try.
    pub addr: SocketAddr,
    /// Lower is preferred.
    pub priority: u32,
}

impl InternetCandidate {
    /// Construct a candidate.
    #[must_use]
    pub const fn new(kind: CandidateKind, addr: SocketAddr, priority: u32) -> Self {
        Self {
            kind,
            addr,
            priority,
        }
    }
}

/// Relay server fallback configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayConfig {
    /// Relay control/data endpoint.
    pub endpoint: SocketAddr,
    /// Whether relay traffic must use TLS/DTLS at the relay connection layer.
    pub require_tls: bool,
}

/// High-level WebRTC connectivity configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebRtcConfig {
    /// ICE servers in preference order.
    pub ice_servers: Vec<IceServer>,
    /// Optional relay fallback.
    pub relay: Option<RelayConfig>,
    /// Maximum time spent gathering/checking ICE candidates.
    pub ice_timeout: Duration,
    /// Whether relay fallback is permitted when direct checks fail.
    pub allow_relay_fallback: bool,
}

impl Default for WebRtcConfig {
    fn default() -> Self {
        Self {
            ice_servers: vec![IceServer::stun("stun:stun.l.google.com:19302")],
            relay: None,
            ice_timeout: Duration::from_secs(5),
            allow_relay_fallback: true,
        }
    }
}

/// Remote session security policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSessionPolicy {
    /// Device that will be contacted.
    pub peer: DeviceId,
    /// Whether the peer must already be trusted/pairing-authenticated.
    pub require_trusted_device: bool,
    /// Whether application-layer session encryption is required in addition to
    /// WebRTC DTLS/TLS transport encryption.
    pub require_application_encryption: bool,
    /// Whether replay protection must be enforced by the crypto session.
    pub require_replay_protection: bool,
}

impl RemoteSessionPolicy {
    /// Secure default for internet sessions.
    #[must_use]
    pub const fn trusted_encrypted(peer: DeviceId) -> Self {
        Self {
            peer,
            require_trusted_device: true,
            require_application_encryption: true,
            require_replay_protection: true,
        }
    }

    /// Whether all mandatory security requirements are enabled.
    #[must_use]
    pub const fn is_secure(&self) -> bool {
        self.require_trusted_device
            && self.require_application_encryption
            && self.require_replay_protection
    }
}

/// Chosen remote connectivity path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectivityPlan {
    /// Try direct WebRTC ICE candidates.
    WebRtcDirect {
        /// Ordered candidates.
        candidates: Vec<InternetCandidate>,
    },
    /// Use relay fallback.
    Relay {
        /// Relay endpoint.
        relay: RelayConfig,
    },
    /// No viable internet path.
    Unavailable,
}

/// Planner for NAT traversal and relay fallback.
#[derive(Debug, Clone)]
pub struct InternetConnectivityPlanner {
    config: WebRtcConfig,
}

impl InternetConnectivityPlanner {
    /// Create a planner.
    #[must_use]
    pub fn new(config: WebRtcConfig) -> Self {
        Self { config }
    }

    /// Build a connection plan from NAT estimate and gathered candidates.
    #[must_use]
    pub fn plan(&self, nat: NatType, mut candidates: Vec<InternetCandidate>) -> ConnectivityPlan {
        candidates.sort_by_key(|candidate| candidate.priority);

        let direct_viable = !matches!(nat, NatType::Symmetric | NatType::UdpBlocked)
            && candidates
                .iter()
                .any(|candidate| candidate.kind != CandidateKind::Relay);

        if direct_viable {
            ConnectivityPlan::WebRtcDirect {
                candidates: candidates
                    .into_iter()
                    .filter(|candidate| candidate.kind != CandidateKind::Relay)
                    .collect(),
            }
        } else if self.config.allow_relay_fallback {
            self.config
                .relay
                .clone()
                .map(|relay| ConnectivityPlan::Relay { relay })
                .or_else(|| {
                    candidates
                        .into_iter()
                        .find(|candidate| candidate.kind == CandidateKind::Relay)
                        .map(|candidate| ConnectivityPlan::Relay {
                            relay: RelayConfig {
                                endpoint: candidate.addr,
                                require_tls: true,
                            },
                        })
                })
                .unwrap_or(ConnectivityPlan::Unavailable)
        } else {
            ConnectivityPlan::Unavailable
        }
    }

    /// Whether the configured ICE server list includes TURN relay support.
    #[must_use]
    pub fn has_turn_server(&self) -> bool {
        self.config.ice_servers.iter().any(IceServer::is_turn)
    }
}

#[cfg(feature = "transport-webrtc")]
/// Placeholder WebRTC backend configuration exported when the feature is on.
///
/// The real backend will consume this config to create ICE agents/data channels
/// while still implementing [`crate::Transport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebRtcTransportConfig {
    /// Connectivity configuration.
    pub connectivity: WebRtcConfig,
    /// Security policy for remote sessions.
    pub security: RemoteSessionPolicy,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(port: u16) -> SocketAddr {
        ([203, 0, 113, 10], port).into()
    }

    #[test]
    fn direct_webrtc_prefers_non_relay_candidates() {
        let planner = InternetConnectivityPlanner::new(WebRtcConfig::default());
        let plan = planner.plan(
            NatType::Cone,
            vec![
                InternetCandidate::new(CandidateKind::Relay, addr(5000), 10),
                InternetCandidate::new(CandidateKind::ServerReflexive, addr(4000), 20),
            ],
        );
        let ConnectivityPlan::WebRtcDirect { candidates } = plan else {
            panic!("expected direct WebRTC plan");
        };
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].kind, CandidateKind::ServerReflexive);
    }

    #[test]
    fn symmetric_nat_uses_relay_fallback() {
        let relay = RelayConfig {
            endpoint: addr(443),
            require_tls: true,
        };
        let planner = InternetConnectivityPlanner::new(WebRtcConfig {
            relay: Some(relay.clone()),
            ..WebRtcConfig::default()
        });
        assert_eq!(
            planner.plan(NatType::Symmetric, Vec::new()),
            ConnectivityPlan::Relay { relay }
        );
    }

    #[test]
    fn relay_can_be_disabled() {
        let planner = InternetConnectivityPlanner::new(WebRtcConfig {
            allow_relay_fallback: false,
            ..WebRtcConfig::default()
        });
        assert_eq!(
            planner.plan(NatType::UdpBlocked, Vec::new()),
            ConnectivityPlan::Unavailable
        );
    }

    #[test]
    fn remote_policy_requires_trust_encryption_and_replay_protection() {
        let policy = RemoteSessionPolicy::trusted_encrypted(DeviceId::generate());
        assert!(policy.is_secure());
    }

    #[test]
    fn detects_turn_server() {
        let planner = InternetConnectivityPlanner::new(WebRtcConfig {
            ice_servers: vec![IceServer::turn("turns:relay.example:5349", "u", "p")],
            ..WebRtcConfig::default()
        });
        assert!(planner.has_turn_server());
    }
}
