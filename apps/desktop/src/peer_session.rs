use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use nexkvm_crypto::PublicKey;
use nexkvm_network::{Connection, NetworkError, SequencedConnection, TransportKind};
use nexkvm_protocol::{
    Envelope, MessageId, MessageKind, PROTOCOL_VERSION, ProtocolError, VersionRange,
};
use tokio::sync::{Mutex, mpsc, oneshot, watch};

use crate::connection::{ConnectionOrigin, PeerConnectionContext, PeerConnectionHandler};

const LANE_CAPACITY: usize = 512;
const SESSION_ARBITRATION_FALLBACK: Duration = Duration::from_millis(250);
const SESSION_ARBITRATION_TIMEOUT: Duration = Duration::from_secs(3);
const SESSION_CLOSE_TIMEOUT: Duration = Duration::from_millis(500);
const LANE_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);
const SESSION_ARBITRATION_MAGIC: &[u8; 5] = b"NXSA1";
const SESSION_DATA_FIRST_ID: u64 = 1;

/// One subsystem attached to a peer connection and the inbound message kinds
/// that subsystem is allowed to consume.
#[derive(Clone)]
pub struct PeerLaneHandler {
    handler: PeerConnectionHandler,
    inbound_kinds: Vec<MessageKind>,
}

impl PeerLaneHandler {
    pub fn new(
        handler: PeerConnectionHandler,
        inbound_kinds: impl IntoIterator<Item = MessageKind>,
    ) -> Self {
        Self {
            handler,
            inbound_kinds: inbound_kinds.into_iter().collect(),
        }
    }
}

impl std::fmt::Debug for PeerLaneHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerLaneHandler")
            .field("inbound_kinds", &self.inbound_kinds)
            .finish_non_exhaustive()
    }
}

/// Compose subsystem handlers over one sequenced sender and one receive loop.
/// Each inbound envelope is delivered exactly once to the lane registered for
/// its [`MessageKind`].
pub fn compose_peer_handler(
    local_identity: PublicKey,
    handlers: Vec<PeerLaneHandler>,
) -> Option<PeerConnectionHandler> {
    if handlers.is_empty() {
        return None;
    }
    let handlers = Arc::new(handlers);
    let arbiter = Arc::new(SessionArbiter::default());
    Some(Arc::new(move |connection, context| {
        let handlers = Arc::clone(&handlers);
        let local_identity = local_identity.clone();
        let arbiter = Arc::clone(&arbiter);
        tokio::spawn(async move {
            if let Err(error) =
                arbitrate_and_fan_out(connection, context, local_identity, arbiter, handlers).await
            {
                tracing::warn!(%error, "peer session arbitration failed");
            }
        });
    }))
}

#[derive(Debug, Default)]
struct SessionArbiter {
    active_peers: StdMutex<HashSet<PublicKey>>,
}

impl SessionArbiter {
    fn try_acquire(self: &Arc<Self>, peer: PublicKey) -> Option<SessionLease> {
        let mut active = self
            .active_peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !active.insert(peer.clone()) {
            return None;
        }
        Some(SessionLease {
            arbiter: Arc::clone(self),
            peer,
        })
    }
}

#[derive(Debug)]
struct SessionLease {
    arbiter: Arc<SessionArbiter>,
    peer: PublicKey,
}

