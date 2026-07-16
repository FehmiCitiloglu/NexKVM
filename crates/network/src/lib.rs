//! Networking stack for nexkvm.
//!
//! # Decision: layered transport with graceful fallback
//! nexkvm speaks one application protocol ([`nexkvm_protocol`]) over a pluggable
//! transport. The [`TransportSelector`] tries transports in priority order and
//! falls back on failure:
//!
//! 1. **QUIC** (`transport-quic`) — preferred for direct LAN links. Built-in
//!    TLS 1.3, multiplexed streams (separate input/clipboard/file lanes without
//!    head-of-line blocking), low-latency datagrams for real-time input, and
//!    fast connection migration when a device roams between networks.
//! 2. **TCP + TLS** (`transport-tcp`) — universal fallback when QUIC/UDP is
//!    blocked by a firewall. Single ordered stream; framed via
//!    [`nexkvm_protocol::FrameCodec`].
//! 3. **WebRTC** (`transport-webrtc`) — later phase: NAT traversal (ICE/STUN/
//!    TURN) for remote-mode connections beyond the LAN.
//!
//! Each backend is feature-gated so builds only compile the stacks they use.
//! All backends present the same [`Transport`]/[`Connection`] traits, so the
//! rest of the platform is transport-agnostic. Security (auth, AEAD, replay
//! protection) is layered via [`nexkvm_crypto`] on top of the transport's own
//! TLS, binding the channel to device identity.
//!
//! # Networking-core building blocks
//! Beyond the transport backends, this crate provides the transport-agnostic
//! machinery a robust link needs:
//! - [`wire`] — [`nexkvm_protocol::Envelope`] ⇄ bytes codec.
//! - [`session`] — resumable, reconnect-surviving session state.
//! - [`heartbeat`] — keep-alive + liveness detection.
//! - [`latency`] — EWMA RTT/jitter measurement.
//! - [`quality`] — link quality grading from RTT/jitter/loss/throughput.
//! - [`retry`] — exponential backoff for reconnection.
//! - [`buffer`] — adaptive, latency-aware outbound batching.
//! - [`internet`] — WebRTC/NAT traversal/relay planning for remote mode.
//! - [`bandwidth`] — dynamic bandwidth and mobile-network adaptation.
//! - [`mesh`] — decentralized trusted-device mesh routing policy.
//! - [`relay`] — self-hosted/managed relay admission and route policy.
//! - [`browser`] — browser remote-session ticket planning.

mod error;
mod secure;
mod selector;
mod sequence;
mod transport;

pub mod bandwidth;
pub mod browser;
pub mod buffer;
pub mod heartbeat;
pub mod internet;
pub mod latency;
pub mod mesh;
pub mod packet;
pub mod pairing;
pub mod quality;
pub mod reconnect_auth;
pub mod relay;
pub mod remote_session;
pub mod retry;
pub mod session;
pub mod wire;

#[cfg(feature = "transport-tcp")]
mod tcp;
#[cfg(feature = "transport-tcp")]
pub use tcp::{TcpConnection, TcpTransport};

#[cfg(feature = "transport-quic")]
mod quic;
#[cfg(feature = "transport-quic")]
pub use quic::{QuicConnection, QuicTransport};

pub use bandwidth::{
    BandwidthAdapter, BandwidthPolicy, BandwidthRecommendation, BandwidthSample, NetworkProfile,
};
pub use browser::{BrowserRemoteSession, BrowserSessionPolicy, BrowserSessionTicket};
pub use buffer::{AdaptiveBuffer, BufferPolicy};
pub use error::NetworkError;
pub use heartbeat::{Heartbeat, HeartbeatConfig, LivenessMonitor};
pub use internet::{
    CandidateKind, ConnectivityPlan, IceServer, InternetCandidate, InternetConnectivityPlanner,
    NatType, RelayConfig, RemoteSessionPolicy, WebRtcConfig,
};
pub use latency::RttTracker;
pub use mesh::{MeshEdge, MeshLinkClass, MeshNode, MeshRoute, MeshRouter, MeshTrustLevel};
pub use packet::{PacketBatch, ZeroCopyPacket};
pub use pairing::{
    PairingConfirmationPrompt, exchange_pairing_approval, exchange_pairing_persistence,
    initiate_pairing_handshake, respond_pairing_handshake,
};
pub use quality::{
    NetworkQualityEstimator, NetworkQualityGrade, NetworkQualityRecommendation,
    NetworkQualitySample,
};
pub use reconnect_auth::{
    TrustedReconnectPeer, accept_trusted_reconnect, authenticate_trusted_reconnect,
};
pub use relay::{RelayAdmission, RelayPolicy, RelayRegistration, RelayRoutePlan, RelayServerKind};
pub use remote_session::{
    RejectReason, RemoteSessionAnswer, RemoteSessionError, RemoteSessionEstablisher,
    RemoteSessionId, RemoteSessionOffer, RemoteSessionState, SessionSecurityRequirements,
    answer_offer,
};
pub use retry::Backoff;
pub use secure::{
    SecureConnection, establish_trusted_session, establish_trusted_session_with_material,
    trusted_peer_session_security, trusted_session_context, trusted_session_transcript,
};
pub use selector::TransportSelector;
pub use sequence::SequencedConnection;
pub use session::{Session, SessionToken};
pub use transport::{Connection, Transport, TransportKind};

#[cfg(feature = "transport-webrtc")]
pub use internet::WebRtcTransportConfig;
