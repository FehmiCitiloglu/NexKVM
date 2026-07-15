use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use nexkvm_core::identity::DeviceId;
use nexkvm_crypto::{DeviceKeypair, PublicKey};
use nexkvm_discovery::{DiscoveryService, ReconnectTarget};
use nexkvm_network::{
    Connection, NetworkError, SecureConnection, Transport, TransportKind, establish_trusted_session,
};
use nexkvm_protocol::{Envelope, ProtocolError};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch};
use tracing::{debug, info, warn};

const EXPLICIT_RECONNECT_DELAY: Duration = Duration::from_secs(2);
const INITIAL_ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(10);
const MAX_ACCEPT_ERROR_BACKOFF: Duration = Duration::from_secs(1);
const MAX_PENDING_INBOUND_HANDSHAKES: usize = 32;

pub type PeerConnectionHandler =
    Arc<dyn Fn(Box<dyn Connection>, PeerConnectionContext) + Send + Sync>;

/// Whether this daemon accepted or initiated the physical peer connection.
///
/// Session arbitration combines this direction with the ordered authenticated
/// device identities so both endpoints classify the same cross-dial candidate
/// identically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionOrigin {
    Inbound,
    Outbound,
}

/// Metadata and lifetime notification carried with one physical peer session.
#[derive(Debug, Clone)]
pub struct PeerConnectionContext {
    origin: ConnectionOrigin,
    session_end: Option<watch::Receiver<bool>>,
}

impl PeerConnectionContext {
    #[must_use]
    pub fn new(origin: ConnectionOrigin) -> Self {
        Self {
            origin,
            session_end: None,
        }
    }

    #[must_use]
    pub fn origin(&self) -> ConnectionOrigin {
        self.origin
    }

    pub(crate) fn with_session_end(mut self, session_end: watch::Receiver<bool>) -> Self {
        self.session_end = Some(session_end);
        self
    }

    /// Wait until the composed session router observes physical-link closure.
    pub async fn wait_for_session_end(&mut self) {
        let Some(session_end) = self.session_end.as_mut() else {
            std::future::pending::<()>().await;
            return;
        };
        if *session_end.borrow() {
            return;
        }
        while session_end.changed().await.is_ok() {
            if *session_end.borrow_and_update() {
                return;
            }
        }
    }
}

/// Trusted-session material used to secure raw transport connections.
#[derive(Debug, Clone)]
pub struct TrustedSessionConfig {
    local_identity: DeviceKeypair,
    trusted_peer_keys: Arc<Vec<PublicKey>>,
}

impl TrustedSessionConfig {
    pub fn new(local_identity: DeviceKeypair, trusted_peer_keys: Vec<PublicKey>) -> Self {
        Self {
            local_identity,
            trusted_peer_keys: Arc::new(trusted_peer_keys),
        }
    }
}

/// A successful outbound reconnect attempt.
pub struct ConnectedPeer {
    pub device_id: DeviceId,
    pub device_name: String,
    pub addr: SocketAddr,
    pub transport: TransportKind,
    pub connection: Box<dyn Connection>,
}

/// Dial one trusted rediscovery target over the configured transport.
pub async fn connect_reconnect_target(
    transport: Arc<dyn Transport>,
    target: ReconnectTarget,
    session_config: Option<TrustedSessionConfig>,
) -> Result<ConnectedPeer, NetworkError> {
    let addr = target.device.addr;
    let connection = secure_connection(transport.connect(addr).await?, session_config).await?;
    Ok(ConnectedPeer {
        device_id: target.device.info.id,
        device_name: target.device.info.name,
        addr,
        transport: connection.kind(),
        connection,
    })
}

/// Dial a configured peer address over the selected transport.
pub async fn connect_explicit_addr(
    transport: Arc<dyn Transport>,
    addr: SocketAddr,
    session_config: Option<TrustedSessionConfig>,
) -> Result<Box<dyn Connection>, NetworkError> {
    secure_connection(transport.connect(addr).await?, session_config).await
}

