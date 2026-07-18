//! Authenticated, multi-peer clipboard session orchestration.
//!
//! The transport's trusted session encrypts and authenticates the complete
//! protocol envelope. Consequently the clipboard state machine deliberately
//! uses its no-op payload cipher here; accepting this lane without an
//! authenticated peer identity is forbidden by the caller.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nexkvm_clipboard::{Clipboard, ClipboardSync, ClipboardUpdate, PlaintextCipher};
use nexkvm_core::DeviceId;
use nexkvm_crypto::PublicKey;
use nexkvm_network::{Connection, NetworkError};
use nexkvm_protocol::{Envelope, MessageId, MessageKind, PROTOCOL_VERSION};

use crate::clipboard_history::{self, ClipboardHistoryRecorder};

const CLIPBOARD_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// One active clipboard session per authenticated peer.
///
/// Every session owns an independent sync resolver, so a local selection is
/// observed and sent once on each peer link. Duplicate physical connections to
/// the same identity are rejected to avoid competing writers and echo state.
#[derive(Debug, Default)]
pub(crate) struct ClipboardPeerGate {
    active_peers: Mutex<HashSet<PublicKey>>,
}

impl ClipboardPeerGate {
    pub(crate) fn try_acquire(self: &Arc<Self>, peer: PublicKey) -> Option<ClipboardPeerLease> {
        let mut active = self
            .active_peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !active.insert(peer.clone()) {
            return None;
        }
        Some(ClipboardPeerLease {
            gate: Arc::clone(self),
            peer,
        })
    }
}

/// Releases the clipboard peer slot when the connection supervisor exits.
#[derive(Debug)]
pub(crate) struct ClipboardPeerLease {
    gate: Arc<ClipboardPeerGate>,
    peer: PublicKey,
}