impl Drop for SessionLease {
    fn drop(&mut self) {
        self.arbiter
            .active_peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.peer);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArbitrationDecision {
    Accept,
    Reject,
    AcceptAck,
    RejectAck,
}

async fn arbitrate_and_fan_out(
    connection: Box<dyn Connection>,
    context: PeerConnectionContext,
    local_identity: PublicKey,
    arbiter: Arc<SessionArbiter>,
    handlers: Arc<Vec<PeerLaneHandler>>,
) -> Result<(), NetworkError> {
    let Some(peer_identity) = connection.peer_identity() else {
        close_rejected_session(connection.as_ref()).await;
        return Err(ProtocolError::ProtocolMismatch(
            "session arbitration requires an authenticated peer",
        )
        .into());
    };
    if local_identity == peer_identity {
        close_rejected_session(connection.as_ref()).await;
        return Err(
            ProtocolError::ProtocolMismatch("session arbitration rejects a self identity").into(),
        );
    }

    let local_is_authority = local_identity.as_bytes() < peer_identity.as_bytes();
    let lease = if local_is_authority {
        // The lower identity is the sole decision authority. Its outbound
        // session is preferred during a simultaneous cross-dial. Waiting only
        // on the reverse direction preserves a one-way explicit connection as
        // a bounded fallback without allowing endpoints to elect separately.
        if context.origin() == ConnectionOrigin::Inbound {
            tokio::time::sleep(SESSION_ARBITRATION_FALLBACK).await;
        }
        let lease = arbiter.try_acquire(peer_identity.clone());
        let decision = if lease.is_some() {
            ArbitrationDecision::Accept
        } else {
            ArbitrationDecision::Reject
        };
        if let Err(error) = send_arbitration(connection.as_ref(), decision).await {
            close_rejected_session(connection.as_ref()).await;
            return Err(error);
        }
        match lease {
            Some(lease) => {
                let acknowledgement = match receive_arbitration(connection.as_ref()).await {
                    Ok(acknowledgement) => acknowledgement,
                    Err(error) => {
                        close_rejected_session(connection.as_ref()).await;
                        return Err(error);
                    }
                };
                match acknowledgement {
                    ArbitrationDecision::AcceptAck => lease,
                    ArbitrationDecision::RejectAck => {
                        close_rejected_session(connection.as_ref()).await;
                        return Ok(());
                    }
                    ArbitrationDecision::Accept | ArbitrationDecision::Reject => {
                        close_rejected_session(connection.as_ref()).await;
                        return Err(ProtocolError::ProtocolMismatch(
                            "invalid peer session arbitration acknowledgement",
                        )
                        .into());
                    }
                }
            }
            None => {
                close_rejected_session(connection.as_ref()).await;
                return Ok(());
            }
        }
    } else {
        // The higher identity never makes a local election. It follows the
        // authenticated authority's decision on this exact physical session,
        // which closes timeout/arrival-order races between cross-dial links.
        let decision = match receive_arbitration(connection.as_ref()).await {
            Ok(decision) => decision,
            Err(error) => {
                close_rejected_session(connection.as_ref()).await;
                return Err(error);
            }
        };
        match decision {
            ArbitrationDecision::Accept => {
                let lease = arbiter.try_acquire(peer_identity.clone());
                let acknowledgement = if lease.is_some() {
                    ArbitrationDecision::AcceptAck
                } else {
                    ArbitrationDecision::RejectAck
                };
                if let Err(error) = send_arbitration(connection.as_ref(), acknowledgement).await {
                    close_rejected_session(connection.as_ref()).await;
                    return Err(error);
                }
                match lease {
                    Some(lease) => lease,
                    None => {
                        close_rejected_session(connection.as_ref()).await;
                        return Ok(());
                    }
                }
            }
            ArbitrationDecision::Reject => {
                close_rejected_session(connection.as_ref()).await;
                return Ok(());
            }
            ArbitrationDecision::AcceptAck | ArbitrationDecision::RejectAck => {
                close_rejected_session(connection.as_ref()).await;
                return Err(ProtocolError::ProtocolMismatch(
                    "unexpected peer session arbitration acknowledgement",
                )
                .into());
            }
        }
    };

    fan_out_session(connection, context, handlers, lease);
    Ok(())
}

fn arbitration_envelope(decision: ArbitrationDecision) -> Envelope {
    let decision = match decision {
        ArbitrationDecision::Accept => 1,
        ArbitrationDecision::Reject => 0,
        ArbitrationDecision::AcceptAck => 2,
        ArbitrationDecision::RejectAck => 3,
    };
    let mut body = Vec::with_capacity(SESSION_ARBITRATION_MAGIC.len() + 1);
    body.extend_from_slice(SESSION_ARBITRATION_MAGIC);
    body.push(decision);
    Envelope::new(
        PROTOCOL_VERSION,
        MessageId::ZERO,
        MessageKind::Handshake,
        Bytes::from(body),
    )
}

fn decode_arbitration_decision(envelope: Envelope) -> Result<ArbitrationDecision, NetworkError> {
    if envelope.id != MessageId::ZERO
        || envelope.kind != MessageKind::Handshake
        || VersionRange::current()
            .negotiate(envelope.version)
            .is_none()
        || envelope.body.len() != SESSION_ARBITRATION_MAGIC.len() + 1
        || &envelope.body[..SESSION_ARBITRATION_MAGIC.len()] != SESSION_ARBITRATION_MAGIC
    {
        return Err(
            ProtocolError::ProtocolMismatch("invalid peer session arbitration message").into(),
        );
    }
    match envelope.body[SESSION_ARBITRATION_MAGIC.len()] {
        1 => Ok(ArbitrationDecision::Accept),
        0 => Ok(ArbitrationDecision::Reject),
        2 => Ok(ArbitrationDecision::AcceptAck),
        3 => Ok(ArbitrationDecision::RejectAck),
        _ => {
            Err(ProtocolError::ProtocolMismatch("invalid peer session arbitration decision").into())
        }
    }
}

async fn receive_arbitration(
    connection: &dyn Connection,
) -> Result<ArbitrationDecision, NetworkError> {
    let envelope = match tokio::time::timeout(SESSION_ARBITRATION_TIMEOUT, connection.recv()).await
    {
        Ok(result) => result?,
        Err(_) => return Err(NetworkError::Timeout),
    };
    decode_arbitration_decision(envelope)
}

async fn send_arbitration(
    connection: &dyn Connection,
    decision: ArbitrationDecision,
) -> Result<(), NetworkError> {
    match tokio::time::timeout(
        SESSION_ARBITRATION_TIMEOUT,
        connection.send(arbitration_envelope(decision)),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(NetworkError::Timeout),
    }
}

async fn close_rejected_session(connection: &dyn Connection) {
    close_physical_session(connection, "rejected").await;
}

async fn close_physical_session(connection: &dyn Connection, reason: &'static str) {
    match tokio::time::timeout(SESSION_CLOSE_TIMEOUT, connection.close()).await {
        Ok(Ok(())) | Ok(Err(NetworkError::Closed)) => {}
        Ok(Err(error)) => {
            tracing::debug!(%error, reason, "physical peer session close failed");
        }
        Err(_) => tracing::warn!(reason, "physical peer session close timed out"),
    }
}

fn fan_out_session(
    connection: Box<dyn Connection>,
    context: PeerConnectionContext,
    handlers: Arc<Vec<PeerLaneHandler>>,
    lease: SessionLease,
) {
    let shared = Arc::new(SequencedConnection::from_box_starting_at(
        connection,
        SESSION_DATA_FIRST_ID,
    ));
    let mut routes = HashMap::new();
    let (session_end, session_end_rx) = watch::channel(false);
    let lane_count = handlers.len();
    // Every lane sends at most one completion signal from Drop. Capacity is
    // therefore a fixed function of the configured lane count, while channel
    // closure still proves every stale handle has actually been dropped.
    let (lane_done, lane_done_rx) = mpsc::channel(lane_count.max(1));

    for registration in handlers.iter() {
        let (sender, receiver) = mpsc::channel(LANE_CAPACITY);
        for kind in &registration.inbound_kinds {
            if routes.insert(*kind, sender.clone()).is_some() {
                tracing::error!(?kind, "duplicate peer-session lane registration");
            }
        }
        let lane = PeerLaneConnection {
            shared: Arc::clone(&shared),
            inbound: Mutex::new(receiver),
            completed: Some(lane_done.clone()),
        };
        (registration.handler)(
            Box::new(lane),
            context.clone().with_session_end(session_end_rx.clone()),
        );
    }
    drop(lane_done);

    let all_lanes_done = if lane_count == 0 {
        None
    } else {
        let (all_lanes_done, all_lanes_done_rx) = oneshot::channel();
        tokio::spawn(monitor_lane_shutdown(
            Arc::clone(&shared),
            lane_done_rx,
            lane_count,
            all_lanes_done,
        ));
        Some(all_lanes_done_rx)
    };

    tokio::spawn(route_inbound(
        shared,
        routes,
        session_end,
        all_lanes_done,
        lane_count,
        lease,
    ));
}

async fn monitor_lane_shutdown(
    connection: Arc<SequencedConnection>,
    mut lane_done: mpsc::Receiver<()>,
    lane_count: usize,
    all_lanes_done: oneshot::Sender<()>,
) {
    let mut completed = 0usize;
    while completed < lane_count {
        match lane_done.recv().await {
            Some(()) => completed += 1,
            // Channel closure also proves every lane-owned sender was dropped,
            // even if a completion notification could not be queued.
            None => break,
        }
    }

    // No runtime lane can use this physical session now. Bound the close
    // attempt, then notify the router so it may terminate a silent recv and
    // release the peer lease. The recv future is cancelled only when the
    // whole session is already terminal, never while another lane is active.
    close_physical_session(connection.as_ref(), "all runtime lanes stopped").await;
    let _ = all_lanes_done.send(());
}

enum RouteEvent {
    Inbound(Result<Envelope, NetworkError>),
    AllLanesStopped,
}

async fn route_inbound(
    connection: Arc<SequencedConnection>,
    mut routes: HashMap<MessageKind, mpsc::Sender<Envelope>>,
    session_end: watch::Sender<bool>,
    mut all_lanes_done: Option<oneshot::Receiver<()>>,
    lane_count: usize,
    lease: SessionLease,
) {
    loop {
        let event = if let Some(done) = all_lanes_done.as_mut() {
            tokio::select! {
                inbound = connection.recv() => RouteEvent::Inbound(inbound),
                _ = done => RouteEvent::AllLanesStopped,
            }
        } else {
            RouteEvent::Inbound(connection.recv().await)
        };

        match event {
            RouteEvent::AllLanesStopped => {
                all_lanes_done = None;
                break;
            }
            RouteEvent::Inbound(Ok(envelope)) => {
                let kind = envelope.kind;
                let Some(route) = routes.get(&kind) else {
                    tracing::debug!(?kind, "peer message has no enabled runtime lane");
                    continue;
                };
                match route.try_send(envelope) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        tracing::warn!(?kind, "peer runtime lane overflow; closing session");
                        close_physical_session(connection.as_ref(), "runtime lane overflow").await;
                        break;
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        tracing::debug!(?kind, "peer runtime lane stopped; closing session");
                        close_physical_session(connection.as_ref(), "runtime lane stopped").await;
                        break;
                    }
                }
            }
            RouteEvent::Inbound(Err(NetworkError::Closed)) => break,
            RouteEvent::Inbound(Err(error)) => {
                tracing::warn!(%error, "peer session receive loop ended");
                break;
            }
        }
    }
    routes.clear();
    session_end.send_replace(true);
    if let Some(mut all_lanes_done) = all_lanes_done
        && tokio::time::timeout(LANE_SHUTDOWN_TIMEOUT, &mut all_lanes_done)
            .await
            .is_err()
    {
        tracing::warn!(lane_count, "peer lanes exceeded session shutdown deadline");
        // The arbiter lease moves with the single waiter. While it is retained,
        // every contender for this peer is rejected before fan-out, so at most
        // one such waiter can exist per peer and memory remains bounded.
        tokio::spawn(retain_lease_until_lane_shutdown(all_lanes_done, lease));
    }
}

