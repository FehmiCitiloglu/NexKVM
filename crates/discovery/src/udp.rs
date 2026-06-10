//! UDP-broadcast discovery backend.
//!
//! The most portable backend: it needs no external service daemon, only a UDP
//! socket. The device periodically broadcasts its [`ServiceAnnouncement`] to the
//! limited broadcast address and listens for peers doing the same, feeding a
//! shared [`DiscoveryRegistry`].
//!
//! # Tradeoffs
//! - Works on every desktop OS and most home networks; great as a fallback.
//! - Limited broadcast (`255.255.255.255`) is **not routed** across subnets and
//!   may be dropped by some enterprise/guest Wi-Fi. mDNS ([`crate::mdns`]) is the
//!   preferred path where available; both can run together.
//! - IPv4 only. IPv6 link-local multicast discovery would be a follow-up.
//!
//! Self-announcements (matching our own [`DeviceId`]) are ignored so a device
//! never discovers itself.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use nexkvm_core::identity::{DeviceId, DeviceInfo};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;

use crate::announce::{DEFAULT_DISCOVERY_PORT, ServiceAnnouncement};
use crate::registry::{DEFAULT_TTL, DiscoveryRegistry};
use crate::{DiscoveredDevice, Discovery, DiscoveryError};

/// Max UDP announcement datagram we will read.
const RECV_BUF: usize = 2 * 1024;

/// Tunables for the UDP backend.
#[derive(Debug, Clone)]
pub struct UdpConfig {
    /// Port used for both broadcasting and listening.
    pub port: u16,
    /// How often to re-broadcast our announcement.
    pub interval: Duration,
    /// How long a peer stays live after its last announcement.
    pub ttl: Duration,
}

impl Default for UdpConfig {
    fn default() -> Self {
        Self {
            port: DEFAULT_DISCOVERY_PORT,
            interval: Duration::from_secs(3),
            ttl: DEFAULT_TTL,
        }
    }
}

/// UDP-broadcast discovery backend.
#[derive(Debug)]
pub struct UdpDiscovery {
    own_id: DeviceId,
    socket: Arc<UdpSocket>,
    registry: Arc<DiscoveryRegistry>,
    announcement: Arc<Mutex<Option<ServiceAnnouncement>>>,
    broadcast_addr: SocketAddr,
    interval: Duration,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl UdpDiscovery {
    /// Bind a UDP discovery backend for the device identified by `own_id`.
    ///
    /// Binds to `0.0.0.0:config.port` with address/port reuse so multiple peers
    /// (and our own send path) can share the broadcast port. A background
    /// receive loop starts immediately and feeds the registry.
    ///
    /// # Errors
    /// Returns [`DiscoveryError::Io`] if the socket cannot be created or bound.
    pub fn bind(own_id: DeviceId, config: UdpConfig) -> Result<Self, DiscoveryError> {
        let bind_addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, config.port));
        let socket = Arc::new(make_socket(bind_addr)?);
        let registry = Arc::new(DiscoveryRegistry::new(config.ttl));
        let broadcast_addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::BROADCAST, config.port));

        let backend = Self {
            own_id,
            socket: Arc::clone(&socket),
            registry: Arc::clone(&registry),
            announcement: Arc::new(Mutex::new(None)),
            broadcast_addr,
            interval: config.interval,
            tasks: Mutex::new(Vec::new()),
        };
        backend.spawn_recv_loop();
        Ok(backend)
    }

    /// The registry of live peers, for consumers that want direct access.
    #[must_use]
    pub fn registry(&self) -> Arc<DiscoveryRegistry> {
        Arc::clone(&self.registry)
    }

    /// The local address the discovery socket is bound to (useful in tests).
    ///
    /// # Errors
    /// Returns [`DiscoveryError::Io`] if the address cannot be read.
    pub fn local_addr(&self) -> Result<SocketAddr, DiscoveryError> {
        Ok(self.socket.local_addr()?)
    }

    fn spawn_recv_loop(&self) {
        let socket = Arc::clone(&self.socket);
        let registry = Arc::clone(&self.registry);
        let own_id = self.own_id;
        let handle = tokio::spawn(async move {
            let mut buf = vec![0u8; RECV_BUF];
            loop {
                let (n, src) = match socket.recv_from(&mut buf).await {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let Ok(ann) = ServiceAnnouncement::decode(&buf[..n]) else {
                    continue;
                };
                if ann.device_id() == own_id {
                    continue; // never discover ourselves
                }
                let device = announcement_to_device(&ann, src);
                registry.observe(device, Instant::now());
            }
        });
        self.tasks
            .lock()
            .expect("tasks mutex poisoned")
            .push(handle);
    }

    fn spawn_broadcast_loop(&self) {
        let socket = Arc::clone(&self.socket);
        let announcement = Arc::clone(&self.announcement);
        let broadcast_addr = self.broadcast_addr;
        let interval = self.interval;
        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            loop {
                ticker.tick().await;
                // Clone the current announcement out of the lock before any await.
                let current = announcement.lock().expect("ann mutex poisoned").clone();
                if let Some(ann) = current
                    && let Ok(bytes) = ann.encode()
                {
                    let _ = socket.send_to(&bytes, broadcast_addr).await;
                }
            }
        });
        self.tasks
            .lock()
            .expect("tasks mutex poisoned")
            .push(handle);
    }
}