/// Resolve and dial a configured peer endpoint over the selected transport.
pub async fn connect_explicit_endpoint(
    transport: Arc<dyn Transport>,
    endpoint: &str,
    session_config: Option<TrustedSessionConfig>,
) -> Result<(SocketAddr, Box<dyn Connection>), NetworkError> {
    let mut last_error = None;
    let addrs = tokio::net::lookup_host(endpoint)
        .await
        .map_err(NetworkError::Io)?;

    for addr in addrs {
        match connect_explicit_addr(Arc::clone(&transport), addr, session_config.clone()).await {
            Ok(connection) => return Ok((addr, connection)),
            Err(error) => last_error = Some(error),
        }
    }

    Err(last_error.unwrap_or(NetworkError::AllTransportsFailed))
}

/// Accept inbound links for the daemon lifetime.
pub fn spawn_inbound_accept_loop(
    transport: Arc<dyn Transport>,
    handler: Option<PeerConnectionHandler>,
    session_config: Option<TrustedSessionConfig>,
) {
    tokio::spawn(run_inbound_accept_loop(transport, handler, session_config));
}

async fn run_inbound_accept_loop(
    transport: Arc<dyn Transport>,
    handler: Option<PeerConnectionHandler>,
    session_config: Option<TrustedSessionConfig>,
) {
    let admission = inbound_handshake_admission();
    let mut error_backoff = AcceptErrorBackoff::default();

    loop {
        match transport.accept().await {
            Ok(connection) => {
                error_backoff.reset();
                let peer = connection.peer_addr();
                let kind = connection.kind();
                let Some(permit) = try_admit_inbound_handshake(&admission) else {
                    warn!(
                        %peer,
                        ?kind,
                        limit = MAX_PENDING_INBOUND_HANDSHAKES,
                        "inbound handshake capacity exhausted; dropping connection"
                    );
                    drop(connection);
                    continue;
                };

                info!(%peer, ?kind, "accepted peer connection");
                let session_config = session_config.clone();
                let handler = handler.clone();
                tokio::spawn(async move {
                    let secured = secure_connection(connection, session_config).await;
                    // Admission limits only unauthenticated handshakes. A live,
                    // authenticated session must not consume this permit.
                    drop(permit);
                    match secured {
                        Ok(connection) => {
                            run_managed_session(connection, handler, ConnectionOrigin::Inbound)
                                .await;
                        }
                        Err(error) => log_handshake_failure(&error),
                    }
                });
            }
            Err(error) => {
                let delay = error_backoff.next_delay();
                warn!(%error, ?delay, "failed to accept peer connection");
                tokio::time::sleep(delay).await;
            }
        }
    }
}

/// Keep a manually configured peer connected for the daemon lifetime.
pub fn spawn_explicit_connect_driver(
    transport: Arc<dyn Transport>,
    endpoint: String,
    session_config: Option<TrustedSessionConfig>,
    handler: Option<PeerConnectionHandler>,
) {
    tokio::spawn(run_explicit_connect_driver(
        transport,
        endpoint,
        session_config,
        handler,
        EXPLICIT_RECONNECT_DELAY,
    ));
}

async fn run_explicit_connect_driver(
    transport: Arc<dyn Transport>,
    endpoint: String,
    session_config: Option<TrustedSessionConfig>,
    handler: Option<PeerConnectionHandler>,
    reconnect_delay: Duration,
) {
    let mut attempt: u64 = 0;
    loop {
        match connect_explicit_endpoint(
            Arc::clone(&transport),
            endpoint.as_str(),
            session_config.clone(),
        )
        .await
        {
            Ok((addr, connection)) => {
                info!(endpoint = %endpoint, %addr, attempt, "explicit peer connected");
                attempt = 0;
                run_managed_session(connection, handler.clone(), ConnectionOrigin::Outbound).await;
                info!(endpoint = %endpoint, %addr, "explicit peer session ended; reconnecting");
            }
            Err(error) => {
                warn!(endpoint = %endpoint, attempt, %error, "explicit peer connect failed");
                attempt = attempt.saturating_add(1);
            }
        }
        tokio::time::sleep(reconnect_delay).await;
    }
}

