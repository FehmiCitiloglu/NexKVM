use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use nexkvm_crypto::{
    AeadSessionSecurity, CryptoError, DeviceKeypair, EphemeralKeyAgreement, EphemeralPublicKey,
    IdentitySignature, PublicKey, SessionKeys,
};
use nexkvm_network::{
    Connection, NetworkError, SecureConnection, TransportKind, establish_trusted_session,
    establish_trusted_session_with_material, trusted_peer_session_security,
    trusted_session_context,
};
use nexkvm_protocol::{Envelope, MessageId, MessageKind, PROTOCOL_VERSION, ProtocolVersion};

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

fn handshake_envelope(
    key: &PublicKey,
    challenge: [u8; 32],
    ephemeral: EphemeralPublicKey,
) -> Envelope {
    let mut body = Vec::with_capacity(4 + 2 + key.as_bytes().len() + challenge.len() + 32);
    body.extend_from_slice(b"NXH2");
    body.extend_from_slice(&(key.as_bytes().len() as u16).to_be_bytes());
    body.extend_from_slice(key.as_bytes());
    body.extend_from_slice(&challenge);
    body.extend_from_slice(ephemeral.as_bytes());
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
    let local_ephemeral = EphemeralKeyAgreement::from_secret([5u8; 32]);
    let peer_ephemeral = EphemeralKeyAgreement::from_secret([6u8; 32]);
    let peer_signature = peer.sign_identity_challenge(&nexkvm_network::trusted_session_transcript(
        &peer.public_key(),
        &local.public_key(),
        peer_challenge,
        local_challenge,
        peer_ephemeral.public_key(),
        local_ephemeral.public_key(),
    ));
    let raw = Arc::new(MockConnection::default());
    raw.inbound
        .lock()
        .expect("inbound")
        .push_back(handshake_envelope(
            &peer.public_key(),
            peer_challenge,
            peer_ephemeral.public_key(),
        ));
    raw.inbound
        .lock()
        .expect("inbound")
        .push_back(proof_envelope(&peer_signature));

    let secure = establish_trusted_session_with_material(
        Box::new(ArcConnection(raw.clone())),
        local.clone(),
        local_challenge,
        local_ephemeral,
        std::slice::from_ref(&peer.public_key()),
    )
    .await
    .expect("trusted session");
    assert_eq!(secure.peer_identity(), Some(peer.public_key()));

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
    let peer_ephemeral = EphemeralKeyAgreement::from_secret([6u8; 32]);
    let raw = Arc::new(MockConnection::default());
    raw.inbound
        .lock()
        .expect("inbound")
        .push_back(handshake_envelope(
            &peer.public_key(),
            [4u8; 32],
            peer_ephemeral.public_key(),
        ));

    let error = establish_trusted_session(Box::new(ArcConnection(raw)), local, &[])
        .await
        .expect_err("untrusted peer must fail");

    assert!(matches!(
        error,
        NetworkError::Crypto(CryptoError::Untrusted)
    ));
}

#[tokio::test]
async fn establish_trusted_session_rejects_legacy_v1_hello_before_parsing_it() {
    let local = DeviceKeypair::from_seed([1u8; 32]);
    let peer = DeviceKeypair::from_seed([2u8; 32]);
    let key = peer.public_key();
    let mut legacy_body = Vec::with_capacity(2 + key.as_bytes().len() + 32);
    legacy_body.extend_from_slice(&(key.as_bytes().len() as u16).to_be_bytes());
    legacy_body.extend_from_slice(key.as_bytes());
    legacy_body.extend_from_slice(&[4u8; 32]);

    let raw = Arc::new(MockConnection::default());
    raw.inbound.lock().unwrap().push_back(Envelope::new(
        ProtocolVersion { major: 1, minor: 0 },
        MessageId(0),
        MessageKind::Handshake,
        Bytes::from(legacy_body),
    ));

    let error = establish_trusted_session(
        Box::new(ArcConnection(raw)),
        local,
        std::slice::from_ref(&key),
    )
    .await
    .expect_err("legacy v1 hello must require a coordinated upgrade");

    assert!(matches!(error, NetworkError::Protocol(_)));
    assert!(error.to_string().contains("incompatible protocol version"));
}

