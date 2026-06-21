//! Application-layer session security wrapper for established connections.

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use nexkvm_crypto::{
    AeadSessionSecurity, CryptoError, DeviceKeypair, IdentitySignature, PublicKey, SessionKeys,
    SessionSecurity, verify_identity_signature,
};
use nexkvm_protocol::{Envelope, MessageId, MessageKind, PROTOCOL_VERSION};
use sha2::{Digest, Sha256};

use crate::error::NetworkError;
use crate::transport::{Connection, TransportKind};

const TRUSTED_SESSION_SECRET_LABEL: &[u8] = b"nexkvm trusted peer session secret v1";
const TRUSTED_SESSION_CONTEXT_LABEL: &[u8] = b"nexkvm trusted peer session context v1";
const HANDSHAKE_CHALLENGE_LEN: usize = 32;

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

/// Build the signed transcript for one side of a trusted session handshake.
#[must_use]
pub fn trusted_session_transcript(
    signer: &PublicKey,
    verifier: &PublicKey,
    signer_challenge: [u8; HANDSHAKE_CHALLENGE_LEN],
    verifier_challenge: [u8; HANDSHAKE_CHALLENGE_LEN],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(
        TRUSTED_SESSION_CONTEXT_LABEL.len()
            + signer.as_bytes().len()
            + verifier.as_bytes().len()
            + HANDSHAKE_CHALLENGE_LEN * 2,
    );
    out.extend_from_slice(TRUSTED_SESSION_CONTEXT_LABEL);
    out.extend_from_slice(signer.as_bytes());
    out.extend_from_slice(verifier.as_bytes());
    out.extend_from_slice(&signer_challenge);
    out.extend_from_slice(&verifier_challenge);
    out
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
    local: DeviceKeypair,
    local_challenge: [u8; HANDSHAKE_CHALLENGE_LEN],
    trusted_peers: &[PublicKey],
) -> Result<SecureConnection, NetworkError> {
    let local_public_key = local.public_key();
    let local_hello = Envelope::new(
        PROTOCOL_VERSION,
        MessageId(0),
        MessageKind::Handshake,
        Bytes::from(encode_handshake_hello(&local_public_key, local_challenge)),
    );
    inner.send(local_hello).await?;

    let peer_hello = inner.recv().await?;
    if peer_hello.kind != MessageKind::Handshake {
        return Err(CryptoError::KeyExchange("expected trusted session handshake".into()).into());
    }

    let (peer, peer_challenge) = decode_handshake_hello(&peer_hello.body)?;
    if !trusted_peers.iter().any(|trusted| trusted == &peer) {
        return Err(CryptoError::Untrusted.into());
    }

    let local_transcript =
        trusted_session_transcript(&local_public_key, &peer, local_challenge, peer_challenge);
    let local_signature = local.sign_identity_challenge(&local_transcript);
    let local_proof = Envelope::new(
        PROTOCOL_VERSION,
        MessageId(1),
        MessageKind::Handshake,
        Bytes::copy_from_slice(local_signature.as_bytes()),
    );
    inner.send(local_proof).await?;

    let peer_proof = inner.recv().await?;
    if peer_proof.kind != MessageKind::Handshake {
        return Err(CryptoError::KeyExchange("expected trusted session proof".into()).into());
    }
    let peer_transcript =
        trusted_session_transcript(&peer, &local_public_key, peer_challenge, local_challenge);
    verify_identity_signature(
        &peer,
        &peer_transcript,
        &IdentitySignature(peer_proof.body.to_vec()),
    )?;

    let security = trusted_peer_session_security(&local_public_key, &peer)?;
    Ok(SecureConnection::new(inner, Arc::new(security)))
}

fn encode_handshake_hello(
    public_key: &PublicKey,
    challenge: [u8; HANDSHAKE_CHALLENGE_LEN],
) -> Vec<u8> {
    let key = public_key.as_bytes();
    let mut out = Vec::with_capacity(2 + key.len() + challenge.len());
    out.extend_from_slice(&(key.len() as u16).to_be_bytes());
    out.extend_from_slice(key);
    out.extend_from_slice(&challenge);
    out
}

fn decode_handshake_hello(
    body: &[u8],
) -> Result<(PublicKey, [u8; HANDSHAKE_CHALLENGE_LEN]), CryptoError> {
    if body.len() < 2 + HANDSHAKE_CHALLENGE_LEN {
        return Err(CryptoError::KeyExchange(
            "truncated trusted session hello".into(),
        ));
    }
    let key_len = u16::from_be_bytes([body[0], body[1]]) as usize;
    let expected = 2 + key_len + HANDSHAKE_CHALLENGE_LEN;
    if body.len() != expected {
        return Err(CryptoError::KeyExchange(
            "invalid trusted session hello".into(),
        ));
    }
    let key_start = 2;
    let challenge_start = key_start + key_len;
    let mut challenge = [0u8; HANDSHAKE_CHALLENGE_LEN];
    challenge.copy_from_slice(&body[challenge_start..]);
    Ok((
        PublicKey(body[key_start..challenge_start].to_vec()),
        challenge,
    ))
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