fn log_handshake_failure(error: &NetworkError) {
    if matches!(
        error,
        NetworkError::Protocol(ProtocolError::ProtocolMismatch(_))
    ) {
        debug!(%error, "non-nexkvm probe dropped");
    } else {
        warn!(%error, "trusted session handshake failed");
    }
}

/// Drive trusted rediscovery targets into real transport connections.
pub fn spawn_reconnect_driver(
    service: Arc<DiscoveryService>,
    transport: Arc<dyn Transport>,
    mut targets: mpsc::Receiver<ReconnectTarget>,
    session_config: Option<TrustedSessionConfig>,
    handler: Option<PeerConnectionHandler>,
) {
    tokio::spawn(async move {
        while let Some(target) = targets.recv().await {
            let device_id = target.device.info.id;
            let attempt = target.attempt;
            match connect_reconnect_target(Arc::clone(&transport), target, session_config.clone())
                .await
            {
                Ok(peer) => {
                    info!(
                        device = %peer.device_name,
                        addr = %peer.addr,
                        ?peer.transport,
                        attempt,
                        "trusted peer connected"
                    );
                    service.report_success(peer.device_id);
                    let service = Arc::clone(&service);
                    let handler = handler.clone();
                    tokio::spawn(async move {
                        run_managed_session(peer.connection, handler, ConnectionOrigin::Outbound)
                            .await;
                        info!(device = %peer.device_name, "trusted peer session ended; reconnecting");
                        service.report_failure(peer.device_id);
                    });
                }
                Err(error) => {
                    warn!(%device_id, attempt, %error, "trusted peer reconnect failed");
                    service.report_failure(device_id);
                }
            }
        }
    });
}

async fn run_managed_session(
    connection: Box<dyn Connection>,
    handler: Option<PeerConnectionHandler>,
    origin: ConnectionOrigin,
) {
    if let Some(handler) = handler {
        let (connection, completion) = tracked_session(connection);
        handler(connection, PeerConnectionContext::new(origin));
        completion.wait().await;
    } else {
        hold_connection_until_closed(connection).await;
    }
}

fn tracked_session(connection: Box<dyn Connection>) -> (Box<dyn Connection>, SessionCompletion) {
    let (ended, receiver) = watch::channel(false);
    (
        Box::new(SessionTrackedConnection {
            inner: connection,
            ended,
        }),
        SessionCompletion { receiver },
    )
}

#[derive(Debug)]
struct SessionCompletion {
    receiver: watch::Receiver<bool>,
}

impl SessionCompletion {
    async fn wait(mut self) {
        if *self.receiver.borrow() {
            return;
        }
        let _ = self.receiver.changed().await;
    }
}

struct SessionTrackedConnection {
    inner: Box<dyn Connection>,
    ended: watch::Sender<bool>,
}

impl SessionTrackedConnection {
    fn signal_ended(&self) {
        self.ended.send_replace(true);
    }
}

impl std::fmt::Debug for SessionTrackedConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionTrackedConnection")
            .field("kind", &self.inner.kind())
            .field("peer_addr", &self.inner.peer_addr())
            .finish_non_exhaustive()
    }
}

impl Drop for SessionTrackedConnection {
    fn drop(&mut self) {
        self.signal_ended();
    }
}

#[async_trait]
impl Connection for SessionTrackedConnection {
    fn kind(&self) -> TransportKind {
        self.inner.kind()
    }

    fn peer_addr(&self) -> SocketAddr {
        self.inner.peer_addr()
    }

    fn peer_identity(&self) -> Option<PublicKey> {
        self.inner.peer_identity()
    }

    async fn send(&self, envelope: Envelope) -> Result<(), NetworkError> {
        let result = self.inner.send(envelope).await;
        if result.is_err() {
            self.signal_ended();
        }
        result
    }

    async fn recv(&self) -> Result<Envelope, NetworkError> {
        let result = self.inner.recv().await;
        if result.is_err() {
            self.signal_ended();
        }
        result
    }