#[tokio::test]
async fn establish_trusted_session_rejects_bad_identity_signature() {
    let local = DeviceKeypair::from_seed([1u8; 32]);
    let peer = DeviceKeypair::from_seed([2u8; 32]);
    let attacker = DeviceKeypair::from_seed([9u8; 32]);
    let local_ephemeral = EphemeralKeyAgreement::from_secret([5u8; 32]);
    let peer_ephemeral = EphemeralKeyAgreement::from_secret([6u8; 32]);
    let raw = Arc::new(MockConnection::default());
    raw.inbound
        .lock()
        .expect("inbound")
        .push_back(handshake_envelope(
            &peer.public_key(),
            [4u8; 32],
            peer_ephemeral.public_key(),
        ));
    raw.inbound
        .lock()
        .expect("inbound")
        .push_back(proof_envelope(
            &attacker.sign_identity_challenge(b"wrong transcript"),
        ));

    let error = establish_trusted_session_with_material(
        Box::new(ArcConnection(raw)),
        local,
        [3u8; 32],
        local_ephemeral,
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
async fn establish_trusted_session_rejects_incompatible_proof_metadata() {
    let local = DeviceKeypair::from_seed([1u8; 32]);
    let peer = DeviceKeypair::from_seed([2u8; 32]);
    let local_challenge = [3u8; 32];
    let peer_challenge = [4u8; 32];
    let local_ephemeral = EphemeralKeyAgreement::from_secret([5u8; 32]);
    let peer_ephemeral = EphemeralKeyAgreement::from_secret([6u8; 32]);
    let peer_signature = peer.sign_identity_challenge(&nexkvm_network::trusted_session_transcript(
        &peer.public_key(),
        &local.public_key(),
        peer_challenge,
        local_challenge,
        peer_ephemeral.public_key(),
        local_ephemeral.public_key(),
    ));
    let raw = Arc::new(MockConnection::default());
    raw.inbound.lock().unwrap().push_back(handshake_envelope(
        &peer.public_key(),
        peer_challenge,
        peer_ephemeral.public_key(),
    ));
    let mut proof = proof_envelope(&peer_signature);
    proof.version.major += 1;
    raw.inbound.lock().unwrap().push_back(proof);

    let error = establish_trusted_session_with_material(
        Box::new(ArcConnection(raw)),
        local,
        local_challenge,
        local_ephemeral,
        std::slice::from_ref(&peer.public_key()),
    )
    .await
    .expect_err("incompatible proof metadata must fail");

    assert!(matches!(error, NetworkError::Protocol(_)));
}

#[tokio::test]
async fn trusted_peer_session_security_derives_complementary_endpoint_keys() {
    let mac = PublicKey(vec![1; 32]);
    let linux = PublicKey(vec![2; 32]);
    let mac_ephemeral = EphemeralKeyAgreement::from_secret([7; 32]);
    let linux_ephemeral = EphemeralKeyAgreement::from_secret([8; 32]);
    let mac_shared = mac_ephemeral.agree(linux_ephemeral.public_key()).unwrap();
    let linux_shared = linux_ephemeral.agree(mac_ephemeral.public_key()).unwrap();
    let context = trusted_session_context(
        &mac,
        &linux,
        [3; 32],
        [4; 32],
        mac_ephemeral.public_key(),
        linux_ephemeral.public_key(),
    );
    let mac_security = Arc::new(
        trusted_peer_session_security(&mac, &linux, mac_shared.as_bytes(), &context)
            .expect("mac trusted session security"),
    );
    let linux_security = Arc::new(
        trusted_peer_session_security(&linux, &mac, linux_shared.as_bytes(), &context)
            .expect("linux trusted session security"),
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
async fn fresh_ephemeral_handshakes_change_ciphertext_for_the_same_message_id() {
    let mac = PublicKey(vec![1; 32]);
    let linux = PublicKey(vec![2; 32]);
    let peer_ephemeral = EphemeralKeyAgreement::from_secret([9; 32]);

    let ciphertext_for = |local_seed: [u8; 32], challenge: [u8; 32]| {
        let local_ephemeral = EphemeralKeyAgreement::from_secret(local_seed);
        let shared = local_ephemeral.agree(peer_ephemeral.public_key()).unwrap();
        let context = trusted_session_context(
            &mac,
            &linux,
            challenge,
            [4; 32],
            local_ephemeral.public_key(),
            peer_ephemeral.public_key(),
        );
        Arc::new(trusted_peer_session_security(&mac, &linux, shared.as_bytes(), &context).unwrap())
    };

    let raw_first = Arc::new(MockConnection::default());
    let raw_second = Arc::new(MockConnection::default());
    let first = SecureConnection::new(
        Box::new(ArcConnection(raw_first.clone())),
        ciphertext_for([10; 32], [11; 32]),
    );
    let second = SecureConnection::new(
        Box::new(ArcConnection(raw_second.clone())),
        ciphertext_for([12; 32], [13; 32]),
    );

    first
        .send(input_envelope(0, b"same plaintext"))
        .await
        .unwrap();
    second
        .send(input_envelope(0, b"same plaintext"))
        .await
        .unwrap();

    let first_wire = raw_first.sent.lock().unwrap().pop_front().unwrap();
    let second_wire = raw_second.sent.lock().unwrap().pop_front().unwrap();
    assert_ne!(first_wire.body, second_wire.body);
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

#[tokio::test]
async fn secure_connection_authenticates_message_kind_metadata() {
    let (a, b) = sessions();
    let raw = Arc::new(MockConnection::default());
    let sender = SecureConnection::new(Box::new(ArcConnection(raw.clone())), a);
    let receiver = SecureConnection::new(Box::new(ArcConnection(raw.clone())), b);

    sender
        .send(input_envelope(15, b"must remain input"))
        .await
        .unwrap();
    let mut wire = raw.sent.lock().unwrap().pop_front().unwrap();
    wire.kind = MessageKind::Clipboard;
    raw.inbound.lock().unwrap().push_back(wire);

    let error = receiver.recv().await.expect_err("kind tampering must fail");
    assert!(matches!(
        error,
        NetworkError::Crypto(CryptoError::BadSignature)
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
