//! User-confirmed pairing exchange over an established transport connection.
//!
//! This module deliberately stops at the user-confirmation prompt: it exchanges
//! pairing identities over [`MessageKind::Pairing`] and derives the short code
//! both devices must compare. Pinning the peer into a trust store is a separate
//! storage/runtime step.

use std::time::{Duration, Instant};

use bytes::Bytes;
use nexkvm_crypto::{
    ConfirmationCode, DeviceIdentity, PairingBootstrap, PairingMethod, PairingRequest,
    PairingResponse, PairingSession,
};
use nexkvm_protocol::{Envelope, MessageId, MessageKind, PROTOCOL_VERSION};
use serde::{Deserialize, Serialize};

use crate::{Connection, NetworkError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingConfirmationPrompt {
    /// Peer identity received over the transport.
    pub peer: DeviceIdentity,
    /// Short code the local user must compare with the peer's screen.
    pub code: ConfirmationCode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum PairingHandshakeMessage {
    Request(PairingRequest),
    Response(PairingResponse),
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
    now: Instant,
) -> Result<PairingConfirmationPrompt, NetworkError> {
    let bootstrap = session
        .bootstrap()
        .ok_or_else(|| NetworkError::Pairing("initiator session required".into()))?;
    let request = PairingRequest {
        identity: DeviceIdentity {
            display_name: bootstrap.display_name,
            public_key: bootstrap.public_key,
        },
        method,
        nonce: bootstrap.nonce,
    };
    send_pairing_message(
        connection,
        MessageId::ZERO,
        &PairingHandshakeMessage::Request(request),
    )
    .await?;

    let response = match recv_pairing_message(connection).await? {
        PairingHandshakeMessage::Response(response) => response,
        PairingHandshakeMessage::Request(_) => {
            return Err(NetworkError::Pairing(
                "expected pairing response, received request".into(),
            ));
        }
    };

    if response.confirmed {
        return Err(NetworkError::Pairing(
            "peer confirmed before local user comparison".into(),
        ));
    }

    let code = session.confirmation_code(&response.identity.public_key, now)?;
    Ok(PairingConfirmationPrompt {
        peer: response.identity,
        code,
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
    now: Instant,
    ttl: Duration,
) -> Result<(PairingSession, PairingConfirmationPrompt), NetworkError> {
    let request = match recv_pairing_message(connection).await? {
        PairingHandshakeMessage::Request(request) => request,
        PairingHandshakeMessage::Response(_) => {
            return Err(NetworkError::Pairing(
                "expected pairing request, received response".into(),
            ));
        }
    };

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
        MessageId::ZERO,
        &PairingHandshakeMessage::Response(PairingResponse {
            identity: local,
            confirmed: false,
        }),
    )
    .await?;

    Ok((
        session,
        PairingConfirmationPrompt {
            peer: request.identity,
            code,
        },
    ))
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
) -> Result<PairingHandshakeMessage, NetworkError> {
    let envelope = connection.recv().await?;
    if envelope.kind != MessageKind::Pairing {
        return Err(NetworkError::Pairing(format!(
            "expected pairing message, received {:?}",
            envelope.kind
        )));
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
        DeviceIdentity {
            display_name: name.into(),
            public_key: PublicKey(key.to_vec()),
        }
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
            now,
        );
        let responder_task = respond_pairing_handshake(
            &responder_conn,
            responder_id.clone(),
            now,
            DEFAULT_PAIRING_TTL,
        );
        let (initiator_prompt, responder_result) = tokio::join!(initiator_task, responder_task);
        let initiator_prompt = initiator_prompt.unwrap();
        let (responder_session, responder_prompt) = responder_result.unwrap();

        assert_eq!(initiator_prompt.peer, responder_id);
        assert_eq!(responder_prompt.peer.display_name, "desk-macos");
        assert_eq!(initiator_prompt.code, responder_prompt.code);
        assert_eq!(
            responder_session.state(),
            PairingState::AwaitingConfirmation
        );
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
            Instant::now(),
            DEFAULT_PAIRING_TTL,
        )
        .await
        .unwrap_err();

        assert!(matches!(err, NetworkError::Pairing(_)));
    }
}
