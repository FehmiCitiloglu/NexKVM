//! Trust-gated reconnect authentication for already-paired devices.
//!
//! Discovery fingerprints decide which peers are worth dialing; this module is
//! the reconnect gate after a transport connection exists. Both sides exchange
//! device identities over the handshake lane and reject peers whose public key
//! is not already pinned in the local trust store.
//!
//! This does not yet prove private-key possession. The follow-up signing/key
//! ownership feature will strengthen this gate from "pinned public key match" to
//! a cryptographic challenge response.

use bytes::Bytes;
use nexkvm_crypto::{DeviceIdentity, TrustEntry, TrustStore};
use nexkvm_protocol::{Envelope, MessageId, MessageKind, PROTOCOL_VERSION};
use serde::{Deserialize, Serialize};

use crate::{Connection, NetworkError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedReconnectPeer {
    /// Peer identity received over the transport.
    pub identity: DeviceIdentity,
    /// Matching trust-store entry pinned during pairing.
    pub trust: TrustEntry,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ReconnectHello {
    identity: DeviceIdentity,
}

/// Authenticate an outbound reconnect after dialing a rediscovered peer.
///
/// Both endpoints may call this function concurrently: it sends the local
/// identity first, then waits for the peer's identity and checks that public key
/// against `trust`.
///
/// # Errors
/// Returns [`NetworkError::Crypto`] when the peer is not already trusted, or
/// [`NetworkError::Pairing`] when the peer sends the wrong message kind or a
/// malformed identity payload.
pub async fn authenticate_trusted_reconnect(
    connection: &dyn Connection,
    local: &DeviceIdentity,
    trust: &dyn TrustStore,
) -> Result<TrustedReconnectPeer, NetworkError> {
    send_hello(connection, local).await?;
    receive_trusted_hello(connection, trust).await
}

/// Authenticate an inbound reconnect accepted by the local listener.
///
/// This is an alias for the same symmetric exchange used by outbound reconnects;
/// it exists to make call sites read naturally.
///
/// # Errors
/// Returns [`NetworkError`] for transport, malformed-message, or trust failures.
pub async fn accept_trusted_reconnect(
    connection: &dyn Connection,
    local: &DeviceIdentity,
    trust: &dyn TrustStore,
) -> Result<TrustedReconnectPeer, NetworkError> {
    authenticate_trusted_reconnect(connection, local, trust).await
}

async fn send_hello(
    connection: &dyn Connection,
    identity: &DeviceIdentity,
) -> Result<(), NetworkError> {
    let body = serde_json::to_vec(&ReconnectHello {
        identity: identity.clone(),
    })
    .map_err(|error| NetworkError::Pairing(format!("encoding reconnect hello: {error}")))?;
    connection
        .send(Envelope::new(
            PROTOCOL_VERSION,
            MessageId::ZERO,
            MessageKind::Handshake,
            Bytes::from(body),
        ))
        .await
}

async fn receive_trusted_hello(
    connection: &dyn Connection,
    trust: &dyn TrustStore,
) -> Result<TrustedReconnectPeer, NetworkError> {
    let envelope = connection.recv().await?;
    if envelope.kind != MessageKind::Handshake {
        return Err(NetworkError::Pairing(format!(
            "expected reconnect handshake, received {:?}",
            envelope.kind
        )));
    }
    let hello: ReconnectHello = serde_json::from_slice(&envelope.body)
        .map_err(|error| NetworkError::Pairing(format!("decoding reconnect hello: {error}")))?;
    let trust_entry = trust
        .get(&hello.identity.public_key)
        .ok_or(nexkvm_crypto::CryptoError::Untrusted)?;
    Ok(TrustedReconnectPeer {
        identity: hello.identity,
        trust: trust_entry,
    })
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use async_trait::async_trait;
    use nexkvm_crypto::{InMemoryTrustStore, PublicKey, TrustEntry, TrustStore};
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

    fn trust_peer(store: &InMemoryTrustStore, identity: &DeviceIdentity) {
        store.insert(TrustEntry {
            display_name: identity.display_name.clone(),
            public_key: identity.public_key.clone(),
            paired_at: 1_700_000_000,
        });
    }

    #[tokio::test]
    async fn trusted_devices_authenticate_reconnect() {
        let (left_conn, right_conn) = MemoryConnection::pair();
        let left = identity("desk-macos", &[1, 2, 3, 4]);
        let right = identity("laptop-linux", &[9, 8, 7, 6]);
        let left_store = InMemoryTrustStore::new();
        let right_store = InMemoryTrustStore::new();
        trust_peer(&left_store, &right);
        trust_peer(&right_store, &left);

        let left_auth = authenticate_trusted_reconnect(&left_conn, &left, &left_store);
        let right_auth = accept_trusted_reconnect(&right_conn, &right, &right_store);
        let (left_peer, right_peer) = tokio::join!(left_auth, right_auth);

        assert_eq!(left_peer.unwrap().identity, right);
        assert_eq!(right_peer.unwrap().identity, left);
    }

    #[tokio::test]
    async fn untrusted_reconnect_is_rejected() {
        let (left_conn, right_conn) = MemoryConnection::pair();
        let left = identity("desk-macos", &[1, 2, 3, 4]);
        let stranger = identity("stranger", &[0xaa]);
        let left_store = InMemoryTrustStore::new();
        let stranger_store = InMemoryTrustStore::new();
        trust_peer(&stranger_store, &left);

        let left_auth = authenticate_trusted_reconnect(&left_conn, &left, &left_store);
        let stranger_auth = accept_trusted_reconnect(&right_conn, &stranger, &stranger_store);
        let (left_result, stranger_result) = tokio::join!(left_auth, stranger_auth);

        assert!(matches!(left_result, Err(NetworkError::Crypto(_))));
        assert!(stranger_result.is_ok());
    }
}
