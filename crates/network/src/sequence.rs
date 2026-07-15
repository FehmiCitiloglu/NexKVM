//! One monotonic message-id allocator shared by every lane on a connection.

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use nexkvm_crypto::PublicKey;
use nexkvm_protocol::{Envelope, MessageId};
use tokio::sync::Mutex;

use crate::{Connection, NetworkError, TransportKind};

/// Decorates a connection so every outbound envelope receives a unique,
/// monotonically increasing id regardless of which subsystem sent it.
///
/// The send lock deliberately covers id allocation and the underlying send.
/// Besides preventing AEAD nonce reuse, this preserves wire order when input,
/// clipboard, and file-transfer tasks send concurrently.
pub struct SequencedConnection {
    inner: Arc<dyn Connection>,
    next_id: Mutex<Option<u64>>,
}

impl SequencedConnection {
    /// Wrap a shared connection, allocating ids from zero.
    #[must_use]
    pub fn new(inner: Arc<dyn Connection>) -> Self {
        Self::new_starting_at(inner, 0)
    }

    /// Wrap a shared connection, allocating ids from `first_id`.
    ///
    /// Protocol layers that consume an authenticated control id before normal
    /// lane traffic use this constructor to preserve nonce uniqueness.
    #[must_use]
    pub fn new_starting_at(inner: Arc<dyn Connection>, first_id: u64) -> Self {
        Self {
            inner,
            next_id: Mutex::new(Some(first_id)),
        }
    }

    /// Wrap an owned connection, allocating ids from zero.
    #[must_use]
    pub fn from_box(inner: Box<dyn Connection>) -> Self {
        Self::new(Arc::from(inner))
    }

    /// Wrap an owned connection, allocating ids from `first_id`.
    #[must_use]
    pub fn from_box_starting_at(inner: Box<dyn Connection>, first_id: u64) -> Self {
        Self::new_starting_at(Arc::from(inner), first_id)
    }
}

impl std::fmt::Debug for SequencedConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SequencedConnection")
            .field("kind", &self.inner.kind())
            .field("peer_addr", &self.inner.peer_addr())
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Connection for SequencedConnection {
    fn kind(&self) -> TransportKind {
        self.inner.kind()
    }

    fn peer_addr(&self) -> SocketAddr {
        self.inner.peer_addr()
    }

    fn peer_identity(&self) -> Option<PublicKey> {
        self.inner.peer_identity()
    }

    async fn send(&self, mut envelope: Envelope) -> Result<(), NetworkError> {
        let mut next_id = self.next_id.lock().await;
        let id = next_id.take().ok_or(NetworkError::MessageIdExhausted)?;
        *next_id = id.checked_add(1);
        envelope.id = MessageId(id);
        self.inner.send(envelope).await
    }

    async fn recv(&self) -> Result<Envelope, NetworkError> {
        self.inner.recv().await
    }

    async fn close(&self) -> Result<(), NetworkError> {
        self.inner.close().await
    }
}
