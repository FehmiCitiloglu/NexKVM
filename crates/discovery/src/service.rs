//! Discovery orchestration: the glue that turns the standalone primitives
//! ([`Discovery`] backend, [`DiscoveryRegistry`], [`ReconnectPlanner`]) into a
//! running service the daemon can start with one call.
//!
//! Responsibilities:
//! - Advertise this device once on start.
//! - Periodically poll the backend's live peers.
//! - Run the [`ReconnectPlanner`] against a [`TrustOracle`] and stream out
//!   [`ReconnectTarget`]s for a driver (the network crate) to dial.
//! - Accept reconnect outcome feedback so per-device backoff is honored.
//!
//! The service is transport-agnostic: it never dials a peer itself. It emits
//! targets over a bounded channel and exposes
//! [`report_success`](DiscoveryService::report_success) /
//! [`report_failure`](DiscoveryService::report_failure) so the driver closes the
//! loop. This keeps discovery decoupled from `nexkvm-network` and unit-testable.
//!
//! Trust is injected via [`TrustOracle`] rather than depending on
//! `nexkvm-crypto`'s `TrustStore` directly, so discovery stays decoupled from the
//! pairing layer and only needs a fingerprint match. The advertised fingerprint
//! is *not* proof of identity — it only gates *which* peers we attempt to redial;
//! the cryptographic handshake still authenticates the peer after connecting.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nexkvm_core::identity::{DeviceId, DeviceInfo};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::reconnect::{ReconnectPlanner, ReconnectPolicy, ReconnectTarget};
use crate::{DiscoveredDevice, Discovery, DiscoveryError};

/// Decides whether a discovered peer is one we should auto-reconnect to.
///
/// Implementations match the peer's advertised fingerprint against the local
/// trust store. The match is advisory: it selects reconnect candidates, but the
/// handshake — not this check — authenticates the peer.
pub trait TrustOracle: Send + Sync {
    /// Whether `device` corresponds to an already-trusted peer.
    fn is_trusted(&self, device: &DiscoveredDevice) -> bool;
}

/// A [`TrustOracle`] backed by a fixed set of trusted public-key fingerprints.
///
/// Build it from the trust store's entries (each entry's
/// `public_key.fingerprint()`). A peer is trusted only if it advertises a
/// fingerprint present in the set.
#[derive(Debug, Clone, Default)]
pub struct FingerprintAllowlist {
    fingerprints: HashSet<String>,
}

impl FingerprintAllowlist {
    /// Build an allowlist from an iterator of trusted fingerprints.
    #[must_use]
    pub fn new(fingerprints: impl IntoIterator<Item = String>) -> Self {
        Self {
            fingerprints: fingerprints.into_iter().collect(),
        }
    }

    /// Whether the allowlist contains no fingerprints (nothing to reconnect to).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fingerprints.is_empty()
    }
}

impl TrustOracle for FingerprintAllowlist {
    fn is_trusted(&self, device: &DiscoveredDevice) -> bool {
        device
            .fingerprint
            .as_deref()
            .is_some_and(|fp| self.fingerprints.contains(fp))
    }
}

/// Tunables for the orchestration loop.
#[derive(Debug, Clone)]
pub struct ServiceConfig {
    /// How often to poll the backend for live peers and run the planner.
    pub poll_interval: Duration,
    /// Backoff policy for reconnection attempts.
    pub reconnect: ReconnectPolicy,
    /// Bound on the reconnect-target channel (backpressure on the driver).
    pub channel_capacity: usize,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(2),
            reconnect: ReconnectPolicy::default(),
            channel_capacity: 32,
        }
    }
}

/// Drives discovery + trust-gated auto-reconnect over any [`Discovery`] backend.
pub struct DiscoveryService {
    discovery: Arc<dyn Discovery>,
    trust: Arc<dyn TrustOracle>,
    planner: Arc<Mutex<ReconnectPlanner>>,
    config: ServiceConfig,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl DiscoveryService {
    /// Assemble a service over `discovery`, gating reconnects with `trust`.
    #[must_use]
    pub fn new(
        discovery: Arc<dyn Discovery>,
        trust: Arc<dyn TrustOracle>,
        config: ServiceConfig,
    ) -> Self {
        let planner = Arc::new(Mutex::new(ReconnectPlanner::new(config.reconnect.clone())));
        Self {
            discovery,
            trust,
            planner,
            config,
            tasks: Mutex::new(Vec::new()),
        }
    }

    /// Begin advertising and spawn the poll/reconnect loop.
    ///
    /// Returns a receiver of [`ReconnectTarget`]s for the driver to dial. When
    /// the receiver is dropped the loop stops on its next emit.
    ///
    /// # Errors
    /// Returns [`DiscoveryError`] if advertising cannot start.
    pub async fn start(
        &self,
        info: &DeviceInfo,
        listen_addr: SocketAddr,
        fingerprint: Option<&str>,
    ) -> Result<mpsc::Receiver<ReconnectTarget>, DiscoveryError> {
        self.discovery
            .advertise(info, listen_addr, fingerprint)
            .await?;

        let (tx, rx) = mpsc::channel(self.config.channel_capacity);
        let discovery = Arc::clone(&self.discovery);
        let trust = Arc::clone(&self.trust);
        let planner = Arc::clone(&self.planner);
        let interval = self.config.poll_interval;

        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            loop {
                ticker.tick().await;
                let visible = discovery.discovered().await.unwrap_or_default();
                let now = Instant::now();
                // Compute targets under the lock, then release it before await.
                let targets = {
                    let mut planner = planner.lock().expect("planner mutex poisoned");
                    planner.due(&visible, |d| trust.is_trusted(d), now)
                };
                for target in targets {
                    if tx.send(target).await.is_err() {
                        return; // driver gone; stop the loop
                    }
                }
            }
        });