impl Drop for UdpDiscovery {
    fn drop(&mut self) {
        if let Ok(tasks) = self.tasks.lock() {
            for handle in tasks.iter() {
                handle.abort();
            }
        }
    }
}

#[async_trait]
impl Discovery for UdpDiscovery {
    async fn advertise(&self, info: &DeviceInfo, addr: SocketAddr) -> Result<(), DiscoveryError> {
        let ann = ServiceAnnouncement::new(info.clone(), addr.port(), 1);
        let start_loop = {
            let mut guard = self.announcement.lock().expect("ann mutex poisoned");
            let first = guard.is_none();
            *guard = Some(ann);
            first
        };
        // Start the periodic broadcaster only on the first advertise call.
        if start_loop {
            self.spawn_broadcast_loop();
        }
        Ok(())
    }

    async fn discovered(&self) -> Result<Vec<DiscoveredDevice>, DiscoveryError> {
        Ok(self.registry.live(Instant::now()))
    }
}

/// Build a reuse-enabled, broadcast-capable, non-blocking UDP socket.
fn make_socket(bind_addr: SocketAddr) -> Result<UdpSocket, DiscoveryError> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    #[cfg(unix)]
    socket.set_reuse_port(true)?;
    socket.set_broadcast(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&bind_addr.into())?;
    let std_socket: std::net::UdpSocket = socket.into();
    Ok(UdpSocket::from_std(std_socket)?)
}

fn announcement_to_device(ann: &ServiceAnnouncement, src: SocketAddr) -> DiscoveredDevice {
    // The advertised port is the data-plane port; the IP comes from the sender.
    let addr = SocketAddr::new(src.ip(), ann.port);
    DiscoveredDevice {
        info: ann.info.clone(),
        addr,
        fingerprint: ann.fingerprint.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexkvm_core::identity::OsKind;

    #[tokio::test]
    async fn receives_unicast_announcement_into_registry() {
        // Receiver bound on an ephemeral port.
        let own = DeviceId::generate();
        let cfg = UdpConfig {
            port: 0,
            ..UdpConfig::default()
        };
        let backend = UdpDiscovery::bind(own, cfg).unwrap();
        let recv_addr = backend.local_addr().unwrap();

        // A peer sends a unicast announcement to the receiver's port.
        let peer_info = DeviceInfo::new("peer", OsKind::Windows);
        let ann = ServiceAnnouncement::new(peer_info.clone(), 47_654, 1);
        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sender
            .send_to(&ann.encode().unwrap(), ("127.0.0.1", recv_addr.port()))
            .await
            .unwrap();

        // Poll the registry briefly for the observation.
        let found = wait_for_peer(&backend, peer_info.id).await;
        assert!(found, "peer announcement should be registered");
    }

    #[tokio::test]
    async fn ignores_self_announcements() {
        let own = DeviceInfo::new("self", OsKind::Linux);
        let cfg = UdpConfig {
            port: 0,
            ..UdpConfig::default()
        };
        let backend = UdpDiscovery::bind(own.id, cfg).unwrap();
        let recv_addr = backend.local_addr().unwrap();

        let ann = ServiceAnnouncement::new(own.clone(), 47_654, 1);
        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sender
            .send_to(&ann.encode().unwrap(), ("127.0.0.1", recv_addr.port()))
            .await
            .unwrap();

        let found = wait_for_peer(&backend, own.id).await;
        assert!(!found, "device must not discover itself");
    }

    async fn wait_for_peer(backend: &UdpDiscovery, id: DeviceId) -> bool {
        for _ in 0..20 {
            if backend
                .discovered()
                .await
                .unwrap()
                .iter()
                .any(|d| d.info.id == id)
            {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }
}
