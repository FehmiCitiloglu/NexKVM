use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use nexkvm_crypto::{
    DEFAULT_PAIRING_TTL, DeviceIdentity, DeviceKeypair, PairingMethod, PairingSession, PublicKey,
    TrustEntry, TrustStore,
};
use nexkvm_network::{
    Connection, PairingConfirmationPrompt, TcpTransport, Transport, exchange_pairing_approval,
    exchange_pairing_persistence, initiate_pairing_handshake, respond_pairing_handshake,
};
use nexkvm_storage::{Config, FileDeviceIdentityStore, FileTrustStore};
use tokio::sync::mpsc;

use crate::connection;

struct PairingSettingsRequest {
    config_path: PathBuf,
    trust_path: PathBuf,
    session: PairingSession,
    prompt: PairingConfirmationPrompt,
    peer_endpoint: SocketAddr,
    now: Instant,
    paired_at: u64,
}

struct AppliedPairingSettings {
    config_path: PathBuf,
    trust_path: PathBuf,
    previous_config: Config,
    previous_trust: Option<TrustEntry>,
    entry: TrustEntry,
}

impl AppliedPairingSettings {
    fn entry(&self) -> &TrustEntry {
        &self.entry
    }

    fn rollback(self) -> anyhow::Result<()> {
        let mut failures = Vec::new();
        if let Err(error) = self.previous_config.save(&self.config_path) {
            failures.push(format!(
                "restoring config {}: {error}",
                self.config_path.display()
            ));
        }

        match FileTrustStore::load(&self.trust_path) {
            Ok(store) => {
                restore_trust_entry(&store, &self.entry.public_key, self.previous_trust);
                if let Err(error) = store.flush() {
                    failures.push(format!(
                        "restoring trust store {}: {error}",
                        self.trust_path.display()
                    ));
                }
            }
            Err(error) => failures.push(format!(
                "loading trust store {} for rollback: {error}",
                self.trust_path.display()
            )),
        }

        if failures.is_empty() {
            Ok(())
        } else {
            anyhow::bail!(failures.join("; "))
        }
    }
}

fn apply_pairing_settings(
    mut request: PairingSettingsRequest,
) -> anyhow::Result<AppliedPairingSettings> {
    let previous_config = Config::load(&request.config_path)
        .with_context(|| format!("loading config from {}", request.config_path.display()))?;
    let store = FileTrustStore::load(&request.trust_path)
        .with_context(|| format!("loading trust store from {}", request.trust_path.display()))?;
    let previous_trust = store.get(&request.prompt.peer.public_key);
    let entry = match store.confirm_pairing(
        &mut request.session,
        request.prompt.code.as_str(),
        &request.prompt.peer,
        request.paired_at,
        request.now,
    ) {
        Ok(entry) => entry,
        Err(error) => {
            restore_trust_entry(
                &store,
                &request.prompt.peer.public_key,
                previous_trust.clone(),
            );
            return match store.flush() {
                Ok(()) => Err(error).with_context(|| {
                    format!(
                        "persisting trusted peer to {}",
                        request.trust_path.display()
                    )
                }),
                Err(rollback_error) => Err(anyhow::anyhow!(
                    "persisting trusted peer to {} failed: {error}; trust rollback failed: {rollback_error}",
                    request.trust_path.display()
                )),
            };
        }
    };

    let mut configured = previous_config.clone();
    configured.network.connect_addr = Some(request.peer_endpoint.to_string());
    configured.input.active_peer = Some(entry.public_key.fingerprint());
    if let Err(error) = configured.save(&request.config_path) {
        restore_trust_entry(&store, &entry.public_key, previous_trust.clone());
        let rollback = store.flush();
        return match rollback {
            Ok(()) => Err(error).with_context(|| {
                format!(
                    "saving automatic pairing settings to {}",
                    request.config_path.display()
                )
            }),
            Err(rollback_error) => Err(anyhow::anyhow!(
                "saving automatic pairing settings to {} failed: {error}; trust rollback failed: {rollback_error}",
                request.config_path.display()
            )),
        };
    }

    Ok(AppliedPairingSettings {
        config_path: request.config_path,
        trust_path: request.trust_path,
        previous_config,
        previous_trust,
        entry,
    })
}

