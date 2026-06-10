//! End-to-end discovery: a trusted peer broadcasting over real UDP sockets is
//! discovered and surfaced as a reconnect target by [`DiscoveryService`].
//!
//! Uses loopback unicast to the bound discovery port (broadcast delivery to self
//! is unreliable across CI hosts) while exercising the genuine
//! announce/decode/registry/planner path.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use nexkvm_core::identity::{DeviceInfo, OsKind};
use nexkvm_discovery::{
    DiscoveryService, FingerprintAllowlist, ServiceAnnouncement, ServiceConfig, UdpConfig,
    UdpDiscovery,
};
use tokio::net::UdpSocket;

#[tokio::test]
async fn discovers_and_targets_trusted_peer_over_udp() {
    // Local device's discovery backend, bound to an ephemeral port.
    let local = DeviceInfo::new("local", OsKind::MacOs);
    let backend = UdpDiscovery::bind(
        local.id,
        UdpConfig {
            port: 0,
            interval: Duration::from_millis(50),
            ttl: Duration::from_secs(15),
        },
    )
    .unwrap();
    let recv_port = backend.local_addr().unwrap().port();

    // Only "aa:bb" is trusted.
    let trust = Arc::new(FingerprintAllowlist::new(["aa:bb".to_string()]));
    let service = DiscoveryService::new(
        Arc::new(backend),
        trust,
        ServiceConfig {
            poll_interval: Duration::from_millis(25),
            ..ServiceConfig::default()
        },
    );

    let listen: SocketAddr = "0.0.0.0:47654".parse().unwrap();
    let mut targets = service.start(&local, listen).await.unwrap();

    // A trusted peer announces itself via unicast to the local discovery port.
    let peer = DeviceInfo::new("peer", OsKind::Linux);
    let ann = ServiceAnnouncement::new(peer.clone(), 47_654, 1).with_fingerprint("aa:bb");
    let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let datagram = ann.encode().unwrap();

    // Re-send periodically until the service emits a target (or we time out).
    let recv = async {
        loop {
            let _ = sender.send_to(&datagram, ("127.0.0.1", recv_port)).await;
            if let Ok(Some(target)) =
                tokio::time::timeout(Duration::from_millis(50), targets.recv()).await
            {
                break target;
            }
        }
    };

    let target = tokio::time::timeout(Duration::from_secs(3), recv)
        .await
        .expect("trusted peer should yield a reconnect target");
    assert_eq!(target.device.info.id, peer.id);
    assert_eq!(target.device.fingerprint.as_deref(), Some("aa:bb"));
}
