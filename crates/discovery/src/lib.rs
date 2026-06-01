//! LAN device discovery.
//!
//! coklu auto-discovers peers on the local network so pairing is zero-config.
//! Two backends share one [`ServiceAnnouncement`] model and one TTL-based
//! [`DiscoveryRegistry`]:
//!
//! - [`UdpDiscovery`] — UDP-broadcast (feature `udp-broadcast`, default).
//!   Dependency-light and universal; great as a fallback.
//! - [`MdnsDiscovery`] — mDNS/DNS-SD (feature `mdns`). The polished path that
//!   integrates with Bonjour/Avahi; preferred where available.
//!
//! Both implement the [`Discovery`] trait so the rest of the platform consumes
//! discovery events without binding to a specific backend. Discovered peers are
//! *advertised*, not *trusted*: a [`DiscoveredDevice`] becomes usable only after
//! pairing via `coklu-crypto`. [`ReconnectPlanner`] then schedules silent
//! reconnection to peers already present in the trust store.

mod announce;
mod internet;
mod proximity;
mod reconnect;
mod registry;

#[cfg(feature = "udp-broadcast")]
mod udp;

#[cfg(feature = "mdns")]
mod mdns;

use std::net::SocketAddr;

use async_trait::async_trait;
use coklu_core::identity::DeviceInfo;
use thiserror::Error;

pub use announce::{DEFAULT_DISCOVERY_PORT, SERVICE_TYPE, ServiceAnnouncement};
pub use internet::{InternetCandidateKind, InternetDiscoveryCandidate, InternetDiscoveryRecord};
pub use proximity::{
    PresencePolicy, PresenceState, PresenceTracker, ProximityObservation, ProximitySignalKind,
    ProximitySnapshot,
};
pub use reconnect::{ReconnectPlanner, ReconnectPolicy, ReconnectTarget};
pub use registry::{DEFAULT_TTL, DiscoveryRegistry};

#[cfg(feature = "udp-broadcast")]
pub use udp::{UdpConfig, UdpDiscovery};

#[cfg(feature = "mdns")]
pub use mdns::MdnsDiscovery;

/// Errors from the discovery subsystem.
#[derive(Debug, Error)]
pub enum DiscoveryError {
    /// Failed to start advertising or browsing on the network.
    #[error("discovery backend error: {0}")]
    Backend(String),

    /// Failed to encode/decode an announcement.
    #[error("announcement codec error: {0}")]
    Codec(String),

    /// Underlying socket/IO failure.
    #[error("discovery io error: {0}")]
    Io(#[from] std::io::Error),
}

/// A peer found on the LAN, not yet connected or trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredDevice {
    /// Advertised device metadata.
    pub info: DeviceInfo,
    /// Socket address to dial for a session.
    pub addr: SocketAddr,
    /// Advertised public-key fingerprint, if any, for trust matching prior to
    /// connecting. Never treat this as proof of identity — only the handshake
    /// authenticates the peer.
    pub fingerprint: Option<String>,
}

/// LAN discovery backend.
#[async_trait]
pub trait Discovery: Send + Sync {
    /// Begin (or refresh) advertising this device on the LAN.
    ///
    /// `addr` is the data-plane address peers should dial for a session.
    ///
    /// # Errors
    /// Returns [`DiscoveryError`] if advertising cannot start.
    async fn advertise(&self, info: &DeviceInfo, addr: SocketAddr) -> Result<(), DiscoveryError>;

    /// Return the peers currently visible on the network.
    ///
    /// # Errors
    /// Returns [`DiscoveryError`] if browsing fails.
    async fn discovered(&self) -> Result<Vec<DiscoveredDevice>, DiscoveryError>;
}
