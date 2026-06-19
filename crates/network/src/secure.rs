//! Application-layer session security wrapper for established connections.

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use nexkvm_crypto::{AeadSessionSecurity, CryptoError, PublicKey, SessionKeys, SessionSecurity};
use nexkvm_protocol::{Envelope, MessageId, MessageKind, PROTOCOL_VERSION};
use sha2::{Digest, Sha256};

use crate::error::NetworkError;
use crate::transport::{Connection, TransportKind};

const TRUSTED_SESSION_SECRET_LABEL: &[u8] = b"nexkvm trusted peer session secret v1";
const TRUSTED_SESSION_CONTEXT_LABEL: &[u8] = b"nexkvm trusted peer session context v1";

/// Derive session security for an already-trusted peer.
///
/// This is the post-trust bootstrap used once both endpoints know the pinned
/// local and peer public keys. Endpoint A is the lexicographically smaller key,
/// so each side independently selects the complementary tx/rx key direction.
///
/// # Errors
/// Returns [`CryptoError::KeyExchange`] when both keys are identical or key
/// derivation fails.
pub fn trusted_peer_session_security(
    local: &PublicKey,
    peer: &PublicKey,
) -> Result<AeadSessionSecurity, CryptoError> {
    if local == peer {
        return Err(CryptoError::KeyExchange(
            "trusted peer session requires distinct public keys".into(),
        ));
    }

    let (a, b, local_is_a) = if local.as_bytes() < peer.as_bytes() {
        (local.as_bytes(), peer.as_bytes(), true)
    } else {
        (peer.as_bytes(), local.as_bytes(), false)
    };

    let mut secret = Sha256::new();
    secret.update(TRUSTED_SESSION_SECRET_LABEL);
    secret.update(a);
    secret.update(b);
    let shared_secret = secret.finalize();

    let mut context = Sha256::new();
    context.update(TRUSTED_SESSION_CONTEXT_LABEL);
    context.update(a);
    context.update(b);
    let context = context.finalize();

    let (endpoint_a, endpoint_b) = SessionKeys::derive_pair(&shared_secret, &context)?;
    let keys = if local_is_a { endpoint_a } else { endpoint_b };
    AeadSessionSecurity::new(keys)
}

/// Exchange pinned public keys and wrap a trusted connection with session AEAD.
///
/// The handshake is intentionally small: both sides send their local public key
/// as a plaintext `Handshake` envelope, receive the peer key, verify it is
/// pinned in the local trust set, then derive complementary session keys.
///
/// # Errors
/// Returns [`NetworkError::Crypto`] if the peer key is not trusted or session
/// key derivation fails, and propagates transport errors from the underlying
/// connection.
pub async fn establish_trusted_session(
    inner: Box<dyn Connection>,
    local: PublicKey,
    trusted_peers: &[PublicKey],
) -> Result<SecureConnection, NetworkError> {
    let local_hello = Envelope::new(
        PROTOCOL_VERSION,
        MessageId(0),
        MessageKind::Handshake,
        Bytes::copy_from_slice(local.as_bytes()),
    );
    inner.send(local_hello).await?;

    let peer_hello = inner.recv().await?;
    if peer_hello.kind != MessageKind::Handshake {
        return Err(CryptoError::KeyExchange("expected trusted session handshake".into()).into());
    }

    let peer = PublicKey(peer_hello.body.to_vec());
    if !trusted_peers.iter().any(|trusted| trusted == &peer) {
        return Err(CryptoError::Untrusted.into());
    }

    let security = trusted_peer_session_security(&local, &peer)?;
    Ok(SecureConnection::new(inner, Arc::new(security)))
}

/// A connection wrapper that encrypts and authenticates envelope bodies.
///
/// The underlying transport still owns framing and I/O. This layer keeps routing
/// metadata visible (`id`, `kind`, protocol version) while sealing the opaque
/// payload bytes with the established session security context.
pub struct SecureConnection {
    inner: Box<dyn Connection>,
    security: Arc<dyn SessionSecurity>,
}

impl SecureConnection {
    /// Wrap an established transport connection with session security.
    #[must_use]
    pub fn new(inner: Box<dyn Connection>, security: Arc<dyn SessionSecurity>) -> Self {
        Self { inner, security }
    }
}

impl std::fmt::Debug for SecureConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecureConnection")
            .field("kind", &self.inner.kind())
            .field("peer_addr", &self.inner.peer_addr())
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Connection for SecureConnection {
    fn kind(&self) -> TransportKind {
        self.inner.kind()
    }

    fn peer_addr(&self) -> SocketAddr {
        self.inner.peer_addr()
    }

    async fn send(&self, mut envelope: Envelope) -> Result<(), NetworkError> {
        let sealed = self.security.seal(envelope.id.0, &envelope.body)?;
        envelope.body = Bytes::from(sealed);
        self.inner.send(envelope).await
    }

    async fn recv(&self) -> Result<Envelope, NetworkError> {
        let mut envelope = self.inner.recv().await?;
        let opened = self.security.open(envelope.id.0, &envelope.body)?;
        envelope.body = Bytes::from(opened);
        Ok(envelope)
    }

    async fn close(&self) -> Result<(), NetworkError> {
        self.inner.close().await
    }
}