fn restore_trust_entry(store: &FileTrustStore, peer_key: &PublicKey, previous: Option<TrustEntry>) {
    match previous {
        Some(entry) => store.insert(entry),
        None => store.remove(peer_key),
    }
}

pub(crate) async fn initiate(
    peer: &str,
    config_path: PathBuf,
    trust_path: PathBuf,
) -> anyhow::Result<()> {
    let (config, local_keypair) = load_pairing_identity(config_path.clone()).await?;
    let local_identity = DeviceIdentity {
        display_name: config.device.name.clone(),
        public_key: local_keypair.public_key(),
    };
    let now = Instant::now();
    let session = PairingSession::initiate(
        local_identity,
        format!("listener:{}", config.network.listen_port),
        fresh_nonce()?,
        now,
        DEFAULT_PAIRING_TTL,
    );

    let transport: Arc<dyn Transport> = Arc::new(TcpTransport::bind("0.0.0.0:0".parse()?).await?);
    let (resolved_peer, connection) = connection::connect_explicit_endpoint(transport, peer, None)
        .await
        .with_context(|| format!("connecting to automatic pairing peer {peer}"))?;
    let prompt = initiate_pairing_handshake(
        &*connection,
        &session,
        PairingMethod::NumericCode,
        config.network.listen_port,
        now,
    )
    .await?;
    let peer_endpoint = peer_listen_endpoint(resolved_peer, prompt.peer_listen_port);

    let result = complete_pairing(
        &*connection,
        session,
        prompt,
        peer_endpoint,
        config_path,
        trust_path,
    )
    .await;
    let _ = connection.close().await;
    result.map(|_| ())
}

pub(crate) fn spawn_responder(
    config_path: PathBuf,
    trust_path: PathBuf,
    local_public_key: PublicKey,
) -> connection::PairingConnectionHandler {
    let (sender, mut receiver) = mpsc::channel::<Box<dyn Connection>>(1);
    tokio::spawn(async move {
        while let Some(connection) = receiver.recv().await {
            let peer = connection.peer_addr();
            if let Err(error) = respond(
                connection,
                config_path.clone(),
                trust_path.clone(),
                local_public_key.clone(),
            )
            .await
            {
                tracing::warn!(%peer, %error, "automatic pairing failed");
            }
        }
    });

    Arc::new(move |connection| {
        let peer = connection.peer_addr();
        match sender.try_send(connection) {
            Ok(()) => tracing::info!(%peer, "automatic pairing request queued"),
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!(%peer, "automatic pairing request rejected; another request is active");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                tracing::warn!(%peer, "automatic pairing request rejected; responder stopped");
            }
        }
    })
}

async fn respond(
    connection: Box<dyn Connection>,
    config_path: PathBuf,
    trust_path: PathBuf,
    local_public_key: PublicKey,
) -> anyhow::Result<()> {
    let config = load_config(config_path.clone()).await?;
    let local = DeviceIdentity {
        display_name: config.device.name,
        public_key: local_public_key,
    };
    let now = Instant::now();
    let (session, prompt) = respond_pairing_handshake(
        &*connection,
        local,
        config.network.listen_port,
        now,
        DEFAULT_PAIRING_TTL,
    )
    .await?;
    let peer_endpoint = peer_listen_endpoint(connection.peer_addr(), prompt.peer_listen_port);

    let result = complete_pairing(
        &*connection,
        session,
        prompt,
        peer_endpoint,
        config_path,
        trust_path,
    )
    .await;
    let _ = connection.close().await;
    result.map(|_| ())
}

async fn complete_pairing(
    connection: &dyn Connection,
    session: PairingSession,
    prompt: PairingConfirmationPrompt,
    peer_endpoint: SocketAddr,
    config_path: PathBuf,
    trust_path: PathBuf,
) -> anyhow::Result<Option<TrustEntry>> {
    let locally_accepted = prompt_for_approval(prompt.clone(), peer_endpoint).await?;
    complete_pairing_with_approval(
        connection,
        session,
        prompt,
        peer_endpoint,
        config_path,
        trust_path,
        locally_accepted,
    )
    .await
}

