use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use nexkvm_crypto::{
    AeadSessionSecurity, CryptoError, DeviceKeypair, IdentitySignature, PublicKey, SessionKeys,
};
use nexkvm_network::{
    Connection, NetworkError, SecureConnection, TransportKind, establish_trusted_session,
    trusted_peer_session_security,
};
use nexkvm_protocol::{Envelope, MessageId, MessageKind, PROTOCOL_VERSION};

fn sessions() -> (Arc<AeadSessionSecurity>, Arc<AeadSessionSecurity>) {
    let (a, b) = SessionKeys::derive_pair(
        b"shared secret from authenticated key agreement",
        b"pairing transcript and peer identity binding",
    )
    .expect("keys");
    (
        Arc::new(AeadSessionSecurity::new(a).expect("session a")),
        Arc::new(AeadSessionSecurity::new(b).expect("session b")),
    )
}

fn input_envelope(id: u64, body: &'static [u8]) -> Envelope {
    Envelope::new(
        PROTOCOL_VERSION,
        MessageId(id),
        MessageKind::Input,
        Bytes::from_static(body),
    )
}

fn handshake_envelope(key: &PublicKey, challenge: [u8; 32]) -> Envelope {
    let mut body = Vec::with_capacity(2 + key.as_bytes().len() + challenge.len());
    body.extend_from_slice(&(key.as_bytes().len() as u16).to_be_bytes());
    body.extend_from_slice(key.as_bytes());
    body.extend_from_slice(&challenge);
    Envelope::new(
        PROTOCOL_VERSION,
        MessageId(0),
        MessageKind::Handshake,
        Bytes::from(body),
    )
}

fn proof_envelope(signature: &IdentitySignature) -> Envelope {
    Envelope::new(
        PROTOCOL_VERSION,
        MessageId(1),
        MessageKind::Handshake,
        Bytes::copy_from_slice(signature.as_bytes()),
    )
}

#[tokio::test]
async fn establish_trusted_session_announces_key_and_wraps_connection() {
    let local = DeviceKeypair::from_seed([1u8; 32]);
    let peer = DeviceKeypair::from_seed([2u8; 32]);
    let local_challenge = [3u8; 32];
    let peer_challenge = [4u8; 32];
    let peer_signature = peer.sign_identity_challenge(&nexkvm_network::trusted_session_transcript(
        &peer.public_key(),
        &local.public_key(),
        peer_challenge,
        local_challenge,
    ));
    let raw = Arc::new(MockConnection::default());
    raw.inbound
        .lock()
        .expect("inbound")
        .push_back(handshake_envelope(&peer.public_key(), peer_challenge));
    raw.inbound
        .lock()
        .expect("inbound")
        .push_back(proof_envelope(&peer_signature));

    let secure = establish_trusted_session(
        Box::new(ArcConnection(raw.clone())),
        local.clone(),
        local_challenge,
        std::slice::from_ref(&peer.public_key()),
    )
    .await
    .expect("trusted session");

    let announced = raw
        .sent
        .lock()
        .expect("sent")
        .pop_front()
        .expect("local handshake");
    assert_eq!(announced.kind, MessageKind::Handshake);
    assert!(
        announced
            .body
            .windows(32)
            .any(|window| window == local.public_key().as_bytes())
    );
    let proof = raw
        .sent
        .lock()
        .expect("sent")
        .pop_front()
        .expect("local proof");
    assert_eq!(proof.kind, MessageKind::Handshake);
    assert_ne!(proof.body, Bytes::new());

    secure
        .send(input_envelope(11, b"secure after handshake"))
        .await
        .expect("secure send");
    let wire = raw
        .sent
        .lock()
        .expect("sent")
        .pop_front()
        .expect("secure wire");
    assert_eq!(wire.kind, MessageKind::Input);
    assert_ne!(wire.body, Bytes::from_static(b"secure after handshake"));
}

#[tokio::test]
async fn establish_trusted_session_rejects_untrusted_peer_key() {
    let local = DeviceKeypair::from_seed([1u8; 32]);
    let peer = DeviceKeypair::from_seed([2u8; 32]);
    let raw = Arc::new(MockConnection::default());
    raw.inbound
        .lock()
        .expect("inbound")
        .push_back(handshake_envelope(&peer.public_key(), [4u8; 32]));

    let error = establish_trusted_session(Box::new(ArcConnection(raw)), local, [3u8; 32], &[])
        .await
        .expect_err("untrusted peer must fail");

    assert!(matches!(
        error,
        NetworkError::Crypto(CryptoError::Untrusted)
    ));
}

#[tokio::test]
async fn establish_trusted_session_rejects_bad_identity_signature() {
    let local = DeviceKeypair::from_seed([1u8; 32]);
    let peer = DeviceKeypair::from_seed([2u8; 32]);
    let attacker = DeviceKeypair::from_seed([9u8; 32]);
    let raw = Arc::new(MockConnection::default());
    raw.inbound
        .lock()
        .expect("inbound")
        .push_back(handshake_envelope(&peer.public_key(), [4u8; 32]));
    raw.inbound
        .lock()
        .expect("inbound")
        .push_back(proof_envelope(
            &attacker.sign_identity_challenge(b"wrong transcript"),
        ));

    let error = establish_trusted_session(
        Box::new(ArcConnection(raw)),
        local,
        [3u8; 32],
        std::slice::from_ref(&peer.public_key()),
    )
    .await
    .expect_err("bad proof must fail");

    assert!(matches!(
        error,
        NetworkError::Crypto(CryptoError::BadSignature)
    ));
}

