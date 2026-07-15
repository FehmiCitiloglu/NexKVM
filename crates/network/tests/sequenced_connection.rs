use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use nexkvm_network::{Connection, NetworkError, SequencedConnection, TransportKind};
use nexkvm_protocol::{Envelope, MessageId, MessageKind, PROTOCOL_VERSION};

#[derive(Debug, Default)]
struct RecordingConnection {
    sent: Mutex<Vec<Envelope>>,
}

#[async_trait]
impl Connection for RecordingConnection {
    fn kind(&self) -> TransportKind {
        TransportKind::Tcp
    }

    fn peer_addr(&self) -> SocketAddr {
        SocketAddr::from((Ipv4Addr::LOCALHOST, 47_654))
    }

    async fn send(&self, envelope: Envelope) -> Result<(), NetworkError> {
        self.sent
            .lock()
            .expect("sent mutex poisoned")
            .push(envelope);
        Ok(())
    }

    async fn recv(&self) -> Result<Envelope, NetworkError> {
        Err(NetworkError::Closed)
    }

    async fn close(&self) -> Result<(), NetworkError> {
        Ok(())
    }
}

fn envelope(kind: MessageKind) -> Envelope {
    Envelope::new(
        PROTOCOL_VERSION,
        MessageId(999),
        kind,
        Bytes::from_static(b"payload"),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn assigns_one_monotonic_id_across_concurrent_message_kinds() {
    let inner = Arc::new(RecordingConnection::default());
    let sequenced = Arc::new(SequencedConnection::new(inner.clone()));

    let input = {
        let sequenced = Arc::clone(&sequenced);
        tokio::spawn(async move { sequenced.send(envelope(MessageKind::Input)).await })
    };
    let clipboard = {
        let sequenced = Arc::clone(&sequenced);
        tokio::spawn(async move { sequenced.send(envelope(MessageKind::Clipboard)).await })
    };

    input.await.unwrap().unwrap();
    clipboard.await.unwrap().unwrap();

    let mut ids: Vec<_> = inner
        .sent
        .lock()
        .expect("sent mutex poisoned")
        .iter()
        .map(|envelope| envelope.id.0)
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, [0, 1]);
}

#[tokio::test]
async fn overwrites_caller_supplied_ids_to_prevent_nonce_reuse() {
    let inner = Arc::new(RecordingConnection::default());
    let sequenced = SequencedConnection::new(inner.clone());

    sequenced.send(envelope(MessageKind::Input)).await.unwrap();
    sequenced
        .send(envelope(MessageKind::FileTransfer))
        .await
        .unwrap();

    let sent = inner.sent.lock().expect("sent mutex poisoned");
    assert_eq!(sent[0].id, MessageId(0));
    assert_eq!(sent[1].id, MessageId(1));
}

#[tokio::test]
async fn can_reserve_control_ids_before_lane_sequencing() {
    let inner = Arc::new(RecordingConnection::default());
    let sequenced = SequencedConnection::new_starting_at(inner.clone(), 1);

    sequenced.send(envelope(MessageKind::Input)).await.unwrap();
    sequenced
        .send(envelope(MessageKind::Clipboard))
        .await
        .unwrap();

    let sent = inner.sent.lock().expect("sent mutex poisoned");
    assert_eq!(sent[0].id, MessageId(1));
    assert_eq!(sent[1].id, MessageId(2));
}