async fn complete_pairing_with_approval(
    connection: &dyn Connection,
    mut session: PairingSession,
    prompt: PairingConfirmationPrompt,
    peer_endpoint: SocketAddr,
    config_path: PathBuf,
    trust_path: PathBuf,
    locally_accepted: bool,
) -> anyhow::Result<Option<TrustEntry>> {
    let peer_accepted = exchange_pairing_approval(connection, locally_accepted).await?;
    if !locally_accepted || !peer_accepted {
        session.reject();
        if locally_accepted {
            println!("automatic pairing cancelled: the peer rejected the confirmation code");
        } else {
            println!("automatic pairing cancelled locally");
        }
        return Ok(None);
    }

    let request = PairingSettingsRequest {
        config_path,
        trust_path,
        session,
        prompt,
        peer_endpoint,
        now: Instant::now(),
        paired_at: unix_timestamp()?,
    };
    let applied = tokio::task::spawn_blocking(move || apply_pairing_settings(request))
        .await
        .context("automatic pairing persistence task failed")?;
    let local_persisted = applied.is_ok();
    let peer_persisted = exchange_pairing_persistence(connection, local_persisted).await;

    match applied {
        Err(error) => {
            let _ = peer_persisted;
            Err(error)
        }
        Ok(applied) => match peer_persisted {
            Ok(true) => {
                let entry = applied.entry().clone();
                println!(
                    "automatic pairing complete\n  peer: {}\n  fingerprint: {}\n  connect: {}\n  active peer: {}\nRestart any already-running local daemon to load the new connection settings.",
                    entry.display_name,
                    entry.public_key.fingerprint(),
                    peer_endpoint,
                    entry.public_key.fingerprint(),
                );
                Ok(Some(entry))
            }
            Ok(false) => {
                rollback_applied(applied).await?;
                anyhow::bail!("peer could not persist pairing; local changes were rolled back")
            }
            Err(error) => {
                rollback_applied(applied).await?;
                Err(error).context(
                    "pairing persistence status was not confirmed; local changes were rolled back",
                )
            }
        },
    }
}

async fn rollback_applied(applied: AppliedPairingSettings) -> anyhow::Result<()> {
    tokio::task::spawn_blocking(move || applied.rollback())
        .await
        .context("automatic pairing rollback task failed")?
}

async fn prompt_for_approval(
    prompt: PairingConfirmationPrompt,
    peer_endpoint: SocketAddr,
) -> anyhow::Result<bool> {
    tokio::task::spawn_blocking(move || {
        eprintln!();
        eprintln!("Automatic pairing request");
        eprintln!("  peer: {}", prompt.peer.display_name);
        eprintln!("  fingerprint: {}", prompt.peer.public_key.fingerprint());
        eprintln!("  endpoint: {peer_endpoint}");
        eprintln!("  confirmation code: {}", prompt.code);
        eprint!("Type `yes` only if the same code is visible on the other device: ");
        io::stderr().flush().context("flushing pairing prompt")?;

        let mut answer = String::new();
        let read = io::stdin()
            .read_line(&mut answer)
            .context("reading pairing confirmation")?;
        Ok(read != 0 && answer_is_yes(&answer))
    })
    .await
    .context("pairing confirmation task failed")?
}