impl Drop for ClipboardPeerLease {
    fn drop(&mut self) {
        let mut active = self
            .gate
            .active_peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        active.remove(&self.peer);
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ClipboardSessionError {
    #[error(transparent)]
    Network(#[from] NetworkError),
    #[error("unexpected clipboard-lane message kind: {0:?}")]
    UnexpectedKind(MessageKind),
    #[error("clipboard update origin {actual} does not match authenticated peer {expected}")]
    OriginMismatch {
        expected: DeviceId,
        actual: DeviceId,
    },
    #[error("invalid clipboard update: {0}")]
    Update(#[from] nexkvm_clipboard::ClipboardError),
}

/// Run local polling and remote receive in one cancellation scope.
///
/// A fresh [`ClipboardSync`] belongs exclusively to this peer connection. If a
/// send fails after consuming local state, the next connection starts with a
/// fresh resolver and therefore retries the clipboard's current snapshot.
pub(crate) async fn run_peer_session<C>(
    clipboard: Arc<C>,
    connection: Arc<dyn Connection>,
    local_device_id: DeviceId,
    authenticated_peer: DeviceId,
    history: Option<ClipboardHistoryRecorder>,
) -> Result<(), ClipboardSessionError>
where
    C: Clipboard + ?Sized + 'static,
{
    run_peer_session_with_interval(
        clipboard,
        connection,
        local_device_id,
        authenticated_peer,
        history,
        CLIPBOARD_POLL_INTERVAL,
    )
    .await
}

async fn run_peer_session_with_interval<C>(
    clipboard: Arc<C>,
    connection: Arc<dyn Connection>,
    local_device_id: DeviceId,
    authenticated_peer: DeviceId,
    history: Option<ClipboardHistoryRecorder>,
    poll_interval: Duration,
) -> Result<(), ClipboardSessionError>
where
    C: Clipboard + ?Sized + 'static,
{
    // SecureConnection seals/authenticates this entire envelope, including the
    // message kind and update body. A second inner cipher would not add a new
    // trust boundary and would complicate per-session key coordination.
    let mut sync = ClipboardSync::new(local_device_id, Box::new(PlaintextCipher));
    let mut poll = tokio::time::interval(poll_interval);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = poll.tick() => {
                poll_and_send(&*clipboard, &*connection, &mut sync).await?;
            }
            received = connection.recv() => {
                match received {
                    Ok(envelope) => {
                        receive_and_apply(
                            &*clipboard,
                            &mut sync,
                            envelope,
                            authenticated_peer,
                            history.as_ref(),
                        ).await?;
                    }
                    Err(NetworkError::Closed) => return Ok(()),
                    Err(error) => return Err(error.into()),
                }
            }
        }
    }
}

async fn poll_and_send<C>(
    clipboard: &C,
    connection: &dyn Connection,
    sync: &mut ClipboardSync,
) -> Result<(), ClipboardSessionError>
where
    C: Clipboard + ?Sized,
{
    let snapshot = match clipboard.read().await {
        Ok(Some(snapshot)) => snapshot,
        Ok(None) => return Ok(()),
        Err(error) => {
            tracing::warn!(%error, "clipboard read failed");
            return Ok(());
        }
    };
    let Some(update) = sync.prepare_outbound(&snapshot, clipboard_history::now_millis())? else {
        return Ok(());
    };
    let envelope = Envelope::new(
        PROTOCOL_VERSION,
        MessageId(0),
        MessageKind::Clipboard,
        update.encode()?,
    );
    connection.send(envelope).await?;
    Ok(())
}

async fn receive_and_apply<C>(
    clipboard: &C,
    sync: &mut ClipboardSync,
    envelope: Envelope,
    authenticated_peer: DeviceId,
    history: Option<&ClipboardHistoryRecorder>,
) -> Result<(), ClipboardSessionError>
where
    C: Clipboard + ?Sized,
{
    if envelope.kind != MessageKind::Clipboard {
        return Err(ClipboardSessionError::UnexpectedKind(envelope.kind));
    }
    let update = ClipboardUpdate::decode(envelope.body)?;
    if update.origin != authenticated_peer {
        return Err(ClipboardSessionError::OriginMismatch {
            expected: authenticated_peer,
            actual: update.origin,
        });
    }
    let origin = update.origin;
    let Some(snapshot) = sync.accept_inbound(update)? else {
        return Ok(());
    };
    match clipboard.write(snapshot.clone()).await {
        Ok(()) => {}
        Err(error) => {
            tracing::warn!(%error, "clipboard write failed");
            return Ok(());
        }
    }
    if let Some(history) = history
        && let Err(error) = history
            .record(snapshot, origin, clipboard_history::now_millis(), false)
            .await
    {
        tracing::warn!(%error, "failed to persist received clipboard history");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::net::{Ipv4Addr, SocketAddr};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use async_trait::async_trait;
    use nexkvm_clipboard::{ClipboardError, ClipboardSnapshot};
    use nexkvm_crypto::DeviceKeypair;
    use nexkvm_network::TransportKind;
    use nexkvm_storage::{ClipboardConfig, ClipboardHistoryArchive};
    use tokio::sync::Notify;

    use super::*;

    #[derive(Debug)]
    struct TestClipboard {
        snapshot: ClipboardSnapshot,
        reads: AtomicUsize,
        writes: Mutex<Vec<ClipboardSnapshot>>,
    }

    impl TestClipboard {
        fn new(text: &str) -> Self {
            Self {
                snapshot: ClipboardSnapshot::from_text(text),
                reads: AtomicUsize::new(0),
                writes: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl Clipboard for TestClipboard {
        async fn read(&self) -> Result<Option<ClipboardSnapshot>, ClipboardError> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            Ok(Some(self.snapshot.clone()))
        }

        async fn write(&self, snapshot: ClipboardSnapshot) -> Result<(), ClipboardError> {
            self.writes.lock().unwrap().push(snapshot);
            Ok(())
        }
    }

    #[derive(Debug)]
    struct TestConnection {
        identity: PublicKey,
        fail_sends: Mutex<VecDeque<NetworkError>>,
        sent: Mutex<Vec<Envelope>>,
        sent_notify: Notify,
        receive_closed: AtomicBool,
        receive_notify: Notify,
    }

    impl TestConnection {
        fn new(identity: PublicKey) -> Self {
            Self {
                identity,
                fail_sends: Mutex::new(VecDeque::new()),
                sent: Mutex::new(Vec::new()),
                sent_notify: Notify::new(),
                receive_closed: AtomicBool::new(false),
                receive_notify: Notify::new(),
            }
        }

        fn fail_next_send(&self) {
            self.fail_sends
                .lock()
                .unwrap()
                .push_back(NetworkError::Closed);
        }

        fn finish_receive(&self) {
            self.receive_closed.store(true, Ordering::Release);
            self.receive_notify.notify_waiters();
        }

        async fn wait_for_send(&self) {
            loop {
                let notified = self.sent_notify.notified();
                if !self.sent.lock().unwrap().is_empty() {
                    return;
                }
                notified.await;
            }
        }
    }

    #[async_trait]
    impl Connection for TestConnection {
        fn kind(&self) -> TransportKind {
            TransportKind::Tcp
        }

        fn peer_addr(&self) -> SocketAddr {
            SocketAddr::from((Ipv4Addr::LOCALHOST, 47_654))
        }

        fn peer_identity(&self) -> Option<PublicKey> {
            Some(self.identity.clone())
        }

        async fn send(&self, envelope: Envelope) -> Result<(), NetworkError> {
            if let Some(error) = self.fail_sends.lock().unwrap().pop_front() {
                return Err(error);
            }
            self.sent.lock().unwrap().push(envelope);
            self.sent_notify.notify_waiters();
            Ok(())
        }

        async fn recv(&self) -> Result<Envelope, NetworkError> {
            loop {
                let notified = self.receive_notify.notified();
                if self.receive_closed.load(Ordering::Acquire) {
                    return Err(NetworkError::Closed);
                }
                notified.await;
            }
        }

        async fn close(&self) -> Result<(), NetworkError> {
            self.finish_receive();
            Ok(())
        }
    }

    fn key(seed: u8) -> PublicKey {
        DeviceKeypair::from_seed([seed; 32]).public_key()
    }

    #[test]
    fn peer_leases_allow_distinct_peers_and_reject_duplicate_connections() {
        let gate = Arc::new(ClipboardPeerGate::default());
        let first = gate.try_acquire(key(1)).expect("first peer lease");

        assert!(gate.try_acquire(key(1)).is_none());
        let second = gate
            .try_acquire(key(2))
            .expect("a different trusted peer gets its own clipboard lane");

        drop(first);
        assert!(gate.try_acquire(key(1)).is_some());
        assert!(gate.try_acquire(key(2)).is_none());

        drop(second);
        assert!(gate.try_acquire(key(2)).is_some());
    }

    #[tokio::test]
    async fn reconnect_retries_current_clipboard_after_a_failed_send() {
        let clipboard = Arc::new(TestClipboard::new("retry me"));
        let identity = key(3);
        let peer = crate::stable_device_id(&identity);
        let failed = Arc::new(TestConnection::new(identity.clone()));
        failed.fail_next_send();
        let failed_connection: Arc<dyn Connection> = failed;

        assert!(
            run_peer_session_with_interval(
                Arc::clone(&clipboard),
                failed_connection,
                DeviceId::generate(),
                peer,
                None,
                Duration::from_secs(60),
            )
            .await
            .is_err()
        );

        let reconnected = Arc::new(TestConnection::new(identity));
        let connection: Arc<dyn Connection> = reconnected.clone();
        let clipboard_for_task = Arc::clone(&clipboard);
        let task = tokio::spawn(async move {
            run_peer_session_with_interval(
                clipboard_for_task,
                connection,
                DeviceId::generate(),
                peer,
                None,
                Duration::from_secs(60),
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(1), reconnected.wait_for_send())
            .await
            .expect("current clipboard was not retried after reconnect");
        reconnected.finish_receive();
        task.await.unwrap().unwrap();

        assert_eq!(reconnected.sent.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn closed_receive_stops_local_polling() {
        let clipboard = Arc::new(TestClipboard::new("stop polling"));
        let identity = key(4);
        let peer = crate::stable_device_id(&identity);
        let connection = Arc::new(TestConnection::new(identity));
        let connection_trait: Arc<dyn Connection> = connection.clone();
        let clipboard_for_task = Arc::clone(&clipboard);
        let task = tokio::spawn(async move {
            run_peer_session_with_interval(
                clipboard_for_task,
                connection_trait,
                DeviceId::generate(),
                peer,
                None,
                Duration::from_millis(10),
            )
            .await
        });

        tokio::time::timeout(Duration::from_secs(1), connection.wait_for_send())
            .await
            .expect("initial clipboard poll did not run");
        connection.finish_receive();
        task.await.unwrap().unwrap();
        let reads_after_close = clipboard.reads.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(40)).await;

        assert_eq!(clipboard.reads.load(Ordering::SeqCst), reads_after_close);
    }

    #[tokio::test]
    async fn authenticated_peer_copy_becomes_local_selection_and_encrypted_history() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        let config = ClipboardConfig {
            history_enabled: true,
            ..ClipboardConfig::default()
        };
        let history = ClipboardHistoryRecorder::open(&config_path, &config)
            .unwrap()
            .unwrap();
        let remote = DeviceId::generate();
        let snapshot = ClipboardSnapshot::from_text("copy from another computer");
        let mut remote_sync = ClipboardSync::new(remote, Box::new(PlaintextCipher));
        let update = remote_sync
            .prepare_outbound(&snapshot, 123)
            .unwrap()
            .unwrap();
        let envelope = Envelope::new(
            PROTOCOL_VERSION,
            MessageId(7),
            MessageKind::Clipboard,
            update.encode().unwrap(),
        );
        let clipboard = TestClipboard::new("old local clipboard");
        let mut local_sync = ClipboardSync::new(DeviceId::generate(), Box::new(PlaintextCipher));

        receive_and_apply(
            &clipboard,
            &mut local_sync,
            envelope,
            remote,
            Some(&history),
        )
        .await
        .unwrap();

        assert_eq!(
            clipboard.writes.lock().unwrap().as_slice(),
            std::slice::from_ref(&snapshot)
        );
        let archive = ClipboardHistoryArchive::open(
            clipboard_history::archive_path(&config_path),
            clipboard_history::archive_config(&config),
        )
        .unwrap();
        let entry = archive.entries().next().expect("received item is pooled");
        assert_eq!(entry.snapshot, snapshot);
        assert_eq!(entry.origin, remote);
    }
}
