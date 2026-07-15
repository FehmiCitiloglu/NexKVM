//! mDNS / DNS-SD discovery backend (feature `mdns`).
//!
//! An optional LAN backend that integrates with the OS service discovery stack
//! (Bonjour/Avahi) and uses link-local multicast. The current desktop release
//! path uses UDP broadcast; this backend remains available behind the `mdns`
//! feature. It registers under [`SERVICE_TYPE`], with peer metadata in TXT
//! records ([`ServiceAnnouncement::to_txt`]).
//!
//! # Platform notes
//! - macOS ships Bonjour; Linux typically needs Avahi running; Windows 10+ has a
//!   built-in mDNS responder. Where the responder is absent or 5353/udp is
//!   firewalled, fall back to [`crate::udp`].
//! - Resolution is asynchronous: peers appear a beat after they join. The
//!   shared [`DiscoveryRegistry`] absorbs that with its TTL model.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use mdns_sd::{ResolvedService, ServiceDaemon, ServiceEvent, ServiceInfo};
use nexkvm_core::identity::DeviceInfo;

use crate::announce::{SERVICE_TYPE, ServiceAnnouncement};
use crate::registry::{DEFAULT_TTL, DiscoveryRegistry};
use crate::{DiscoveredDevice, Discovery, DiscoveryError};

impl From<mdns_sd::Error> for DiscoveryError {
    fn from(e: mdns_sd::Error) -> Self {
        DiscoveryError::Backend(e.to_string())
    }
}

/// mDNS-backed discovery.
pub struct MdnsDiscovery {
    daemon: ServiceDaemon,
    registry: Arc<DiscoveryRegistry>,
    /// Fully-qualified name of our own registered service, so we can ignore our
    /// own resolution events and unregister on drop.
    registered: Mutex<Option<String>>,
}

impl std::fmt::Debug for MdnsDiscovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MdnsDiscovery")
            .field("registry_len", &self.registry.len())
            .field("registered", &self.registered)
            .finish_non_exhaustive()
    }
}

impl MdnsDiscovery {
    /// Start an mDNS daemon and begin browsing for peers.
    ///
    /// # Errors
    /// Returns [`DiscoveryError::Backend`] if the daemon cannot start or the
    /// browse cannot be initiated.
    pub fn new() -> Result<Self, DiscoveryError> {
        let daemon = ServiceDaemon::new()?;
        let registry = Arc::new(DiscoveryRegistry::new(DEFAULT_TTL));

        let receiver = daemon.browse(SERVICE_TYPE)?;
        let reg = Arc::clone(&registry);
        // Drain resolution events into the registry.
        tokio::spawn(async move {
            while let Ok(event) = receiver.recv_async().await {
                if let ServiceEvent::ServiceResolved(info) = event
                    && let Some(device) = resolved_to_device(&info)
                {
                    reg.observe(device, Instant::now());
                }
            }
        });

        Ok(Self {
            daemon,
            registry,
            registered: Mutex::new(None),
        })
    }

    /// Shared registry of live peers.
    #[must_use]
    pub fn registry(&self) -> Arc<DiscoveryRegistry> {
        Arc::clone(&self.registry)
    }
}

impl Drop for MdnsDiscovery {
    fn drop(&mut self) {
        if let Ok(name) = self.registered.lock()
            && let Some(fullname) = name.as_ref()
        {
            let _ = self.daemon.unregister(fullname);
        }
        let _ = self.daemon.shutdown();
    }
}

#[async_trait]
impl Discovery for MdnsDiscovery {
    async fn advertise(
        &self,
        info: &DeviceInfo,
        addr: SocketAddr,
        fingerprint: Option<&str>,
    ) -> Result<(), DiscoveryError> {
        let mut announcement = ServiceAnnouncement::new(info.clone(), addr.port(), 1);
        if let Some(fingerprint) = fingerprint {
            announcement = announcement.with_fingerprint(fingerprint);
        }
        let instance = info.id.to_string();
        // Hostname must end in ".local."; the daemon resolves addresses itself
        // when we pass an empty IP set and `addr.ip()`.
        let host = format!("{instance}.local.");
        let service = ServiceInfo::new(
            SERVICE_TYPE,
            &instance,
            &host,
            addr.ip(),
            addr.port(),
            announcement.to_txt(),
        )?;
        let fullname = service.get_fullname().to_string();
        self.daemon.register(service)?;
        *self
            .registered
            .lock()
            .map_err(|_| DiscoveryError::Backend("registered service lock poisoned".into()))? =
            Some(fullname);
        Ok(())
    }

    async fn discovered(&self) -> Result<Vec<DiscoveredDevice>, DiscoveryError> {
        Ok(self.registry.live(Instant::now()))
    }
}

fn resolved_to_device(info: &ResolvedService) -> Option<DiscoveredDevice> {
    // Reconstruct the TXT map the announcement codec understands.
    let mut txt = std::collections::HashMap::new();
    for prop in info.get_properties().iter() {
        txt.insert(prop.key().to_string(), prop.val_str().to_string());
    }
    let announcement = ServiceAnnouncement::from_txt(&txt).ok()?;
    let ip = info.get_addresses().iter().next()?.to_ip_addr();
    let addr = SocketAddr::new(ip, announcement.port);
    Some(DiscoveredDevice {
        info: announcement.info,
        addr,
        fingerprint: announcement.fingerprint,
    })
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use nexkvm_core::identity::OsKind;

    use super::*;

    #[test]
    fn converts_resolved_service_from_current_mdns_api() {
        let info = DeviceInfo::new("Apple Silicon Mac", OsKind::MacOs);
        let announcement =
            ServiceAnnouncement::new(info.clone(), 47_654, 1).with_fingerprint("aa:bb");
        let service = ServiceInfo::new(
            SERVICE_TYPE,
            &info.id.to_string(),
            "nexkvm-test.local.",
            IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10)),
            47_654,
            announcement.to_txt(),
        )
        .expect("valid service fixture")
        .as_resolved_service();

        let discovered = resolved_to_device(&service).expect("resolved service converts");

        assert_eq!(discovered.info, info);
        assert_eq!(
            discovered.addr,
            "192.0.2.10:47654".parse().expect("valid address")
        );
        assert_eq!(discovered.fingerprint.as_deref(), Some("aa:bb"));
    }
}