fn answer_is_yes(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn peer_listen_endpoint(mut observed: SocketAddr, listen_port: u16) -> SocketAddr {
    observed.set_port(listen_port);
    observed
}

async fn load_pairing_identity(config_path: PathBuf) -> anyhow::Result<(Config, DeviceKeypair)> {
    tokio::task::spawn_blocking(move || {
        let config = Config::load(&config_path)
            .with_context(|| format!("loading config from {}", config_path.display()))?;
        let identity_path = identity_path_for(&config_path);
        let identity = FileDeviceIdentityStore::new(&identity_path)
            .load_or_create(&config.device.name)
            .with_context(|| format!("loading local identity from {}", identity_path.display()))?;
        Ok((config, identity))
    })
    .await
    .context("pairing identity loader task failed")?
}

async fn load_config(config_path: PathBuf) -> anyhow::Result<Config> {
    tokio::task::spawn_blocking(move || {
        Config::load(&config_path)
            .with_context(|| format!("loading config from {}", config_path.display()))
    })
    .await
    .context("pairing config loader task failed")?
}

fn identity_path_for(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("identity.json")
}

fn fresh_nonce() -> anyhow::Result<[u8; nexkvm_crypto::NONCE_LEN]> {
    let mut nonce = [0u8; nexkvm_crypto::NONCE_LEN];
    getrandom::fill(&mut nonce)
        .map_err(|error| anyhow::anyhow!("generating automatic pairing nonce: {error}"))?;
    Ok(nonce)
}

fn unix_timestamp() -> anyhow::Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_secs())
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    use async_trait::async_trait;
    use nexkvm_crypto::{
        DEFAULT_PAIRING_TTL, DeviceIdentity, PairingSession, PublicKey, TrustStore,
    };
    use nexkvm_network::PairingConfirmationPrompt;
    use nexkvm_network::{NetworkError, TransportKind};
    use nexkvm_protocol::Envelope;
    use nexkvm_storage::{Config, FileTrustStore};
    use tokio::sync::{Mutex, mpsc};

    use super::*;

    #[derive(Debug)]
    struct MemoryConnection {
        peer: SocketAddr,
        tx: mpsc::Sender<Envelope>,
        rx: Mutex<mpsc::Receiver<Envelope>>,
    }

    impl MemoryConnection {
        fn pair() -> (Arc<Self>, Arc<Self>) {
            let (left_tx, left_rx) = mpsc::channel(8);
            let (right_tx, right_rx) = mpsc::channel(8);
            (
                Arc::new(Self {
                    peer: "127.0.0.1:4102".parse().unwrap(),
                    tx: left_tx,
                    rx: Mutex::new(right_rx),
                }),
                Arc::new(Self {
                    peer: "127.0.0.1:4101".parse().unwrap(),
                    tx: right_tx,
                    rx: Mutex::new(left_rx),
                }),
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

    #[test]
    fn approved_pairing_persists_settings_and_can_roll_back() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        let trust_path = directory.path().join("trust.json");
        let mut original = Config::default();
        original.network.connect_addr = Some("192.168.1.10:47654".into());
        original.input.active_peer = Some("old-peer".into());
        original.save(&config_path).unwrap();

        let now = Instant::now();
        let local = DeviceIdentity {
            display_name: "local".into(),
            public_key: PublicKey(vec![1; 32]),
        };
        let peer = DeviceIdentity {
            display_name: "peer".into(),
            public_key: PublicKey(vec![2; 32]),
        };
        let session = PairingSession::initiate(
            local,
            "192.168.1.20:47654",
            [7; 32],
            now,
            DEFAULT_PAIRING_TTL,
        );
        let code = session.confirmation_code(&peer.public_key, now).unwrap();
        let prompt = PairingConfirmationPrompt {
            peer: peer.clone(),
            code,
            peer_listen_port: 47_654,
        };
        let endpoint: SocketAddr = "192.168.1.20:47654".parse().unwrap();
        let paired_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let applied = apply_pairing_settings(PairingSettingsRequest {
            config_path: config_path.clone(),
            trust_path: trust_path.clone(),
            session,
            prompt,
            peer_endpoint: endpoint,
            now,
            paired_at,
        })
        .unwrap();

        let configured = Config::load(&config_path).unwrap();
        assert_eq!(
            configured.network.connect_addr.as_deref(),
            Some("192.168.1.20:47654")
        );
        assert_eq!(
            configured.input.active_peer.as_deref(),
            Some(peer.public_key.fingerprint().as_str())
        );
        assert!(
            FileTrustStore::load(&trust_path)
                .unwrap()
                .is_trusted(&peer.public_key)
        );

        applied.rollback().unwrap();

        let restored = Config::load(&config_path).unwrap();
        assert_eq!(restored.network.connect_addr, original.network.connect_addr);
        assert_eq!(restored.input.active_peer, original.input.active_peer);
        assert!(
            !FileTrustStore::load(&trust_path)
                .unwrap()
                .is_trusted(&peer.public_key)
        );
    }

    #[test]
    fn confirmation_accepts_only_an_explicit_yes() {
        assert!(answer_is_yes("yes"));
        assert!(answer_is_yes(" Y "));
        assert!(!answer_is_yes(""));
        assert!(!answer_is_yes("no"));
        assert!(!answer_is_yes("sure"));
    }

    #[test]
    fn peer_endpoint_preserves_an_ipv6_scope_id() {
        let observed = SocketAddr::V6(std::net::SocketAddrV6::new(
            "fe80::1".parse().unwrap(),
            55_000,
            0,
            7,
        ));

        let endpoint = peer_listen_endpoint(observed, 47_654);

        assert_eq!(endpoint.port(), 47_654);
        let SocketAddr::V6(endpoint) = endpoint else {
            panic!("expected IPv6 endpoint");
        };
        assert_eq!(endpoint.scope_id(), 7);
    }

    #[tokio::test]
    async fn mutual_approval_persists_trust_and_settings_on_both_devices() {
        let directory = tempfile::tempdir().unwrap();
        let left_config_path = directory.path().join("left/config.toml");
        let left_trust_path = directory.path().join("left/trust.json");
        let right_config_path = directory.path().join("right/config.toml");
        let right_trust_path = directory.path().join("right/trust.json");
        let mut left_config = Config::default();
        left_config.device.name = "left".into();
        left_config.network.listen_port = 41_001;
        left_config.save(&left_config_path).unwrap();
        let mut right_config = Config::default();
        right_config.device.name = "right".into();
        right_config.network.listen_port = 41_002;
        right_config.save(&right_config_path).unwrap();

        let left_identity = DeviceIdentity {
            display_name: "left".into(),
            public_key: PublicKey(vec![1; 32]),
        };
        let right_identity = DeviceIdentity {
            display_name: "right".into(),
            public_key: PublicKey(vec![2; 32]),
        };
        let now = Instant::now();
        let left_session = PairingSession::initiate(
            left_identity.clone(),
            "127.0.0.1:41001",
            [9; 32],
            now,
            DEFAULT_PAIRING_TTL,
        );
        let (left_connection, right_connection) = MemoryConnection::pair();

        let left_flow = async {
            let prompt = initiate_pairing_handshake(
                &*left_connection,
                &left_session,
                PairingMethod::NumericCode,
                41_001,
                now,
            )
            .await
            .unwrap();
            complete_pairing_with_approval(
                &*left_connection,
                left_session,
                prompt,
                "127.0.0.1:41002".parse().unwrap(),
                left_config_path.clone(),
                left_trust_path.clone(),
                true,
            )
            .await
            .unwrap();
        };
        let right_flow = async {
            let (session, prompt) = respond_pairing_handshake(
                &*right_connection,
                right_identity.clone(),
                41_002,
                now,
                DEFAULT_PAIRING_TTL,
            )
            .await
            .unwrap();
            complete_pairing_with_approval(
                &*right_connection,
                session,
                prompt,
                "127.0.0.1:41001".parse().unwrap(),
                right_config_path.clone(),
                right_trust_path.clone(),
                true,
            )
            .await
            .unwrap();
        };

        tokio::join!(left_flow, right_flow);

        let left_trust = FileTrustStore::load(&left_trust_path).unwrap();
        let right_trust = FileTrustStore::load(&right_trust_path).unwrap();
        assert!(left_trust.is_trusted(&right_identity.public_key));
        assert!(right_trust.is_trusted(&left_identity.public_key));
        let left_config = Config::load(&left_config_path).unwrap();
        let right_config = Config::load(&right_config_path).unwrap();
        assert_eq!(
            left_config.network.connect_addr.as_deref(),
            Some("127.0.0.1:41002")
        );
        assert_eq!(
            right_config.network.connect_addr.as_deref(),
            Some("127.0.0.1:41001")
        );
        assert_eq!(
            left_config.input.active_peer,
            Some(right_identity.public_key.fingerprint())
        );
        assert_eq!(
            right_config.input.active_peer,
            Some(left_identity.public_key.fingerprint())
        );
    }
}