    async fn close(&self) -> Result<(), NetworkError> {
        let result = self.inner.close().await;
        self.signal_ended();
        result
    }
}

fn inbound_handshake_admission() -> Arc<Semaphore> {
    Arc::new(Semaphore::new(MAX_PENDING_INBOUND_HANDSHAKES))
}

fn try_admit_inbound_handshake(admission: &Arc<Semaphore>) -> Option<OwnedSemaphorePermit> {
    Arc::clone(admission).try_acquire_owned().ok()
}

#[derive(Debug)]
struct AcceptErrorBackoff {
    next: Duration,
}

impl Default for AcceptErrorBackoff {
    fn default() -> Self {
        Self {
            next: INITIAL_ACCEPT_ERROR_BACKOFF,
        }
    }
}

impl AcceptErrorBackoff {
    fn next_delay(&mut self) -> Duration {
        let delay = self.next;
        self.next = self.next.saturating_mul(2).min(MAX_ACCEPT_ERROR_BACKOFF);
        delay
    }

    fn reset(&mut self) {
        self.next = INITIAL_ACCEPT_ERROR_BACKOFF;
    }
}

async fn secure_connection(
    connection: Box<dyn Connection>,
    session_config: Option<TrustedSessionConfig>,
) -> Result<Box<dyn Connection>, NetworkError> {
    match session_config {
        Some(config) => {
            let secure: SecureConnection = establish_trusted_session(
                connection,
                config.local_identity,
                config.trusted_peer_keys.as_ref(),
            )
            .await?;
            Ok(Box::new(secure))
        }
        None => Ok(connection),
    }
}