        self.tasks
            .lock()
            .expect("tasks mutex poisoned")
            .push(handle);
        Ok(rx)
    }

    /// Live peers currently visible to the backend.
    ///
    /// # Errors
    /// Returns [`DiscoveryError`] if the backend cannot report peers.
    pub async fn discovered(&self) -> Result<Vec<DiscoveredDevice>, DiscoveryError> {
        self.discovery.discovered().await
    }

    /// Report that a reconnect attempt to `id` succeeded, clearing its backoff.
    pub fn report_success(&self, id: DeviceId) {
        self.planner
            .lock()
            .expect("planner mutex poisoned")
            .record_success(id);
    }

    /// Report that a reconnect attempt to `id` failed, growing its backoff.
    pub fn report_failure(&self, id: DeviceId) {
        self.planner
            .lock()
            .expect("planner mutex poisoned")
            .record_failure(id, Instant::now());
    }
}

impl std::fmt::Debug for DiscoveryService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscoveryService")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl Drop for DiscoveryService {
    fn drop(&mut self) {
        if let Ok(tasks) = self.tasks.lock() {
            for handle in tasks.iter() {
                handle.abort();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use nexkvm_core::identity::{DeviceInfo, OsKind};

    /// A backend returning a fixed set of peers, recording the advertised addr.
    struct StubDiscovery {
        peers: Vec<DiscoveredDevice>,
        advertised: Mutex<Option<SocketAddr>>,
    }

    #[async_trait]
    impl Discovery for StubDiscovery {
        async fn advertise(
            &self,
            _info: &DeviceInfo,
            addr: SocketAddr,
            _fingerprint: Option<&str>,
        ) -> Result<(), DiscoveryError> {
            *self.advertised.lock().unwrap() = Some(addr);
            Ok(())
        }

        async fn discovered(&self) -> Result<Vec<DiscoveredDevice>, DiscoveryError> {
            Ok(self.peers.clone())
        }
    }

    fn device(name: &str, fp: Option<&str>) -> DiscoveredDevice {
        DiscoveredDevice {
            info: DeviceInfo::new(name, OsKind::Linux),
            addr: "127.0.0.1:47654".parse().unwrap(),
            fingerprint: fp.map(str::to_string),
        }
    }

    #[test]
    fn allowlist_only_trusts_known_fingerprints() {
        let allow = FingerprintAllowlist::new(["aa:bb".to_string()]);
        assert!(allow.is_trusted(&device("trusted", Some("aa:bb"))));
        assert!(!allow.is_trusted(&device("stranger", Some("zz:zz"))));
        assert!(!allow.is_trusted(&device("anon", None)));
    }

    #[tokio::test]
    async fn emits_reconnect_target_for_trusted_peer() {
        let trusted = device("laptop", Some("aa:bb"));
        let stranger = device("guest", Some("zz:zz"));
        let backend = Arc::new(StubDiscovery {
            peers: vec![trusted.clone(), stranger],
            advertised: Mutex::new(None),
        });
        let trust = Arc::new(FingerprintAllowlist::new(["aa:bb".to_string()]));
        let config = ServiceConfig {
            poll_interval: Duration::from_millis(10),
            ..ServiceConfig::default()
        };
        let service = DiscoveryService::new(backend.clone(), trust, config);

        let info = DeviceInfo::new("self", OsKind::MacOs);
        let addr: SocketAddr = "0.0.0.0:47654".parse().unwrap();
        let mut rx = service.start(&info, addr, Some("self-fp")).await.unwrap();

        let target = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("a target should be emitted")
            .expect("channel open");
        assert_eq!(target.device.info.id, trusted.info.id);
        assert_eq!(target.attempt, 0);
        assert_eq!(*backend.advertised.lock().unwrap(), Some(addr));
    }

    #[tokio::test]
    async fn untrusted_only_network_emits_nothing() {
        let backend = Arc::new(StubDiscovery {
            peers: vec![device("guest", Some("zz:zz"))],
            advertised: Mutex::new(None),
        });
        let trust = Arc::new(FingerprintAllowlist::new(["aa:bb".to_string()]));
        let config = ServiceConfig {
            poll_interval: Duration::from_millis(10),
            ..ServiceConfig::default()
        };
        let service = DiscoveryService::new(backend, trust, config);

        let info = DeviceInfo::new("self", OsKind::MacOs);
        let mut rx = service
            .start(&info, "0.0.0.0:47654".parse().unwrap(), Some("self-fp"))
            .await
            .unwrap();

        let got = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await;
        assert!(got.is_err(), "no trusted peers => no targets");
    }
}
