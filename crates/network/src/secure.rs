//! Application-layer session security wrapper for established connections.

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::{Buf, BufMut, Bytes, BytesMut};
use nexkvm_crypto::{
    AeadSessionSecurity, CryptoError, DeviceKeypair, EphemeralKeyAgreement, EphemeralPublicKey,
    IdentitySignature, PublicKey, SessionKeys, SessionSecurity, verify_identity_signature,
};
use nexkvm_protocol::{
    Envelope, MessageId, MessageKind, PROTOCOL_VERSION, ProtocolError, VersionRange,
};

use crate::error::NetworkError;
use crate::transport::{Connection, TransportKind};

const TRUSTED_SESSION_CONTEXT_LABEL: &[u8] = b"nexkvm trusted peer session context v2";
const TRUSTED_SESSION_TRANSCRIPT_LABEL: &[u8] = b"nexkvm trusted peer proof v2";
const HANDSHAKE_HELLO_MAGIC: &[u8; 4] = b"NXH2";
const HANDSHAKE_CHALLENGE_LEN: usize = 32;
const EPHEMERAL_KEY_LEN: usize = 32;
const SECURE_BODY_HEADER_LEN: usize = 6;
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

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
    shared_secret: &[u8],
    context: &[u8],
) -> Result<AeadSessionSecurity, CryptoError> {
    if local == peer {
        return Err(CryptoError::KeyExchange(
            "trusted peer session requires distinct public keys".into(),
        ));
    }

    let local_is_a = local.as_bytes() < peer.as_bytes();
    let (endpoint_a, endpoint_b) = SessionKeys::derive_pair(shared_secret, context)?;
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
    signer_ephemeral: EphemeralPublicKey,
    verifier_ephemeral: EphemeralPublicKey,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(192);
    out.extend_from_slice(TRUSTED_SESSION_TRANSCRIPT_LABEL);
    out.extend_from_slice(&PROTOCOL_VERSION.major.to_be_bytes());
    out.extend_from_slice(&PROTOCOL_VERSION.minor.to_be_bytes());
    append_length_prefixed(&mut out, signer.as_bytes());
    append_length_prefixed(&mut out, verifier.as_bytes());
    out.extend_from_slice(&signer_challenge);
    out.extend_from_slice(&verifier_challenge);
    out.extend_from_slice(signer_ephemeral.as_bytes());
    out.extend_from_slice(verifier_ephemeral.as_bytes());
    out
}

/// Symmetric HKDF context binding identities, fresh challenges, ephemeral keys,
/// endpoint ordering, and the protocol version.
#[must_use]
pub fn trusted_session_context(
    local: &PublicKey,
    peer: &PublicKey,
    local_challenge: [u8; HANDSHAKE_CHALLENGE_LEN],
    peer_challenge: [u8; HANDSHAKE_CHALLENGE_LEN],
    local_ephemeral: EphemeralPublicKey,
    peer_ephemeral: EphemeralPublicKey,
) -> Vec<u8> {
    let (a_key, a_challenge, a_ephemeral, b_key, b_challenge, b_ephemeral) =
        if local.as_bytes() < peer.as_bytes() {
            (
                local,
                local_challenge,
                local_ephemeral,
                peer,
                peer_challenge,
                peer_ephemeral,
            )
        } else {
            (
                peer,
                peer_challenge,
                peer_ephemeral,
                local,
                local_challenge,
                local_ephemeral,
            )
        };
    let mut out = Vec::with_capacity(192);
    out.extend_from_slice(TRUSTED_SESSION_CONTEXT_LABEL);
    out.extend_from_slice(&PROTOCOL_VERSION.major.to_be_bytes());
    out.extend_from_slice(&PROTOCOL_VERSION.minor.to_be_bytes());
    append_length_prefixed(&mut out, a_key.as_bytes());
    append_length_prefixed(&mut out, b_key.as_bytes());
    out.extend_from_slice(&a_challenge);
    out.extend_from_slice(&b_challenge);
    out.extend_from_slice(a_ephemeral.as_bytes());
    out.extend_from_slice(b_ephemeral.as_bytes());
    out
}

/// Exchange pinned public keys and wrap a trusted connection with session AEAD.
///
/// Both sides exchange a version-2 hello containing their identity key, fresh
/// challenge, and ephemeral X25519 key. They verify the peer is pinned, sign
/// the complete transcript, and derive complementary per-session keys.
///
/// # Errors
/// Returns [`NetworkError::Crypto`] if the peer key is not trusted or session
/// key derivation fails, and propagates transport errors from the underlying
/// connection.
pub async fn establish_trusted_session(
    inner: Box<dyn Connection>,
    local: DeviceKeypair,
    trusted_peers: &[PublicKey],
) -> Result<SecureConnection, NetworkError> {
    let mut local_challenge = [0u8; HANDSHAKE_CHALLENGE_LEN];
    getrandom::fill(&mut local_challenge).map_err(|error| {
        CryptoError::KeyExchange(format!("handshake random generation failed: {error}"))
    })?;
    let local_ephemeral = EphemeralKeyAgreement::generate()?;
    establish_trusted_session_with_material(
        inner,
        local,
        local_challenge,
        local_ephemeral,
        trusted_peers,
    )
    .await
}