async fn hold_connection_until_closed(connection: Box<dyn Connection>) {
    loop {
        match connection.recv().await {
            Ok(envelope) => debug!(
                id = envelope.id.0,
                kind = ?envelope.kind,
                "received peer envelope before session router is attached"
            ),
            Err(NetworkError::Closed) => break,
            Err(error) => {
                warn!(%error, "peer connection closed with error");
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use async_trait::async_trait;
    use nexkvm_core::identity::{DeviceInfo, OsKind};
    use nexkvm_crypto::DeviceKeypair;
    use nexkvm_discovery::{
        DiscoveredDevice, Discovery, DiscoveryError, DiscoveryService, ReconnectPolicy,
        ReconnectTarget, ServiceConfig, TrustOracle,
    };
    use nexkvm_network::{Connection, NetworkError, TcpTransport, Transport, TransportKind};
    use nexkvm_protocol::Envelope;

    fn loopback() -> SocketAddr {
        "127.0.0.1:0".parse().unwrap()
    }

    fn target(addr: SocketAddr) -> ReconnectTarget {
        ReconnectTarget {
            device: DiscoveredDevice {
                info: DeviceInfo::new("linux-peer", OsKind::Linux),
                addr,
                fingerprint: Some("aa:bb".into()),
            },
            attempt: 0,
        }
    }

    #[derive(Debug)]
    struct IdleConnection {
        peer: SocketAddr,
    }

    #[async_trait]
    impl Connection for IdleConnection {
        fn kind(&self) -> TransportKind {
            TransportKind::Tcp
        }

        fn peer_addr(&self) -> SocketAddr {
            self.peer
        }

        async fn send(&self, _envelope: Envelope) -> Result<(), NetworkError> {
            Ok(())
        }

        async fn recv(&self) -> Result<Envelope, NetworkError> {
            std::future::pending().await
        }

        async fn close(&self) -> Result<(), NetworkError> {
            Ok(())
        }
    }

    #[derive(Debug)]
    struct ClosedConnection;

    #[async_trait]
    impl Connection for ClosedConnection {
        fn kind(&self) -> TransportKind {
            TransportKind::Tcp
        }

        fn peer_addr(&self) -> SocketAddr {
            "127.0.0.1:47654".parse().unwrap()
        }

        async fn send(&self, _envelope: Envelope) -> Result<(), NetworkError> {
            Ok(())
        }

        async fn recv(&self) -> Result<Envelope, NetworkError> {
            Err(NetworkError::Closed)
        }

        async fn close(&self) -> Result<(), NetworkError> {
            Ok(())
        }
    }

    struct PendingHandshakeConnection {
        dropped: Arc<AtomicUsize>,
    }

    impl Drop for PendingHandshakeConnection {
        fn drop(&mut self) {
            self.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl Connection for PendingHandshakeConnection {
        fn kind(&self) -> TransportKind {
            TransportKind::Tcp
        }

        fn peer_addr(&self) -> SocketAddr {
            "127.0.0.1:47654".parse().unwrap()
        }

        async fn send(&self, _envelope: Envelope) -> Result<(), NetworkError> {
            Ok(())
        }

        async fn recv(&self) -> Result<Envelope, NetworkError> {
            std::future::pending().await
        }

        async fn close(&self) -> Result<(), NetworkError> {
            Ok(())
        }
    }

    struct QueueTransport {
        connections: Mutex<VecDeque<Box<dyn Connection>>>,
        accepts: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Transport for QueueTransport {
        fn kind(&self) -> TransportKind {
            TransportKind::Tcp
        }

        async fn connect(&self, _addr: SocketAddr) -> Result<Box<dyn Connection>, NetworkError> {
            std::future::pending().await
        }

        async fn accept(&self) -> Result<Box<dyn Connection>, NetworkError> {
            let connection = self.connections.lock().unwrap().pop_front();
            match connection {
                Some(connection) => {
                    self.accepts.fetch_add(1, Ordering::SeqCst);
                    Ok(connection)
                }
                None => std::future::pending().await,
            }
        }
    }

    #[derive(Debug)]
    struct CountingTransport {
        peer: SocketAddr,
        connects: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Transport for CountingTransport {
        fn kind(&self) -> TransportKind {
            TransportKind::Tcp
        }

        async fn connect(&self, _addr: SocketAddr) -> Result<Box<dyn Connection>, NetworkError> {
            self.connects.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(IdleConnection { peer: self.peer }))
        }

        async fn accept(&self) -> Result<Box<dyn Connection>, NetworkError> {
            std::future::pending().await
        }
    }

    #[derive(Debug)]
    struct FixedDiscovery {
        peers: Vec<DiscoveredDevice>,
    }

    #[async_trait]
    impl Discovery for FixedDiscovery {
        async fn advertise(
            &self,
            _info: &DeviceInfo,
            _addr: SocketAddr,
            _fingerprint: Option<&str>,
        ) -> Result<(), DiscoveryError> {
            Ok(())
        }

        async fn discovered(&self) -> Result<Vec<DiscoveredDevice>, DiscoveryError> {
            Ok(self.peers.clone())
        }
    }

    #[derive(Debug)]
    struct TrustAll;

    impl TrustOracle for TrustAll {
        fn is_trusted(&self, _device: &DiscoveredDevice) -> bool {
            true
        }
    }

    async fn wait_for_count(counter: &AtomicUsize, expected: usize) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while counter.load(Ordering::SeqCst) < expected {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("counter did not reach expected value");
    }

    #[tokio::test]
    async fn reconnect_target_dials_real_tcp_transport() {
        let server = TcpTransport::bind(loopback()).await.unwrap();
        let addr = server.local_addr().unwrap();
        let accept = tokio::spawn(async move { server.accept().await.unwrap() });

        let client: Arc<dyn Transport> = Arc::new(TcpTransport::bind(loopback()).await.unwrap());
        let attempt = super::connect_reconnect_target(client, target(addr), None)
            .await
            .unwrap();

        assert_eq!(attempt.device_name, "linux-peer");
        assert_eq!(attempt.addr, addr);
        assert_eq!(attempt.transport, TransportKind::Tcp);

        let server_conn = accept.await.unwrap();
        assert_eq!(server_conn.kind(), TransportKind::Tcp);
    }

    #[tokio::test]
    async fn explicit_addr_dials_real_tcp_transport() {
        let server = TcpTransport::bind(loopback()).await.unwrap();
        let addr = server.local_addr().unwrap();
        let accept = tokio::spawn(async move { server.accept().await.unwrap() });

        let client: Arc<dyn Transport> = Arc::new(TcpTransport::bind(loopback()).await.unwrap());
        let connection = super::connect_explicit_addr(client, addr, None)
            .await
            .unwrap();

        assert_eq!(connection.peer_addr(), addr);
        assert_eq!(connection.kind(), TransportKind::Tcp);

        let server_conn = accept.await.unwrap();
        assert_eq!(server_conn.kind(), TransportKind::Tcp);
    }

    #[tokio::test]
    async fn explicit_endpoint_dials_real_tcp_transport() {
        let server = TcpTransport::bind(loopback()).await.unwrap();
        let addr = server.local_addr().unwrap();
        let accept = tokio::spawn(async move { server.accept().await.unwrap() });

        let client: Arc<dyn Transport> = Arc::new(TcpTransport::bind(loopback()).await.unwrap());
        let (resolved_addr, connection) =
            super::connect_explicit_endpoint(client, &addr.to_string(), None)
                .await
                .unwrap();

        assert_eq!(resolved_addr, addr);
        assert_eq!(connection.peer_addr(), addr);
        assert_eq!(connection.kind(), TransportKind::Tcp);

        let server_conn = accept.await.unwrap();
        assert_eq!(server_conn.kind(), TransportKind::Tcp);
    }

    #[tokio::test]
    async fn explicit_driver_reconnects_after_managed_session_ends() {
        let connects = Arc::new(AtomicUsize::new(0));
        let transport: Arc<dyn Transport> = Arc::new(CountingTransport {
            peer: "127.0.0.1:47654".parse().unwrap(),
            connects: Arc::clone(&connects),
        });
        let release = Arc::new(tokio::sync::Notify::new());
        let (attached_tx, mut attached_rx) = tokio::sync::mpsc::channel(2);
        let handler: super::PeerConnectionHandler = {
            let release = Arc::clone(&release);
            Arc::new(move |connection, _context| {
                let release = Arc::clone(&release);
                let attached_tx = attached_tx.clone();
                tokio::spawn(async move {
                    attached_tx.send(()).await.unwrap();
                    release.notified().await;
                    drop(connection);
                });
            })
        };

        let driver = tokio::spawn(super::run_explicit_connect_driver(
            transport,
            "127.0.0.1:47654".into(),
            None,
            Some(handler),
            Duration::from_millis(1),
        ));

        wait_for_count(&connects, 1).await;
        attached_rx.recv().await.unwrap();
        tokio::task::yield_now().await;
        assert_eq!(connects.load(Ordering::SeqCst), 1);
        release.notify_one();
        wait_for_count(&connects, 2).await;
        driver.abort();
    }

    #[tokio::test]
    async fn discovery_driver_reports_session_end_and_reconnects() {
        let discovered = target("127.0.0.1:47654".parse().unwrap()).device;
        let service = Arc::new(DiscoveryService::new(
            Arc::new(FixedDiscovery {
                peers: vec![discovered],
            }),
            Arc::new(TrustAll),
            ServiceConfig {
                poll_interval: Duration::from_millis(2),
                reconnect: ReconnectPolicy {
                    base: Duration::from_millis(2),
                    max: Duration::from_millis(10),
                    multiplier: 1.0,
                },
                channel_capacity: 4,
            },
        ));
        let targets = service
            .start(
                &DeviceInfo::new("self", OsKind::MacOs),
                "127.0.0.1:47655".parse().unwrap(),
                None,
            )
            .await
            .unwrap();
        let connects = Arc::new(AtomicUsize::new(0));
        let transport: Arc<dyn Transport> = Arc::new(CountingTransport {
            peer: "127.0.0.1:47654".parse().unwrap(),
            connects: Arc::clone(&connects),
        });
        let release = Arc::new(tokio::sync::Notify::new());
        let (attached_tx, mut attached_rx) = tokio::sync::mpsc::channel(2);
        let handler: super::PeerConnectionHandler = {
            let release = Arc::clone(&release);
            Arc::new(move |connection, _context| {
                let release = Arc::clone(&release);
                let attached_tx = attached_tx.clone();
                tokio::spawn(async move {
                    attached_tx.send(()).await.unwrap();
                    release.notified().await;
                    drop(connection);
                });
            })
        };

        super::spawn_reconnect_driver(
            Arc::clone(&service),
            transport,
            targets,
            None,
            Some(handler),
        );

        wait_for_count(&connects, 1).await;
        attached_rx.recv().await.unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(connects.load(Ordering::SeqCst), 1);
        release.notify_one();
        wait_for_count(&connects, 2).await;
    }

    #[tokio::test]
    async fn managed_session_ends_on_receive_close_before_handler_drops_connection() {
        let release = Arc::new(tokio::sync::Notify::new());
        let (receive_finished_tx, mut receive_finished_rx) = tokio::sync::mpsc::channel(1);
        let handler: super::PeerConnectionHandler = {
            let release = Arc::clone(&release);
            Arc::new(move |connection, _context| {
                let release = Arc::clone(&release);
                let receive_finished_tx = receive_finished_tx.clone();
                tokio::spawn(async move {
                    assert!(matches!(connection.recv().await, Err(NetworkError::Closed)));
                    receive_finished_tx.send(()).await.unwrap();
                    release.notified().await;
                    drop(connection);
                });
            })
        };

        let session = tokio::spawn(super::run_managed_session(
            Box::new(ClosedConnection),
            Some(handler),
            super::ConnectionOrigin::Inbound,
        ));
        receive_finished_rx.recv().await.unwrap();
        tokio::time::timeout(Duration::from_millis(100), session)
            .await
            .expect("receive closure must end the managed session")
            .unwrap();
        release.notify_one();
    }

    #[tokio::test]
    async fn inbound_admission_is_strictly_bounded() {
        let admission = super::inbound_handshake_admission();
        let mut permits = VecDeque::new();
        for _ in 0..super::MAX_PENDING_INBOUND_HANDSHAKES {
            permits.push_back(
                super::try_admit_inbound_handshake(&admission)
                    .expect("capacity must admit a handshake"),
            );
        }
        assert!(super::try_admit_inbound_handshake(&admission).is_none());

        drop(permits.pop_front());
        assert!(super::try_admit_inbound_handshake(&admission).is_some());
    }

    #[tokio::test]
    async fn accept_loop_drops_connections_above_pre_auth_capacity() {
        let accepts = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        let connections = (0..=super::MAX_PENDING_INBOUND_HANDSHAKES)
            .map(|_| {
                Box::new(PendingHandshakeConnection {
                    dropped: Arc::clone(&dropped),
                }) as Box<dyn Connection>
            })
            .collect();
        let transport: Arc<dyn Transport> = Arc::new(QueueTransport {
            connections: Mutex::new(connections),
            accepts: Arc::clone(&accepts),
        });
        let runner = tokio::spawn(super::run_inbound_accept_loop(
            transport,
            None,
            Some(super::TrustedSessionConfig::new(
                DeviceKeypair::from_seed([7; 32]),
                Vec::new(),
            )),
        ));

        wait_for_count(&accepts, super::MAX_PENDING_INBOUND_HANDSHAKES + 1).await;
        wait_for_count(&dropped, 1).await;
        assert_eq!(
            dropped.load(Ordering::SeqCst),
            1,
            "only the excess unauthenticated connection should be dropped"
        );
        runner.abort();
    }

    #[test]
    fn accept_error_backoff_is_bounded_and_resets_after_success() {
        let mut backoff = super::AcceptErrorBackoff::default();
        let first = backoff.next_delay();
        let second = backoff.next_delay();
        assert!(second > first);

        let mut last = second;
        for _ in 0..32 {
            last = backoff.next_delay();
        }
        assert_eq!(last, super::MAX_ACCEPT_ERROR_BACKOFF);

        backoff.reset();
        assert_eq!(backoff.next_delay(), first);
    }
}
