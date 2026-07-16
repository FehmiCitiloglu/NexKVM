//! User-confirmed pairing exchange over an established transport connection.
//!
//! The transport exchange has three phases: identity/code exchange, explicit
//! approval exchange, and persistence-status exchange. Pinning the peer remains
//! a storage/runtime responsibility.

use std::time::{Duration, Instant};

use bytes::Bytes;
use nexkvm_crypto::{
    ConfirmationCode, DeviceIdentity, PairingBootstrap, PairingMethod, PairingRequest,
    PairingResponse, PairingSession,
};
use nexkvm_protocol::{
    Envelope, MessageId, MessageKind, PROTOCOL_VERSION, ProtocolError, VersionRange,
};
use serde::{Deserialize, Serialize};

use crate::{Connection, NetworkError};

const IDENTITY_EXCHANGE_ID: MessageId = MessageId(0);
const APPROVAL_EXCHANGE_ID: MessageId = MessageId(1);
const PERSISTENCE_EXCHANGE_ID: MessageId = MessageId(2);
const PAIRING_MESSAGE_TIMEOUT: Duration = Duration::from_secs(130);
const DEVICE_IDENTITY_KEY_BYTES: usize = 32;
const MAX_DEVICE_DISPLAY_NAME_BYTES: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingConfirmationPrompt {
    /// Peer identity received over the transport.
    pub peer: DeviceIdentity,
    /// Short code the local user must compare with the peer's screen.
    pub code: ConfirmationCode,
    /// TCP port on which the peer accepts future trusted sessions.
    pub peer_listen_port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum PairingHandshakeMessage {
    Request {
        request: PairingRequest,
        listen_port: u16,
    },
    Response {
        response: PairingResponse,
        listen_port: u16,
    },
    Approval {
        accepted: bool,
    },
    Persistence {
        succeeded: bool,
    },
}

/// Send an initiator pairing request and wait for the responder identity.
///
/// The returned prompt is not trust. The caller must show `code` to the local
/// user and only continue to trust-store persistence after explicit approval.
///
/// # Errors
/// Returns [`NetworkError`] if the session is not an initiator, the peer sends a
/// non-pairing message, the payload is malformed, or the pairing token expired.
pub async fn initiate_pairing_handshake(
    connection: &dyn Connection,
    session: &PairingSession,
    method: PairingMethod,
    local_listen_port: u16,
    now: Instant,
) -> Result<PairingConfirmationPrompt, NetworkError> {
    validate_listen_port(local_listen_port)?;
    let bootstrap = session
        .bootstrap()
        .ok_or_else(|| NetworkError::Pairing("initiator session required".into()))?;
    let local_identity = DeviceIdentity {
        display_name: bootstrap.display_name,
        public_key: bootstrap.public_key,
    };
    validate_pairing_identity(&local_identity)?;
    let request = PairingRequest {
        identity: local_identity.clone(),
        method,
        nonce: bootstrap.nonce,
    };
    send_pairing_message(
        connection,
        IDENTITY_EXCHANGE_ID,
        &PairingHandshakeMessage::Request {
            request,
            listen_port: local_listen_port,
        },
    )
    .await?;

    let (response, peer_listen_port) =
        match recv_pairing_message(connection, IDENTITY_EXCHANGE_ID).await? {
            PairingHandshakeMessage::Response {
                response,
                listen_port,
            } => (response, listen_port),
            _ => return Err(NetworkError::Pairing("expected pairing response".into())),
        };
    validate_listen_port(peer_listen_port)?;
    validate_pairing_identities(&local_identity, &response.identity)?;

    if response.confirmed {
        return Err(NetworkError::Pairing(
            "peer confirmed before local user comparison".into(),
        ));
    }

    let code = session.confirmation_code(&response.identity.public_key, now)?;
    Ok(PairingConfirmationPrompt {
        peer: response.identity,
        code,
        peer_listen_port,
    })
}

/// Receive an initiator request, answer with `local`, and return the responder
/// session plus the local confirmation prompt.
///
/// The responder session is kept in `AwaitingConfirmation`; callers must require
/// explicit user approval before accepting and persisting trust.
///
/// # Errors
/// Returns [`NetworkError`] if the peer sends a non-pairing message, the payload
/// is malformed, or the pairing token is expired.
pub async fn respond_pairing_handshake(
    connection: &dyn Connection,
    local: DeviceIdentity,
    local_listen_port: u16,
    now: Instant,
    ttl: Duration,
) -> Result<(PairingSession, PairingConfirmationPrompt), NetworkError> {
    validate_listen_port(local_listen_port)?;
    validate_pairing_identity(&local)?;
    let (request, peer_listen_port) =
        match recv_pairing_message(connection, IDENTITY_EXCHANGE_ID).await? {
            PairingHandshakeMessage::Request {
                request,
                listen_port,
            } => (request, listen_port),
            _ => return Err(NetworkError::Pairing("expected pairing request".into())),
        };
    validate_listen_port(peer_listen_port)?;
    validate_pairing_identities(&local, &request.identity)?;

    let bootstrap = PairingBootstrap::new(
        request.identity.display_name.clone(),
        request.identity.public_key.clone(),
        request.nonce,
        "transport",
    );
    let session = PairingSession::respond(local.clone(), &bootstrap, now, ttl)?;
    let code = session.confirmation_code(&request.identity.public_key, now)?;

    send_pairing_message(
        connection,
        IDENTITY_EXCHANGE_ID,
        &PairingHandshakeMessage::Response {
            response: PairingResponse {
                identity: local,
                confirmed: false,
            },
            listen_port: local_listen_port,
        },
    )
    .await?;

    Ok((
        session,
        PairingConfirmationPrompt {
            peer: request.identity,
            code,
            peer_listen_port,
        },
    ))
}

/// Exchange local user approval with the peer.
///
/// Returning `true` means the peer approved. Callers must require both local and
/// peer approval before touching persistent trust.
///
/// # Errors
/// Returns [`NetworkError`] for transport, version, malformed-message, or
/// out-of-order pairing failures.
pub async fn exchange_pairing_approval(
    connection: &dyn Connection,
    locally_accepted: bool,
) -> Result<bool, NetworkError> {
    send_pairing_message(
        connection,
        APPROVAL_EXCHANGE_ID,
        &PairingHandshakeMessage::Approval {
            accepted: locally_accepted,
        },
    )
    .await?;

    match recv_pairing_message(connection, APPROVAL_EXCHANGE_ID).await? {
        PairingHandshakeMessage::Approval { accepted } => Ok(accepted),
        _ => Err(NetworkError::Pairing("expected pairing approval".into())),
    }
}

/// Exchange whether each endpoint persisted trust and automatic peer settings.
///
/// Callers that persisted locally but receive `false` (or a transport error)
/// should roll back their local transaction.
///
/// # Errors
/// Returns [`NetworkError`] for transport, version, malformed-message, or
/// out-of-order pairing failures.
pub async fn exchange_pairing_persistence(
    connection: &dyn Connection,
    locally_succeeded: bool,
) -> Result<bool, NetworkError> {
    send_pairing_message(
        connection,
        PERSISTENCE_EXCHANGE_ID,
        &PairingHandshakeMessage::Persistence {
            succeeded: locally_succeeded,
        },
    )
    .await?;

    match recv_pairing_message(connection, PERSISTENCE_EXCHANGE_ID).await? {
        PairingHandshakeMessage::Persistence { succeeded } => Ok(succeeded),
        _ => Err(NetworkError::Pairing(
            "expected pairing persistence status".into(),
        )),
    }
}

fn validate_listen_port(port: u16) -> Result<(), NetworkError> {
    if port == 0 {
        return Err(NetworkError::Pairing(
            "pairing listen port must be non-zero".into(),
        ));
    }
    Ok(())
}

fn validate_pairing_identities(
    local: &DeviceIdentity,
    peer: &DeviceIdentity,
) -> Result<(), NetworkError> {
    validate_pairing_identity(local)?;
    validate_pairing_identity(peer)?;
    if local.public_key == peer.public_key {
        return Err(NetworkError::Pairing(
            "automatic pairing cannot pair a device with itself".into(),
        ));
    }
    Ok(())
}

fn validate_pairing_identity(identity: &DeviceIdentity) -> Result<(), NetworkError> {
    let display_name = identity.display_name.trim();
    if display_name.is_empty()
        || display_name.len() > MAX_DEVICE_DISPLAY_NAME_BYTES
        || display_name.chars().any(char::is_control)
    {
        return Err(NetworkError::Pairing(
            "pairing device name is empty, too long, or contains control characters".into(),
        ));
    }
    if identity.public_key.as_bytes().len() != DEVICE_IDENTITY_KEY_BYTES {
        return Err(NetworkError::Pairing(
            "pairing identity must contain a 32-byte Ed25519 public key".into(),
        ));
    }
    Ok(())
}

async fn send_pairing_message(
    connection: &dyn Connection,
    id: MessageId,
    message: &PairingHandshakeMessage,
) -> Result<(), NetworkError> {
    let body = serde_json::to_vec(message)
        .map_err(|error| NetworkError::Pairing(format!("encoding pairing message: {error}")))?;
    connection
        .send(Envelope::new(
            PROTOCOL_VERSION,
            id,
            MessageKind::Pairing,
            Bytes::from(body),
        ))
        .await
}

async fn recv_pairing_message(
    connection: &dyn Connection,
    expected_id: MessageId,
) -> Result<PairingHandshakeMessage, NetworkError> {
    let envelope = tokio::time::timeout(PAIRING_MESSAGE_TIMEOUT, connection.recv())
        .await
        .map_err(|_| NetworkError::Timeout)??;
    if envelope.kind != MessageKind::Pairing {
        return Err(NetworkError::Pairing(format!(
            "expected pairing message, received {:?}",
            envelope.kind
        )));
    }
    if envelope.id != expected_id {
        return Err(NetworkError::Pairing(format!(
            "expected pairing message id {}, received {}",
            expected_id.0, envelope.id.0
        )));
    }
    if VersionRange::current()
        .negotiate(envelope.version)
        .is_none()
    {
        return Err(ProtocolError::IncompatibleVersion {
            peer: envelope.version,
            supported: VersionRange::current(),
        }
        .into());
    }
    serde_json::from_slice(&envelope.body)
        .map_err(|error| NetworkError::Pairing(format!("decoding pairing message: {error}")))
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use async_trait::async_trait;
    use nexkvm_crypto::{DEFAULT_PAIRING_TTL, PairingState, PublicKey};
    use tokio::sync::mpsc;

    use super::*;
    use crate::TransportKind;

    #[derive(Debug)]
    struct MemoryConnection {
        peer: SocketAddr,
        tx: mpsc::Sender<Envelope>,
        rx: tokio::sync::Mutex<mpsc::Receiver<Envelope>>,
    }

    impl MemoryConnection {
        fn pair() -> (Self, Self) {
            let (a_tx, a_rx) = mpsc::channel(4);
            let (b_tx, b_rx) = mpsc::channel(4);
            (
                Self {
                    peer: "127.0.0.1:4101".parse().unwrap(),
                    tx: a_tx,
                    rx: tokio::sync::Mutex::new(b_rx),
                },
                Self {
                    peer: "127.0.0.1:4102".parse().unwrap(),
                    tx: b_tx,
                    rx: tokio::sync::Mutex::new(a_rx),
                },
            )
        }
    }

    #[async_trait]
    impl Connection for MemoryConnection {
        fn kind(&self) -> TransportKind {
            TransportKind::Tcp
        }

        fn peer_addr(&self) -> SocketAddr {
            self.peer
        }

        async fn send(&self, envelope: Envelope) -> Result<(), NetworkError> {
            self.tx
                .send(envelope)
                .await
                .map_err(|_| NetworkError::Closed)
        }

        async fn recv(&self) -> Result<Envelope, NetworkError> {
            self.rx
                .lock()
                .await
                .recv()
                .await
                .ok_or(NetworkError::Closed)
        }

        async fn close(&self) -> Result<(), NetworkError> {
            Ok(())
        }
    }

    fn identity(name: &str, key: &[u8]) -> DeviceIdentity {
        let mut public_key = vec![0; 32];
        for (index, byte) in public_key.iter_mut().enumerate() {
            *byte = key[index % key.len()];
        }
        DeviceIdentity {
            display_name: name.into(),
            public_key: PublicKey(public_key),
        }
    }

    #[test]
    fn pairing_identities_must_be_bounded_valid_and_distinct() {
        let left = identity("left", &[1]);
        let right = identity("right", &[2]);
        assert!(validate_pairing_identities(&left, &right).is_ok());

        assert!(validate_pairing_identities(&left, &left).is_err());
        assert!(
            validate_pairing_identities(
                &DeviceIdentity {
                    display_name: "left".into(),
                    public_key: PublicKey(vec![1; 31]),
                },
                &right,
            )
            .is_err()
        );
        assert!(
            validate_pairing_identities(
                &left,
                &DeviceIdentity {
                    display_name: " ".into(),
                    public_key: PublicKey(vec![2; 32]),
                },
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn network_pairing_exchange_yields_matching_confirmation_codes() {
        let now = Instant::now();
        let (initiator_conn, responder_conn) = MemoryConnection::pair();
        let initiator_id = identity("desk-macos", &[1, 2, 3, 4]);
        let responder_id = identity("laptop-linux", &[9, 8, 7, 6]);
        let initiator = PairingSession::initiate(
            initiator_id,
            "127.0.0.1:4101",
            [42u8; 32],
            now,
            DEFAULT_PAIRING_TTL,
        );

        let initiator_task = initiate_pairing_handshake(
            &initiator_conn,
            &initiator,
            PairingMethod::NumericCode,
            47_654,
            now,
        );
        let responder_task = respond_pairing_handshake(
            &responder_conn,
            responder_id.clone(),
            47_655,
            now,
            DEFAULT_PAIRING_TTL,
        );
        let (initiator_prompt, responder_result) = tokio::join!(initiator_task, responder_task);
        let initiator_prompt = initiator_prompt.unwrap();
        let (responder_session, responder_prompt) = responder_result.unwrap();

        assert_eq!(initiator_prompt.peer, responder_id);
        assert_eq!(responder_prompt.peer.display_name, "desk-macos");
        assert_eq!(initiator_prompt.code, responder_prompt.code);
        assert_eq!(initiator_prompt.peer_listen_port, 47_655);
        assert_eq!(responder_prompt.peer_listen_port, 47_654);
        assert_eq!(
            responder_session.state(),
            PairingState::AwaitingConfirmation
        );
    }

    #[tokio::test]
    async fn both_sides_exchange_approval_and_persistence_status() {
        let now = Instant::now();
        let (initiator_conn, responder_conn) = MemoryConnection::pair();
        let initiator_id = identity("desk-macos", &[1, 2, 3, 4]);
        let responder_id = identity("laptop-linux", &[9, 8, 7, 6]);
        let initiator = PairingSession::initiate(
            initiator_id,
            "127.0.0.1:4101",
            [42u8; 32],
            now,
            DEFAULT_PAIRING_TTL,
        );

        let initiator_flow = async {
            let prompt = initiate_pairing_handshake(
                &initiator_conn,
                &initiator,
                PairingMethod::NumericCode,
                47_654,
                now,
            )
            .await
            .unwrap();
            let peer_approved = exchange_pairing_approval(&initiator_conn, true)
                .await
                .unwrap();
            let peer_persisted = exchange_pairing_persistence(&initiator_conn, true)
                .await
                .unwrap();
            (prompt, peer_approved, peer_persisted)
        };
        let responder_flow = async {
            let (_session, prompt) = respond_pairing_handshake(
                &responder_conn,
                responder_id,
                47_655,
                now,
                DEFAULT_PAIRING_TTL,
            )
            .await
            .unwrap();
            let peer_approved = exchange_pairing_approval(&responder_conn, true)
                .await
                .unwrap();
            let peer_persisted = exchange_pairing_persistence(&responder_conn, true)
                .await
                .unwrap();
            (prompt, peer_approved, peer_persisted)
        };

        let (initiator_result, responder_result) = tokio::join!(initiator_flow, responder_flow);

        assert_eq!(initiator_result.0.code, responder_result.0.code);
        assert!(initiator_result.1 && responder_result.1);
        assert!(initiator_result.2 && responder_result.2);
    }

    #[tokio::test]
    async fn either_side_can_reject_before_trust_is_persisted() {
        let (left, right) = MemoryConnection::pair();
        let (left_result, right_result) = tokio::join!(
            exchange_pairing_approval(&left, true),
            exchange_pairing_approval(&right, false),
        );

        assert!(!left_result.unwrap());
        assert!(right_result.unwrap());
    }

    #[tokio::test]
    async fn non_pairing_envelope_is_rejected() {
        let (initiator_conn, responder_conn) = MemoryConnection::pair();
        initiator_conn
            .send(Envelope::new(
                PROTOCOL_VERSION,
                MessageId::ZERO,
                MessageKind::Heartbeat,
                Bytes::from_static(b"not pairing"),
            ))
            .await
            .unwrap();

        let err = respond_pairing_handshake(
            &responder_conn,
            identity("laptop-linux", &[9]),
            47_655,
            Instant::now(),
            DEFAULT_PAIRING_TTL,
        )
        .await
        .unwrap_err();

        assert!(matches!(err, NetworkError::Pairing(_)));
    }
}
