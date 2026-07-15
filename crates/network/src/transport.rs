//! Transport-agnostic connection traits.

use std::net::SocketAddr;

use async_trait::async_trait;
use nexkvm_crypto::PublicKey;
use nexkvm_protocol::Envelope;

use crate::error::NetworkError;

/// Identifies which transport backend produced a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransportKind {
    /// QUIC (UDP, TLS 1.3, multiplexed).
    Quic,
    /// TCP + TLS.
    Tcp,
    /// WebRTC data channel.
    WebRtc,
}

impl TransportKind {
    /// Priority order the selector attempts, most-preferred first.
    pub const PRIORITY: [TransportKind; 3] = [
        TransportKind::Quic,
        TransportKind::Tcp,
        TransportKind::WebRtc,
    ];
}

/// An established, authenticated link to a peer.
///
/// Sends/receives whole [`Envelope`]s; framing and encryption are the
/// implementation's responsibility so callers stay transport-agnostic. Methods
/// take `&self` (not `&mut`) so a connection can be shared across tasks; backends
/// guard their internal streams as needed without holding locks across `.await`.
#[async_trait]
pub trait Connection: Send + Sync {
    /// Which transport backs this connection.
    fn kind(&self) -> TransportKind;

    /// Remote peer address.
    fn peer_addr(&self) -> SocketAddr;

    /// Authenticated peer identity when the connection completed the trusted
    /// session handshake. Plain transport connections return `None`.
    fn peer_identity(&self) -> Option<PublicKey> {
        None
    }

    /// Send one envelope to the peer.
    ///
    /// # Errors
    /// Returns [`NetworkError`] on framing/encryption/I/O failure or if the
    /// connection is closed.
    async fn send(&self, envelope: Envelope) -> Result<(), NetworkError>;

    /// Receive the next envelope from the peer.
    ///
    /// # Errors
    /// Returns [`NetworkError::Closed`] when the peer closes the link, or
    /// another [`NetworkError`] on failure.
    async fn recv(&self) -> Result<Envelope, NetworkError>;

    /// Close the connection gracefully.
    async fn close(&self) -> Result<(), NetworkError>;
}

/// A transport backend: dials outbound and accepts inbound connections.
#[async_trait]
pub trait Transport: Send + Sync {
    /// Which backend this is.
    fn kind(&self) -> TransportKind;

    /// Dial a peer at `addr`.
    ///
    /// # Errors
    /// Returns [`NetworkError`] if the connection cannot be established.
    async fn connect(&self, addr: SocketAddr) -> Result<Box<dyn Connection>, NetworkError>;

    /// Accept the next inbound connection on the bound listener.
    ///
    /// # Errors
    /// Returns [`NetworkError`] on listener failure.
    async fn accept(&self) -> Result<Box<dyn Connection>, NetworkError>;
}