/// Deterministic-material entry point for protocol tests. Production callers
/// must use [`establish_trusted_session`] so every connection gets fresh CSPRNG
/// material.
#[doc(hidden)]
pub async fn establish_trusted_session_with_material(
    inner: Box<dyn Connection>,
    local: DeviceKeypair,
    local_challenge: [u8; HANDSHAKE_CHALLENGE_LEN],
    local_ephemeral: EphemeralKeyAgreement,
    trusted_peers: &[PublicKey],
) -> Result<SecureConnection, NetworkError> {
    tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        establish_trusted_session_inner(
            inner,
            local,
            local_challenge,
            local_ephemeral,
            trusted_peers,
        ),
    )
    .await
    .map_err(|_| NetworkError::Timeout)?
}

async fn establish_trusted_session_inner(
    inner: Box<dyn Connection>,
    local: DeviceKeypair,
    local_challenge: [u8; HANDSHAKE_CHALLENGE_LEN],
    local_ephemeral: EphemeralKeyAgreement,
    trusted_peers: &[PublicKey],
) -> Result<SecureConnection, NetworkError> {
    let local_public_key = local.public_key();
    let local_ephemeral_public = local_ephemeral.public_key();
    let local_hello = Envelope::new(
        PROTOCOL_VERSION,
        MessageId(0),
        MessageKind::Handshake,
        Bytes::from(encode_handshake_hello(
            &local_public_key,
            local_challenge,
            local_ephemeral_public,
        )),
    );
    inner.send(local_hello).await?;

    let peer_hello = inner.recv().await?;
    if peer_hello.kind != MessageKind::Handshake {
        return Err(CryptoError::KeyExchange("expected trusted session handshake".into()).into());
    }
    if peer_hello.id != MessageId(0) {
        return Err(CryptoError::KeyExchange("invalid trusted session hello id".into()).into());
    }

    if VersionRange::current()
        .negotiate(peer_hello.version)
        .is_none()
    {
        return Err(ProtocolError::IncompatibleVersion {
            peer: peer_hello.version,
            supported: VersionRange::current(),
        }
        .into());
    }

    let (peer, peer_challenge, peer_ephemeral) = decode_handshake_hello(&peer_hello.body)?;
    if !trusted_peers.iter().any(|trusted| trusted == &peer) {
        return Err(CryptoError::Untrusted.into());
    }

    let local_transcript = trusted_session_transcript(
        &local_public_key,
        &peer,
        local_challenge,
        peer_challenge,
        local_ephemeral_public,
        peer_ephemeral,
    );
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
    if peer_proof.id != MessageId(1) {
        return Err(CryptoError::KeyExchange("invalid trusted session proof id".into()).into());
    }
    if VersionRange::current()
        .negotiate(peer_proof.version)
        .is_none()
    {
        return Err(ProtocolError::IncompatibleVersion {
            peer: peer_proof.version,
            supported: VersionRange::current(),
        }
        .into());
    }
    let peer_transcript = trusted_session_transcript(
        &peer,
        &local_public_key,
        peer_challenge,
        local_challenge,
        peer_ephemeral,
        local_ephemeral_public,
    );
    verify_identity_signature(
        &peer,
        &peer_transcript,
        &IdentitySignature(peer_proof.body.to_vec()),
    )?;

    let shared_secret = local_ephemeral.agree(peer_ephemeral)?;
    let context = trusted_session_context(
        &local_public_key,
        &peer,
        local_challenge,
        peer_challenge,
        local_ephemeral_public,
        peer_ephemeral,
    );
    let security = trusted_peer_session_security(
        &local_public_key,
        &peer,
        shared_secret.as_bytes(),
        &context,
    )?;
    Ok(SecureConnection::new_authenticated(
        inner,
        Arc::new(security),
        peer,
    ))
}

fn encode_handshake_hello(
    public_key: &PublicKey,
    challenge: [u8; HANDSHAKE_CHALLENGE_LEN],
    ephemeral: EphemeralPublicKey,
) -> Vec<u8> {
    let key = public_key.as_bytes();
    let mut out = Vec::with_capacity(
        HANDSHAKE_HELLO_MAGIC.len() + 2 + key.len() + challenge.len() + EPHEMERAL_KEY_LEN,
    );
    out.extend_from_slice(HANDSHAKE_HELLO_MAGIC);
    out.extend_from_slice(&(key.len() as u16).to_be_bytes());
    out.extend_from_slice(key);
    out.extend_from_slice(&challenge);
    out.extend_from_slice(ephemeral.as_bytes());
    out
}

