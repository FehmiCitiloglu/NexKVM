use std::net::SocketAddr;
use std::sync::Arc;

use nexkvm_core::identity::DeviceId;
use nexkvm_crypto::{DeviceKeypair, PublicKey};
use nexkvm_discovery::{DiscoveryService, ReconnectTarget};
use nexkvm_network::{
    Connection, NetworkError, SecureConnection, Transport, TransportKind, establish_trusted_session,
};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

pub type PeerConnectionHandler = Arc<dyn Fn(Box<dyn Connection>) + Send + Sync>;

/// Trusted-session material used to secure raw transport connections.
#[derive(Debug, Clone)]
pub struct TrustedSessionConfig {
    local_identity: DeviceKeypair,
    local_challenge: [u8; 32],
    trusted_peer_keys: Arc<Vec<PublicKey>>,
}

impl TrustedSessionConfig {
    pub fn new(
        local_identity: DeviceKeypair,
        local_challenge: [u8; 32],
        trusted_peer_keys: Vec<PublicKey>,
    ) -> Self {
        Self {
            local_identity,
            local_challenge,
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
    _connection: Box<dyn Connection>,
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
        _connection: connection,
    })
}

/// Accept inbound links for the daemon lifetime.
pub fn spawn_inbound_accept_loop(
    transport: Arc<dyn Transport>,
    handler: Option<PeerConnectionHandler>,
    session_config: Option<TrustedSessionConfig>,
) {
    tokio::spawn(async move {
        loop {
            match transport.accept().await {
                Ok(connection) => {
                    let peer = connection.peer_addr();
                    let kind = connection.kind();
                    info!(%peer, ?kind, "accepted peer connection");
                    let session_config = session_config.clone();
                    if let Some(handler) = &handler {
                        let handler = Arc::clone(handler);
                        tokio::spawn(async move {
                            match secure_connection(connection, session_config).await {
                                Ok(connection) => handler(connection),
                                Err(error) => {
                                    warn!(%error, "trusted session handshake failed");
                                }
                            }
                        });
                    } else {
                        tokio::spawn(async move {
                            match secure_connection(connection, session_config).await {
                                Ok(connection) => hold_connection_until_closed(connection).await,
                                Err(error) => {
                                    warn!(%error, "trusted session handshake failed");
                                }
                            }
                        });
                    }
                }
                Err(error) => {
                    warn!(%error, "failed to accept peer connection");
                }
            }
        }
    });
}

/// Drive trusted rediscovery targets into real transport connections.
pub fn spawn_reconnect_driver(
    service: Arc<DiscoveryService>,
    transport: Arc<dyn Transport>,
    mut targets: mpsc::Receiver<ReconnectTarget>,
    session_config: Option<TrustedSessionConfig>,
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
                    tokio::spawn(async move {
                        hold_connection_until_closed(peer._connection).await;
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

async fn secure_connection(
    connection: Box<dyn Connection>,
    session_config: Option<TrustedSessionConfig>,
) -> Result<Box<dyn Connection>, NetworkError> {
    match session_config {
        Some(config) => {
            let secure: SecureConnection = establish_trusted_session(
                connection,
                config.local_identity,
                config.local_challenge,
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
    use std::net::SocketAddr;
    use std::sync::Arc;

    use nexkvm_core::identity::{DeviceInfo, OsKind};
    use nexkvm_discovery::{DiscoveredDevice, ReconnectTarget};
    use nexkvm_network::{TcpTransport, Transport, TransportKind};

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
}