async fn retain_lease_until_lane_shutdown(
    all_lanes_done: oneshot::Receiver<()>,
    lease: SessionLease,
) {
    let _ = all_lanes_done.await;
    drop(lease);
}

struct PeerLaneConnection {
    shared: Arc<SequencedConnection>,
    inbound: Mutex<mpsc::Receiver<Envelope>>,
    completed: Option<mpsc::Sender<()>>,
}

impl Drop for PeerLaneConnection {
    fn drop(&mut self) {
        if let Some(completed) = self.completed.take()
            && let Err(error) = completed.try_send(())
            && !matches!(error, mpsc::error::TrySendError::Closed(_))
        {
            tracing::warn!("peer lane completion channel unexpectedly reached capacity");
        }
    }
}

impl std::fmt::Debug for PeerLaneConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerLaneConnection")
            .field("kind", &self.shared.kind())
            .field("peer_addr", &self.shared.peer_addr())
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Connection for PeerLaneConnection {
    fn kind(&self) -> TransportKind {
        self.shared.kind()
    }

    fn peer_addr(&self) -> SocketAddr {
        self.shared.peer_addr()
    }

    fn peer_identity(&self) -> Option<PublicKey> {
        self.shared.peer_identity()
    }

    async fn send(&self, envelope: Envelope) -> Result<(), NetworkError> {
        self.shared.send(envelope).await
    }

    async fn recv(&self) -> Result<Envelope, NetworkError> {
        self.inbound
            .lock()
            .await
            .recv()
            .await
            .ok_or(NetworkError::Closed)
    }

    async fn close(&self) -> Result<(), NetworkError> {
        self.shared.close().await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::net::{Ipv4Addr, SocketAddr};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use async_trait::async_trait;
    use bytes::Bytes;
    use nexkvm_crypto::PublicKey;
    use nexkvm_network::{Connection, NetworkError, TransportKind};
    use nexkvm_protocol::{Envelope, MessageId, MessageKind, PROTOCOL_VERSION};
    use tokio::sync::{mpsc, watch};

    use super::{PeerLaneHandler, compose_peer_handler};
    use crate::connection::{ConnectionOrigin, PeerConnectionContext, PeerConnectionHandler};

    #[derive(Debug, Default)]
    struct MemoryConnection {
        inbound: Mutex<VecDeque<Envelope>>,
        sent: Mutex<Vec<Envelope>>,
    }

    #[derive(Clone)]
    struct SharedMemoryConnection(Arc<MemoryConnection>);

    #[async_trait]
    impl Connection for SharedMemoryConnection {
        fn kind(&self) -> TransportKind {
            TransportKind::Tcp
        }

        fn peer_addr(&self) -> SocketAddr {
            SocketAddr::from((Ipv4Addr::LOCALHOST, 47_654))
        }

        fn peer_identity(&self) -> Option<PublicKey> {
            Some(PublicKey(vec![2; 32]))
        }

        async fn send(&self, envelope: Envelope) -> Result<(), NetworkError> {
            self.0.sent.lock().unwrap().push(envelope);
            Ok(())
        }

        async fn recv(&self) -> Result<Envelope, NetworkError> {
            self.0
                .inbound
                .lock()
                .unwrap()
                .pop_front()
                .ok_or(NetworkError::Closed)
        }

        async fn close(&self) -> Result<(), NetworkError> {
            Ok(())
        }
    }

    fn envelope(kind: MessageKind, body: &'static [u8]) -> Envelope {
        Envelope::new(
            PROTOCOL_VERSION,
            MessageId(999),
            kind,
            Bytes::from_static(body),
        )
    }

    struct DuplexConnection {
        peer_identity: PublicKey,
        marker: u16,
        inbound: tokio::sync::Mutex<mpsc::Receiver<Envelope>>,
        outbound: mpsc::Sender<Envelope>,
        closed: watch::Receiver<bool>,
        close_signal: watch::Sender<bool>,
        close_calls: Arc<AtomicUsize>,
    }

    impl std::fmt::Debug for DuplexConnection {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("DuplexConnection")
                .field("marker", &self.marker)
                .finish_non_exhaustive()
        }
    }

    #[async_trait]
    impl Connection for DuplexConnection {
        fn kind(&self) -> TransportKind {
            TransportKind::Tcp
        }

        fn peer_addr(&self) -> SocketAddr {
            SocketAddr::from((Ipv4Addr::LOCALHOST, self.marker))
        }

        fn peer_identity(&self) -> Option<PublicKey> {
            Some(self.peer_identity.clone())
        }

        async fn send(&self, envelope: Envelope) -> Result<(), NetworkError> {
            if *self.closed.borrow() {
                return Err(NetworkError::Closed);
            }
            self.outbound
                .send(envelope)
                .await
                .map_err(|_| NetworkError::Closed)
        }

        async fn recv(&self) -> Result<Envelope, NetworkError> {
            let mut closed = self.closed.clone();
            let mut inbound = self.inbound.lock().await;
            tokio::select! {
                biased;
                _ = closed.wait_for(|closed| *closed) => Err(NetworkError::Closed),
                envelope = inbound.recv() => envelope.ok_or(NetworkError::Closed),
            }
        }

        async fn close(&self) -> Result<(), NetworkError> {
            self.close_calls.fetch_add(1, Ordering::SeqCst);
            self.close_signal.send_replace(true);
            Ok(())
        }
    }

    #[derive(Clone)]
    struct DuplexControl {
        close_signal: watch::Sender<bool>,
        close_calls: Arc<AtomicUsize>,
    }

    impl DuplexControl {
        fn close(&self) {
            self.close_signal.send_replace(true);
        }
    }

    fn duplex_pair(
        a: &PublicKey,
        b: &PublicKey,
        marker: u16,
    ) -> (Box<dyn Connection>, Box<dyn Connection>, DuplexControl) {
        let (a_to_b, b_inbound) = mpsc::channel(16);
        let (b_to_a, a_inbound) = mpsc::channel(16);
        let (close_signal, closed) = watch::channel(false);
        let close_calls = Arc::new(AtomicUsize::new(0));
        let a_connection = DuplexConnection {
            peer_identity: b.clone(),
            marker,
            inbound: tokio::sync::Mutex::new(a_inbound),
            outbound: a_to_b,
            closed: closed.clone(),
            close_signal: close_signal.clone(),
            close_calls: Arc::clone(&close_calls),
        };
        let b_connection = DuplexConnection {
            peer_identity: a.clone(),
            marker,
            inbound: tokio::sync::Mutex::new(b_inbound),
            outbound: b_to_a,
            closed,
            close_signal: close_signal.clone(),
            close_calls: Arc::clone(&close_calls),
        };
        (
            Box::new(a_connection),
            Box::new(b_connection),
            DuplexControl {
                close_signal,
                close_calls,
            },
        )
    }

    fn retaining_handler(
        lane: &'static str,
        attached: mpsc::UnboundedSender<(&'static str, u16)>,
        ended: Option<mpsc::UnboundedSender<&'static str>>,
    ) -> PeerConnectionHandler {
        Arc::new(move |connection, mut context| {
            attached
                .send((lane, connection.peer_addr().port()))
                .unwrap();
            let ended = ended.clone();
            tokio::spawn(async move {
                context.wait_for_session_end().await;
                drop(connection);
                if let Some(ended) = ended {
                    let _ = ended.send(lane);
                }
            });
        })
    }

    fn dropping_handler(
        attached: mpsc::UnboundedSender<u16>,
        ended: mpsc::UnboundedSender<()>,
    ) -> PeerConnectionHandler {
        Arc::new(move |connection, mut context| {
            attached.send(connection.peer_addr().port()).unwrap();
            drop(connection);
            let ended = ended.clone();
            tokio::spawn(async move {
                context.wait_for_session_end().await;
                let _ = ended.send(());
            });
        })
    }

    fn delayed_shutdown_handler(
        attached: mpsc::UnboundedSender<u16>,
        release: Arc<tokio::sync::Notify>,
        ended: mpsc::UnboundedSender<()>,
    ) -> PeerConnectionHandler {
        Arc::new(move |connection, mut context| {
            attached.send(connection.peer_addr().port()).unwrap();
            let release = Arc::clone(&release);
            let ended = ended.clone();
            tokio::spawn(async move {
                context.wait_for_session_end().await;
                release.notified().await;
                drop(connection);
                let _ = ended.send(());
            });
        })
    }

    fn composed_test_handler(
        identity: PublicKey,
        attached: mpsc::UnboundedSender<(&'static str, u16)>,
        ended: Option<mpsc::UnboundedSender<&'static str>>,
    ) -> PeerConnectionHandler {
        compose_peer_handler(
            identity,
            vec![
                PeerLaneHandler::new(
                    retaining_handler("input", attached.clone(), ended.clone()),
                    [MessageKind::Input],
                ),
                PeerLaneHandler::new(
                    retaining_handler("clipboard", attached, ended),
                    [MessageKind::Clipboard],
                ),
            ],
        )
        .unwrap()
    }

    #[tokio::test]
    async fn simultaneous_cross_dial_selects_one_physical_session_before_lane_fanout() {
        let a = PublicKey(vec![1; 32]);
        let b = PublicKey(vec![2; 32]);
        let (a_attached_tx, mut a_attached_rx) = mpsc::unbounded_channel();
        let (b_attached_tx, mut b_attached_rx) = mpsc::unbounded_channel();
        let a_handler = composed_test_handler(a.clone(), a_attached_tx, None);
        let b_handler = composed_test_handler(b.clone(), b_attached_tx, None);

        // The higher-identity dial arrives first. It must remain pending long
        // enough for the deterministic lower-identity dial to win.
        let (nonpreferred_a, nonpreferred_b, nonpreferred) = duplex_pair(&a, &b, 47_001);
        a_handler(
            nonpreferred_a,
            PeerConnectionContext::new(ConnectionOrigin::Inbound),
        );
        b_handler(
            nonpreferred_b,
            PeerConnectionContext::new(ConnectionOrigin::Outbound),
        );
        tokio::task::yield_now().await;

        let (preferred_a, preferred_b, preferred) = duplex_pair(&a, &b, 47_002);
        a_handler(
            preferred_a,
            PeerConnectionContext::new(ConnectionOrigin::Outbound),
        );
        b_handler(
            preferred_b,
            PeerConnectionContext::new(ConnectionOrigin::Inbound),
        );

        for receiver in [&mut a_attached_rx, &mut b_attached_rx] {
            let first = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
                .await
                .unwrap()
                .unwrap();
            let second = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(first.1, 47_002);
            assert_eq!(second.1, 47_002);
        }
        tokio::time::sleep(Duration::from_millis(350)).await;
        assert!(a_attached_rx.try_recv().is_err());
        assert!(b_attached_rx.try_recv().is_err());
        assert!(nonpreferred.close_calls.load(Ordering::SeqCst) >= 2);
        assert_eq!(preferred.close_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn late_preferred_dial_cannot_replace_an_accepted_fallback_asymmetrically() {
        let a = PublicKey(vec![1; 32]);
        let b = PublicKey(vec![2; 32]);
        let (a_attached_tx, mut a_attached_rx) = mpsc::unbounded_channel();
        let (b_attached_tx, mut b_attached_rx) = mpsc::unbounded_channel();
        let a_handler = composed_test_handler(a.clone(), a_attached_tx, None);
        let b_handler = composed_test_handler(b.clone(), b_attached_tx, None);

        let (fallback_a, fallback_b, fallback) = duplex_pair(&a, &b, 47_101);
        a_handler(
            fallback_a,
            PeerConnectionContext::new(ConnectionOrigin::Inbound),
        );
        b_handler(
            fallback_b,
            PeerConnectionContext::new(ConnectionOrigin::Outbound),
        );
        for receiver in [&mut a_attached_rx, &mut b_attached_rx] {
            for _ in 0..2 {
                let attached = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
                    .await
                    .expect("one-way fallback was not accepted")
                    .expect("fallback attachment channel closed");
                assert_eq!(attached.1, 47_101);
            }
        }

        // Once the authority has accepted the bounded fallback, it alone
        // rejects a late preferred link and the other endpoint follows that
        // authenticated decision. There is no local replace race.
        let (preferred_a, preferred_b, preferred) = duplex_pair(&a, &b, 47_102);
        a_handler(
            preferred_a,
            PeerConnectionContext::new(ConnectionOrigin::Outbound),
        );
        b_handler(
            preferred_b,
            PeerConnectionContext::new(ConnectionOrigin::Inbound),
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(a_attached_rx.try_recv().is_err());
        assert!(b_attached_rx.try_recv().is_err());
        assert_eq!(fallback.close_calls.load(Ordering::SeqCst), 0);
        assert!(preferred.close_calls.load(Ordering::SeqCst) >= 2);
    }

    #[tokio::test]
    async fn session_arbitration_releases_all_lanes_before_reconnect_fanout() {
        let a = PublicKey(vec![1; 32]);
        let b = PublicKey(vec![2; 32]);
        let (a_attached_tx, mut a_attached_rx) = mpsc::unbounded_channel();
        let (b_attached_tx, mut b_attached_rx) = mpsc::unbounded_channel();
        let (a_ended_tx, mut a_ended_rx) = mpsc::unbounded_channel();
        let (b_ended_tx, mut b_ended_rx) = mpsc::unbounded_channel();
        let a_handler = composed_test_handler(a.clone(), a_attached_tx, Some(a_ended_tx));
        let b_handler = composed_test_handler(b.clone(), b_attached_tx, Some(b_ended_tx));

        let (first_a, first_b, first) = duplex_pair(&a, &b, 48_001);
        a_handler(
            first_a,
            PeerConnectionContext::new(ConnectionOrigin::Outbound),
        );
        b_handler(
            first_b,
            PeerConnectionContext::new(ConnectionOrigin::Inbound),
        );
        for receiver in [&mut a_attached_rx, &mut b_attached_rx] {
            for _ in 0..2 {
                assert_eq!(receiver.recv().await.unwrap().1, 48_001);
            }
        }

        first.close();
        for receiver in [&mut a_ended_rx, &mut b_ended_rx] {
            for _ in 0..2 {
                tokio::time::timeout(Duration::from_secs(1), receiver.recv())
                    .await
                    .expect("old lane did not observe physical session end")
                    .expect("old lane end channel closed");
            }
        }

        let (second_a, second_b, second) = duplex_pair(&a, &b, 48_002);
        a_handler(
            second_a,
            PeerConnectionContext::new(ConnectionOrigin::Outbound),
        );
        b_handler(
            second_b,
            PeerConnectionContext::new(ConnectionOrigin::Inbound),
        );
        for receiver in [&mut a_attached_rx, &mut b_attached_rx] {
            for _ in 0..2 {
                let attached = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
                    .await
                    .expect("reconnect was not fanned out")
                    .expect("reconnect attachment channel closed");
                assert_eq!(attached.1, 48_002);
            }
        }
        assert_eq!(second.close_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn locally_stopped_last_lane_closes_silent_session_and_allows_reconnect() {
        let a = PublicKey(vec![1; 32]);
        let b = PublicKey(vec![2; 32]);
        let (a_attached_tx, mut a_attached_rx) = mpsc::unbounded_channel();
        let (b_attached_tx, mut b_attached_rx) = mpsc::unbounded_channel();
        let (a_ended_tx, mut a_ended_rx) = mpsc::unbounded_channel();
        let (b_ended_tx, mut b_ended_rx) = mpsc::unbounded_channel();
        let a_handler = compose_peer_handler(
            a.clone(),
            vec![PeerLaneHandler::new(
                dropping_handler(a_attached_tx, a_ended_tx),
                [MessageKind::FileTransfer],
            )],
        )
        .unwrap();
        let b_handler = compose_peer_handler(
            b.clone(),
            vec![PeerLaneHandler::new(
                dropping_handler(b_attached_tx, b_ended_tx),
                [MessageKind::FileTransfer],
            )],
        )
        .unwrap();

        let (first_a, first_b, first) = duplex_pair(&a, &b, 48_051);
        a_handler(
            first_a,
            PeerConnectionContext::new(ConnectionOrigin::Outbound),
        );
        b_handler(
            first_b,
            PeerConnectionContext::new(ConnectionOrigin::Inbound),
        );
        assert_eq!(a_attached_rx.recv().await.unwrap(), 48_051);
        assert_eq!(b_attached_rx.recv().await.unwrap(), 48_051);

        for ended in [&mut a_ended_rx, &mut b_ended_rx] {
            tokio::time::timeout(Duration::from_secs(1), ended.recv())
                .await
                .expect("locally stopped lane did not end the silent physical session")
                .expect("session-end observer channel closed");
        }
        assert!(first.close_calls.load(Ordering::SeqCst) >= 1);

        let (second_a, second_b, second) = duplex_pair(&a, &b, 48_052);
        a_handler(
            second_a,
            PeerConnectionContext::new(ConnectionOrigin::Outbound),
        );
        b_handler(
            second_b,
            PeerConnectionContext::new(ConnectionOrigin::Inbound),
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), a_attached_rx.recv())
                .await
                .expect("reconnect was not accepted after local lane shutdown")
                .expect("attachment channel closed"),
            48_052
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), b_attached_rx.recv())
                .await
                .expect("peer reconnect was not accepted after local lane shutdown")
                .expect("peer attachment channel closed"),
            48_052
        );
        assert!(
            second.close_calls.load(Ordering::SeqCst) >= 1,
            "the reconnect has the same terminal lane policy"
        );
    }

    #[tokio::test]
    async fn stale_lane_past_shutdown_timeout_keeps_peer_lease_until_handle_drops() {
        let a = PublicKey(vec![1; 32]);
        let b = PublicKey(vec![2; 32]);
        let (a_attached_tx, mut a_attached_rx) = mpsc::unbounded_channel();
        let (b_attached_tx, mut b_attached_rx) = mpsc::unbounded_channel();
        let (a_ended_tx, mut a_ended_rx) = mpsc::unbounded_channel();
        let release_a = Arc::new(tokio::sync::Notify::new());
        let a_handler = compose_peer_handler(
            a.clone(),
            vec![PeerLaneHandler::new(
                delayed_shutdown_handler(a_attached_tx, Arc::clone(&release_a), a_ended_tx),
                [MessageKind::Input],
            )],
        )
        .unwrap();
        let b_handler = compose_peer_handler(
            b.clone(),
            vec![PeerLaneHandler::new(
                retaining_handler("input", b_attached_tx, None),
                [MessageKind::Input],
            )],
        )
        .unwrap();

        let (first_a, first_b, first) = duplex_pair(&a, &b, 48_101);
        a_handler(
            first_a,
            PeerConnectionContext::new(ConnectionOrigin::Outbound),
        );
        b_handler(
            first_b,
            PeerConnectionContext::new(ConnectionOrigin::Inbound),
        );
        assert_eq!(a_attached_rx.recv().await.unwrap(), 48_101);
        assert_eq!(b_attached_rx.recv().await.unwrap().1, 48_101);

        first.close();
        tokio::time::sleep(super::LANE_SHUTDOWN_TIMEOUT + Duration::from_millis(100)).await;

        // The route task's bounded cleanup wait has expired, but A's old lane
        // still owns its physical-session handle. A reconnect must be rejected
        // before fan-out on both endpoints.
        let (blocked_a, blocked_b, blocked) = duplex_pair(&a, &b, 48_102);
        a_handler(
            blocked_a,
            PeerConnectionContext::new(ConnectionOrigin::Outbound),
        );
        b_handler(
            blocked_b,
            PeerConnectionContext::new(ConnectionOrigin::Inbound),
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(150), a_attached_rx.recv())
                .await
                .is_err()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(150), b_attached_rx.recv())
                .await
                .is_err()
        );
        assert!(blocked.close_calls.load(Ordering::SeqCst) >= 2);

        release_a.notify_waiters();
        tokio::time::timeout(Duration::from_secs(1), a_ended_rx.recv())
            .await
            .expect("stale lane did not release")
            .expect("stale-lane completion channel closed");
        tokio::task::yield_now().await;

        let (next_a, next_b, next) = duplex_pair(&a, &b, 48_103);
        a_handler(
            next_a,
            PeerConnectionContext::new(ConnectionOrigin::Outbound),
        );
        b_handler(
            next_b,
            PeerConnectionContext::new(ConnectionOrigin::Inbound),
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), a_attached_rx.recv())
                .await
                .expect("reconnect after stale lane release was not accepted")
                .expect("attachment channel closed"),
            48_103
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), b_attached_rx.recv())
                .await
                .expect("peer reconnect after stale lane release was not accepted")
                .expect("peer attachment channel closed")
                .1,
            48_103
        );
        assert_eq!(next.close_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn interleaved_messages_reach_exactly_one_matching_lane() {
        let memory = Arc::new(MemoryConnection::default());
        memory.inbound.lock().unwrap().extend([
            super::arbitration_envelope(super::ArbitrationDecision::AcceptAck),
            envelope(MessageKind::Clipboard, b"clip"),
            envelope(MessageKind::Input, b"input"),
        ]);

        let (received_tx, mut received_rx) = mpsc::channel(2);
        let input_handler: PeerConnectionHandler = {
            let received_tx = received_tx.clone();
            Arc::new(move |connection, _context| {
                let received_tx = received_tx.clone();
                tokio::spawn(async move {
                    let envelope = connection.recv().await.unwrap();
                    received_tx.send(("input", envelope)).await.unwrap();
                });
            })
        };
        let clipboard_handler: PeerConnectionHandler = Arc::new(move |connection, _context| {
            let received_tx = received_tx.clone();
            tokio::spawn(async move {
                let envelope = connection.recv().await.unwrap();
                received_tx.send(("clipboard", envelope)).await.unwrap();
            });
        });

        let composed = compose_peer_handler(
            PublicKey(vec![1; 32]),
            vec![
                PeerLaneHandler::new(input_handler, [MessageKind::Input]),
                PeerLaneHandler::new(clipboard_handler, [MessageKind::Clipboard]),
            ],
        )
        .unwrap();
        composed(
            Box::new(SharedMemoryConnection(memory)),
            PeerConnectionContext::new(ConnectionOrigin::Outbound),
        );

        let first = received_rx.recv().await.unwrap();
        let second = received_rx.recv().await.unwrap();
        let mut received = [first, second];
        received.sort_by_key(|(lane, _)| *lane);
        assert_eq!(received[0].0, "clipboard");
        assert_eq!(received[0].1.kind, MessageKind::Clipboard);
        assert_eq!(received[1].0, "input");
        assert_eq!(received[1].1.kind, MessageKind::Input);
    }

    #[tokio::test]
    async fn every_outbound_lane_shares_one_message_id_sequence() {
        let memory = Arc::new(MemoryConnection::default());
        memory
            .inbound
            .lock()
            .unwrap()
            .push_back(super::arbitration_envelope(
                super::ArbitrationDecision::AcceptAck,
            ));
        let (done_tx, mut done_rx) = mpsc::channel(2);

        let sender = |kind: MessageKind, done_tx: mpsc::Sender<()>| -> PeerConnectionHandler {
            Arc::new(move |connection, _context| {
                let done_tx = done_tx.clone();
                tokio::spawn(async move {
                    connection.send(envelope(kind, b"outbound")).await.unwrap();
                    done_tx.send(()).await.unwrap();
                });
            })
        };
        let composed = compose_peer_handler(
            PublicKey(vec![1; 32]),
            vec![
                PeerLaneHandler::new(sender(MessageKind::Input, done_tx.clone()), []),
                PeerLaneHandler::new(sender(MessageKind::Clipboard, done_tx), []),
            ],
        )
        .unwrap();
        composed(
            Box::new(SharedMemoryConnection(Arc::clone(&memory))),
            PeerConnectionContext::new(ConnectionOrigin::Outbound),
        );

        done_rx.recv().await.unwrap();
        done_rx.recv().await.unwrap();
        let mut ids: Vec<_> = memory
            .sent
            .lock()
            .unwrap()
            .iter()
            .map(|envelope| envelope.id.0)
            .collect();
        ids.sort_unstable();
        assert_eq!(ids, [0, 1, 2]);
    }
}