fn decode_handshake_hello(
    body: &[u8],
) -> Result<(PublicKey, [u8; HANDSHAKE_CHALLENGE_LEN], EphemeralPublicKey), CryptoError> {
    if body.len() < HANDSHAKE_HELLO_MAGIC.len() + 2 + HANDSHAKE_CHALLENGE_LEN + EPHEMERAL_KEY_LEN
        || &body[..HANDSHAKE_HELLO_MAGIC.len()] != HANDSHAKE_HELLO_MAGIC
    {
        return Err(CryptoError::KeyExchange(
            "invalid trusted session hello".into(),
        ));
    }
    let length_start = HANDSHAKE_HELLO_MAGIC.len();
    let key_len = u16::from_be_bytes([body[length_start], body[length_start + 1]]) as usize;
    let expected =
        HANDSHAKE_HELLO_MAGIC.len() + 2 + key_len + HANDSHAKE_CHALLENGE_LEN + EPHEMERAL_KEY_LEN;
    if body.len() != expected {
        return Err(CryptoError::KeyExchange(
            "invalid trusted session hello".into(),
        ));
    }
    let key_start = HANDSHAKE_HELLO_MAGIC.len() + 2;
    let challenge_start = key_start + key_len;
    let ephemeral_start = challenge_start + HANDSHAKE_CHALLENGE_LEN;
    let mut challenge = [0u8; HANDSHAKE_CHALLENGE_LEN];
    challenge.copy_from_slice(&body[challenge_start..ephemeral_start]);
    let mut ephemeral = [0u8; EPHEMERAL_KEY_LEN];
    ephemeral.copy_from_slice(&body[ephemeral_start..]);
    Ok((
        PublicKey(body[key_start..challenge_start].to_vec()),
        challenge,
        EphemeralPublicKey::from_bytes(ephemeral),
    ))
}

fn append_length_prefixed(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = u16::try_from(bytes.len()).unwrap_or(u16::MAX);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&bytes[..usize::from(len)]);
}

/// A connection wrapper that encrypts and authenticates envelope bodies.
///
/// The underlying transport still owns framing and I/O. This layer keeps routing
/// metadata visible (`id`, `kind`, protocol version) while sealing the opaque
/// payload bytes with the established session security context.
pub struct SecureConnection {
    inner: Box<dyn Connection>,
    security: Arc<dyn SessionSecurity>,
    peer_identity: Option<PublicKey>,
}

impl SecureConnection {
    /// Wrap an established transport connection with session security.
    #[must_use]
    pub fn new(inner: Box<dyn Connection>, security: Arc<dyn SessionSecurity>) -> Self {
        Self {
            inner,
            security,
            peer_identity: None,
        }
    }

    fn new_authenticated(
        inner: Box<dyn Connection>,
        security: Arc<dyn SessionSecurity>,
        peer_identity: PublicKey,
    ) -> Self {
        Self {
            inner,
            security,
            peer_identity: Some(peer_identity),
        }
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

    fn peer_identity(&self) -> Option<PublicKey> {
        self.peer_identity.clone()
    }

    async fn send(&self, mut envelope: Envelope) -> Result<(), NetworkError> {
        let bound = encode_secure_body(&envelope);
        let sealed = self.security.seal(envelope.id.0, &bound)?;
        envelope.body = Bytes::from(sealed);
        self.inner.send(envelope).await
    }

    async fn recv(&self) -> Result<Envelope, NetworkError> {
        let mut envelope = self.inner.recv().await?;
        let opened = self.security.open(envelope.id.0, &envelope.body)?;
        envelope.body = decode_secure_body(&envelope, Bytes::from(opened))?;
        Ok(envelope)
    }

    async fn close(&self) -> Result<(), NetworkError> {
        self.inner.close().await
    }
}

fn encode_secure_body(envelope: &Envelope) -> Bytes {
    let mut out = BytesMut::with_capacity(SECURE_BODY_HEADER_LEN + envelope.body.len());
    out.put_u16(envelope.version.major);
    out.put_u16(envelope.version.minor);
    out.put_u16(envelope.kind as u16);
    out.put_slice(&envelope.body);
    out.freeze()
}

fn decode_secure_body(envelope: &Envelope, mut opened: Bytes) -> Result<Bytes, NetworkError> {
    if opened.remaining() < SECURE_BODY_HEADER_LEN {
        return Err(CryptoError::BadSignature.into());
    }
    let major = opened.get_u16();
    let minor = opened.get_u16();
    let kind = opened.get_u16();
    if major != envelope.version.major
        || minor != envelope.version.minor
        || kind != envelope.kind as u16
    {
        return Err(CryptoError::BadSignature.into());
    }
    Ok(opened)
}