#[tokio::test]
async fn trusted_peer_session_security_derives_complementary_endpoint_keys() {
    let mac = PublicKey(vec![1; 32]);
    let linux = PublicKey(vec![2; 32]);
    let mac_security = Arc::new(
        trusted_peer_session_security(&mac, &linux).expect("mac trusted session security"),
    );
    let linux_security = Arc::new(
        trusted_peer_session_security(&linux, &mac).expect("linux trusted session security"),
    );
    let raw = Arc::new(MockConnection::default());
    let mac_conn = SecureConnection::new(Box::new(ArcConnection(raw.clone())), mac_security);
    let linux_conn = SecureConnection::new(Box::new(ArcConnection(raw.clone())), linux_security);

    mac_conn
        .send(input_envelope(3, b"trusted input"))
        .await
        .expect("send");
    let wire = raw
        .sent
        .lock()
        .expect("sent")
        .pop_front()
        .expect("wire env");
    raw.inbound.lock().expect("inbound").push_back(wire);

    let opened = linux_conn.recv().await.expect("recv");
    assert_eq!(opened.body, Bytes::from_static(b"trusted input"));
}

#[tokio::test]
async fn secure_connection_seals_and_opens_envelope_bodies() {
    let (a, b) = sessions();
    let raw = Arc::new(MockConnection::default());
    let sender = SecureConnection::new(Box::new(ArcConnection(raw.clone())), a);
    let receiver = SecureConnection::new(Box::new(ArcConnection(raw.clone())), b);

    sender
        .send(input_envelope(7, b"mouse-delta"))
        .await
        .expect("send");

    let wire = raw
        .sent
        .lock()
        .expect("sent")
        .pop_front()
        .expect("wire env");
    assert_eq!(wire.id, MessageId(7));
    assert_eq!(wire.kind, MessageKind::Input);
    assert_ne!(wire.body, Bytes::from_static(b"mouse-delta"));
    raw.inbound.lock().expect("inbound").push_back(wire);

    let opened = receiver.recv().await.expect("recv");
    assert_eq!(opened.id, MessageId(7));
    assert_eq!(opened.kind, MessageKind::Input);
    assert_eq!(opened.body, Bytes::from_static(b"mouse-delta"));
}

#[tokio::test]
async fn secure_connection_rejects_plaintext_envelope_bodies() {
    let (_a, b) = sessions();
    let raw = Arc::new(MockConnection::default());
    raw.inbound
        .lock()
        .expect("inbound")
        .push_back(input_envelope(8, b"plain input"));

    let receiver = SecureConnection::new(Box::new(ArcConnection(raw)), b);
    let error = receiver.recv().await.expect_err("plaintext must fail auth");

    assert!(matches!(error, NetworkError::Crypto(_)));
}

#[tokio::test]
async fn secure_connection_rejects_replayed_wire_envelopes() {
    let (a, b) = sessions();
    let raw = Arc::new(MockConnection::default());
    let sender = SecureConnection::new(Box::new(ArcConnection(raw.clone())), a);
    let receiver = SecureConnection::new(Box::new(ArcConnection(raw.clone())), b);

    sender
        .send(input_envelope(9, b"key-press"))
        .await
        .expect("send");
    let wire = raw
        .sent
        .lock()
        .expect("sent")
        .pop_front()
        .expect("wire env");
    raw.inbound.lock().expect("inbound").push_back(wire.clone());
    raw.inbound.lock().expect("inbound").push_back(wire);

    receiver.recv().await.expect("first recv");
    let error = receiver.recv().await.expect_err("replay must fail");

    assert!(matches!(
        error,
        NetworkError::Crypto(CryptoError::Replay(9))
    ));
}

#[derive(Default)]
struct MockConnection {
    inbound: Mutex<VecDeque<Envelope>>,
    sent: Mutex<VecDeque<Envelope>>,
}

#[derive(Clone)]
struct ArcConnection(Arc<MockConnection>);

#[async_trait]
impl Connection for ArcConnection {
    fn kind(&self) -> TransportKind {
        TransportKind::Tcp
    }

    fn peer_addr(&self) -> SocketAddr {
        "127.0.0.1:47654".parse().expect("addr")
    }

    async fn send(&self, envelope: Envelope) -> Result<(), NetworkError> {
        self.0.sent.lock().expect("sent").push_back(envelope);
        Ok(())
    }

    async fn recv(&self) -> Result<Envelope, NetworkError> {
        self.0
            .inbound
            .lock()
            .expect("inbound")
            .pop_front()
            .ok_or(NetworkError::Closed)
    }

    async fn close(&self) -> Result<(), NetworkError> {
        Ok(())
    }
}
