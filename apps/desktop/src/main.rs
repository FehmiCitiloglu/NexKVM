//! nexkvm desktop daemon entry point.
//!
//! Foundation-phase wiring: initialize telemetry, load config, construct the
//! event bus and platform backend, report negotiated protocol version and
//! resolved capabilities, then run until interrupted. Networking, discovery,
//! input pipelines, and the plugin host are attached in subsequent phases.

use anyhow::Context;
use nexkvm_core::platform::PlatformBackend;
use nexkvm_core::{DeviceInfo, EventBus, NativeIntegrationReport};
use nexkvm_network::Transport;
use nexkvm_protocol::{MessageId, PROTOCOL_VERSION, VersionRange};
use nexkvm_storage::{Config, current_os};
use nexkvm_telemetry::LogLevel;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use tracing::info;

mod cli;
mod connection;
mod input_session;

use cli::Command;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let invocation = match cli::parse(std::env::args().skip(1)) {
        Ok(invocation) => invocation,
        Err(message) => anyhow::bail!(message),
    };

    match invocation.command {
        Command::Run => return run_daemon(invocation.debug).await,
        Command::Doctor => return doctor(),
        Command::Permissions => return permissions().await,
        Command::Protocol => return protocol_info(),
        Command::ConfigPath => {
            println!("{}", config_path().display());
            return Ok(());
        }
        Command::Devices => return list_devices(),
        Command::Pair { uri, accept } => return pair(&uri, accept),
        Command::PairingUri { addr } => return pairing_uri(&addr),
        Command::Simulate { path } => return simulate(path),
        Command::Help => {
            print!("{}", cli::help_text());
            return Ok(());
        }
    }
}

/// Run the desktop daemon: foundation-phase wiring of telemetry, config, the
/// event bus, the platform backend, and LAN discovery, until interrupted.
async fn run_daemon(debug: bool) -> anyhow::Result<()> {
    // 1. Config: load from the platform config dir (falls back to defaults).
    let config_path = config_path();
    let mut config = Config::load(&config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;

    // `--debug` raises log verbosity for this run without editing config.
    if debug {
        config.telemetry.level = LogLevel::Debug;
    }

    // 2. Telemetry: install the tracing subscriber before anything else logs.
    nexkvm_telemetry::init(&config.telemetry).context("initializing telemetry")?;

    info!(
        version = %PROTOCOL_VERSION,
        supported = %VersionRange::current(),
        "starting nexkvm daemon"
    );

    // 3. Identity for this device.
    let device = DeviceInfo::new(config.device.name.clone(), current_os());
    info!(id = %device.id, name = %device.name, os = ?device.os, "device identity");

    // 4. Event bus — the in-process backbone subsystems attach to.
    let bus = EventBus::new();

    // 5. Platform backend + capability resolution.
    let backend = platform_backend();
    match backend.as_ref() {
        Some(b) => {
            let caps = b.capabilities();
            info!(os = ?b.os(), ?caps, "platform backend ready");
        }
        None => info!("no platform backend for this OS; running in headless mode"),
    }

    info!(
        listen_port = config.network.listen_port,
        discovery = config.network.enable_discovery,
        require_pairing = config.security.require_pairing,
        "configuration loaded"
    );

    let input_role = input_runtime_role(config.input.control_role);
    #[cfg(target_os = "macos")]
    let input_permissions_ready = {
        let macos = nexkvm_platform_macos::MacosBackend::new();
        let report = macos.input_permission_report();
        report.can_capture_input && report.can_inject_input
    };
    #[cfg(not(target_os = "macos"))]
    let input_permissions_ready = false;
    let input_plan = input_session::plan_runtime(input_role, input_permissions_ready);
    info!(
        role = ?input_role,
        permissions_ready = input_permissions_ready,
        capture = input_plan.start_capture_forwarder,
        inject = input_plan.start_inject_receiver,
        "input runtime plan"
    );
    let input_peer_handler = input_peer_handler(input_plan, input_permissions_ready);
    let trusted_peer_keys = trusted_public_keys();
    let local_identity = load_local_identity(&config_path, &config.device.name)?;
    let session_config = connection::TrustedSessionConfig::new(
        local_identity,
        local_handshake_challenge(&config.device.name),
        trusted_peer_keys,
    );

    // 6. Cross-platform TCP transport: universal desktop fallback for inbound
    //    and trusted rediscovery connections.
    let listen_addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, config.network.listen_port));
    let transport = match nexkvm_network::TcpTransport::bind(listen_addr).await {
        Ok(tcp) => {
            let local_addr = tcp.local_addr().context("resolving TCP listen address")?;
            let transport: Arc<dyn Transport> = Arc::new(tcp);
            connection::spawn_inbound_accept_loop(
                Arc::clone(&transport),
                input_peer_handler,
                Some(session_config.clone()),
            );
            info!(addr = %local_addr, "TCP transport listening");
            Some(transport)
        }
        Err(error) => {
            tracing::warn!(%error, "TCP transport disabled (bind failed)");
            None
        }
    };

    // 7. LAN discovery: advertise this device and auto-reconnect trusted peers.
    //    Kept alive for the daemon's lifetime; dropping it aborts its tasks.
    let _discovery = if config.network.enable_discovery {
        match start_discovery(&device, &config, &config_path, transport, session_config) {
            Ok(service) => Some(service),
            Err(e) => {
                tracing::warn!(error = %e, "LAN discovery disabled (startup failed)");
                None
            }
        }
    } else {
        info!("LAN discovery disabled by config");
        None
    };

    // 8. Run until Ctrl-C, then signal a graceful shutdown on the bus.
    tokio::signal::ctrl_c()
        .await
        .context("waiting for shutdown signal")?;
    info!("shutdown requested");
    bus.publish(nexkvm_core::Event::Shutdown);

    Ok(())
}

fn input_runtime_role(role: nexkvm_storage::InputControlRole) -> input_session::InputRuntimeRole {
    match role {
        nexkvm_storage::InputControlRole::Disabled => input_session::InputRuntimeRole::Disabled,
        nexkvm_storage::InputControlRole::Source => input_session::InputRuntimeRole::Source,
        nexkvm_storage::InputControlRole::Target => input_session::InputRuntimeRole::Target,
        nexkvm_storage::InputControlRole::Both => input_session::InputRuntimeRole::Both,
    }
}

fn input_peer_handler(
    plan: input_session::InputRuntimePlan,
    permissions_ready: bool,
) -> Option<connection::PeerConnectionHandler> {
    if !plan.start_inject_receiver && !plan.start_capture_forwarder {
        return None;
    }
    #[cfg(target_os = "macos")]
    {
        let injector = if plan.start_inject_receiver {
            Some(nexkvm_platform_macos::MacosInputInjector::new(
                permissions_ready,
            ))
        } else {
            None
        };
        let capture = if plan.start_capture_forwarder {
            Some(nexkvm_platform_macos::MacosInputCapture::new(
                permissions_ready,
            ))
        } else {
            None
        };
        let handler: connection::PeerConnectionHandler = Arc::new(move |connection| {
            let connection: Arc<dyn nexkvm_network::Connection> = Arc::from(connection);
            if let Some(injector) = injector.clone() {
                let connection = Arc::clone(&connection);
                tokio::spawn(async move {
                    if let Err(error) =
                        input_session::inject_until_closed(&*connection, &injector).await
                    {
                        tracing::warn!(%error, "input injection session ended");
                    }
                });
            }
            if let Some(capture) = capture.clone() {
                let connection = Arc::clone(&connection);
                tokio::spawn(async move {
                    if let Err(error) =
                        input_session::forward_until_error(&capture, &*connection, MessageId(0))
                            .await
                    {
                        tracing::warn!(%error, "input capture forwarding ended");
                    }
                });
            }
        });
        Some(handler)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = permissions_ready;
        None
    }
}

/// Start LAN discovery: advertise over UDP broadcast and stream trusted-peer
/// reconnect targets to the transport driver. Returns the live service so the
/// caller keeps it alive; dropping it stops discovery.
fn start_discovery(
    device: &DeviceInfo,
    config: &Config,
    config_path: &std::path::Path,
    transport: Option<Arc<dyn Transport>>,
    session_config: connection::TrustedSessionConfig,
) -> anyhow::Result<std::sync::Arc<nexkvm_discovery::DiscoveryService>> {
    use nexkvm_discovery::{DiscoveryService, FingerprintAllowlist, ServiceConfig, UdpDiscovery};
    use nexkvm_storage::FileTrustStore;

    // Build the trust allowlist from persisted pairings (advisory matching).
    let trust_path = config_path
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join("trust.json");
    let allowlist = match FileTrustStore::load(&trust_path) {
        Ok(store) => {
            FingerprintAllowlist::new(store.entries().iter().map(|e| e.public_key.fingerprint()))
        }
        Err(e) => {
            tracing::warn!(error = %e, "trust store unavailable; reconnect disabled");
            FingerprintAllowlist::default()
        }
    };

    let backend = UdpDiscovery::bind(device.id, nexkvm_discovery::UdpConfig::default())
        .context("binding UDP discovery socket")?;
    let service = Arc::new(DiscoveryService::new(
        Arc::new(backend),
        Arc::new(allowlist),
        ServiceConfig::default(),
    ));

    let listen_addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, config.network.listen_port));
    let driver = Arc::clone(&service);
    let info = device.clone();
    tokio::spawn(async move {
        let mut targets = match driver.start(&info, listen_addr).await {
            Ok(rx) => rx,
            Err(e) => {
                tracing::error!(error = %e, "failed to start discovery advertising");
                return;
            }
        };
        info!(port = listen_addr.port(), "LAN discovery advertising");
        if let Some(transport) = transport {
            connection::spawn_reconnect_driver(
                Arc::clone(&driver),
                transport,
                targets,
                Some(session_config),
            );
        } else {
            while let Some(target) = targets.recv().await {
                tracing::warn!(
                    device = %target.device.info.name,
                    addr = %target.device.addr,
                    attempt = target.attempt,
                    "trusted peer rediscovered but no transport is available"
                );
            }
        }
    });

    Ok(service)
}

/// List trusted (paired) devices from the persisted trust store.
fn list_devices() -> anyhow::Result<()> {
    use nexkvm_storage::FileTrustStore;

    let path = trust_path();
    let store = FileTrustStore::load(&path)
        .with_context(|| format!("loading trust store from {}", path.display()))?;
    println!("{}", cli::format_device_list(&store.entries()));
    Ok(())
}

/// Decode or accept a `nexkvm://` pairing bootstrap.
///
/// Without `accept`, this only prints the out-of-band fingerprint for human
/// comparison. With `accept`, it persists the peer key into `trust.json`.
fn pair(uri: &str, accept: bool) -> anyhow::Result<()> {
    use nexkvm_crypto::{PairingBootstrap, TrustEntry, TrustStore};
    use nexkvm_storage::FileTrustStore;
    use std::time::{SystemTime, UNIX_EPOCH};

    let bootstrap = PairingBootstrap::from_uri(uri)
        .context("decoding pairing uri (expected nexkvm://pair/v1/…)")?;
    if !accept {
        println!("{}", cli::format_pairing(&bootstrap));
        return Ok(());
    }

    let paired_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_secs();
    let entry = TrustEntry {
        display_name: bootstrap.display_name,
        public_key: bootstrap.public_key,
        paired_at,
    };
    let path = trust_path();
    let store = FileTrustStore::load(&path)
        .with_context(|| format!("loading trust store from {}", path.display()))?;
    store.insert(entry.clone());
    store
        .flush()
        .with_context(|| format!("writing trust store to {}", path.display()))?;
    println!("{}", cli::format_pairing_accepted(&entry));
    Ok(())
}

/// Generate this device's pairing bootstrap URI.
fn pairing_uri(addr: &str) -> anyhow::Result<()> {
    use nexkvm_crypto::PairingBootstrap;
    use sha2::{Digest, Sha256};
    use std::time::{SystemTime, UNIX_EPOCH};

    let config_path = config_path();
    let config = Config::load(&config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;

    let public_key = load_local_identity(&config_path, &config.device.name)?.public_key();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_nanos();
    let mut hasher = Sha256::new();
    hasher.update(b"nexkvm pairing nonce v1");
    hasher.update(config.device.name.as_bytes());
    hasher.update(addr.as_bytes());
    hasher.update(now.to_be_bytes());
    let digest = hasher.finalize();
    let mut nonce = [0u8; nexkvm_crypto::NONCE_LEN];
    nonce.copy_from_slice(&digest);

    let bootstrap = PairingBootstrap::new(config.device.name, public_key, nonce, addr);
    println!("{}", bootstrap.to_uri());
    Ok(())
}

fn load_local_identity(
    config_path: &std::path::Path,
    device_name: &str,
) -> anyhow::Result<nexkvm_crypto::DeviceKeypair> {
    use nexkvm_storage::FileDeviceIdentityStore;

    let path = identity_path_for(config_path);
    FileDeviceIdentityStore::new(&path)
        .load_or_create(device_name)
        .with_context(|| format!("loading local identity from {}", path.display()))
}

fn local_handshake_challenge(device_name: &str) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    use std::time::{SystemTime, UNIX_EPOCH};

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(b"nexkvm trusted session challenge v1");
    hasher.update(device_name.as_bytes());
    hasher.update(now.to_be_bytes());
    let digest = hasher.finalize();
    let mut challenge = [0u8; 32];
    challenge.copy_from_slice(&digest);
    challenge
}

fn doctor() -> anyhow::Result<()> {
    let path = config_path();
    let config =
        Config::load(&path).with_context(|| format!("loading config from {}", path.display()))?;
    println!("nexkvm doctor");
    println!("  os: {:?}", current_os());
    println!("  config: {}", path.display());
    println!("  device name: {}", config.device.name);
    println!("  discovery: {}", config.network.enable_discovery);
    println!("  transports: {}", config.network.transports.join(","));
    println!("  require pairing: {}", config.security.require_pairing);
    println!("  plugins enabled: {}", config.plugins.enabled);
    println!(
        "  workspace unified desktop: {}",
        config.workspace.unified_desktop
    );
    println!(
        "  collaboration shared cursor: {}",
        config.collaboration.shared_cursor
    );
    match platform_backend() {
        Some(backend) => {
            let report =
                NativeIntegrationReport::from_capabilities(backend.os(), backend.capabilities());
            for line in cli::format_native_integrations(&report).lines() {
                println!("  {line}");
            }
        }
        None => println!("  platform capabilities: headless/unsupported OS"),
    }
    #[cfg(target_os = "macos")]
    {
        let backend = nexkvm_platform_macos::MacosBackend::new();
        let report = backend.input_permission_report();
        let accessibility = match report.accessibility {
            nexkvm_platform_macos::MacosPermissionState::Ready => "ready",
            nexkvm_platform_macos::MacosPermissionState::PermissionRequired => {
                "permission-required"
            }
        };
        for line in cli::format_macos_input_report(
            accessibility,
            report.can_capture_input,
            report.can_inject_input,
            report.next_step,
        )
        .lines()
        {
            println!("  {line}");
        }
    }
    Ok(())
}

async fn permissions() -> anyhow::Result<()> {
    println!("nexkvm permissions");
    #[cfg(target_os = "macos")]
    {
        let backend = nexkvm_platform_macos::MacosBackend::new();
        let _ = backend.request_permissions().await;
        let report = backend.input_permission_report();
        let accessibility = match report.accessibility {
            nexkvm_platform_macos::MacosPermissionState::Ready => "ready",
            nexkvm_platform_macos::MacosPermissionState::PermissionRequired => {
                "permission-required"
            }
        };
        for line in cli::format_macos_input_report(
            accessibility,
            report.can_capture_input,
            report.can_inject_input,
            report.next_step,
        )
        .lines()
        {
            println!("  {line}");
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        println!("  no interactive permission prompt is available on this platform yet");
    }
    Ok(())
}

fn protocol_info() -> anyhow::Result<()> {
    println!("protocol: {PROTOCOL_VERSION}");
    println!("supported: {}", VersionRange::current());
    println!("security: authenticated encrypted sessions required above transport TLS");
    Ok(())
}

fn simulate(path: Option<String>) -> anyhow::Result<()> {
    let path = path.unwrap_or_else(|| "tools/sim/local-workspace.toml".into());
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading simulation config from {path}"))?;
    let devices = text
        .lines()
        .filter(|line| line.trim_start().starts_with("[[device]]"))
        .count();
    println!("simulation config: {path}");
    println!("  devices: {devices}");
    println!("  bytes: {}", text.len());
    println!("  status: valid enough for local sans-IO simulation scaffolding");
    Ok(())
}

/// Resolve the config file path for the current platform.
fn config_path() -> std::path::PathBuf {
    let base = if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        std::path::PathBuf::from(xdg)
    } else if let Ok(home) = std::env::var("HOME") {
        let home = std::path::PathBuf::from(home);
        if cfg!(target_os = "macos") {
            home.join("Library/Application Support")
        } else {
            home.join(".config")
        }
    } else if let Ok(appdata) = std::env::var("APPDATA") {
        std::path::PathBuf::from(appdata)
    } else {
        std::path::PathBuf::from(".")
    };
    base.join("nexkvm").join("config.toml")
}

/// Resolve the trust-store path (sibling of the config file).
fn trust_path() -> std::path::PathBuf {
    config_path()
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join("trust.json")
}

fn identity_path_for(config_path: &std::path::Path) -> std::path::PathBuf {
    config_path
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join("identity.json")
}

fn trusted_public_keys() -> Vec<nexkvm_crypto::PublicKey> {
    use nexkvm_storage::FileTrustStore;

    match FileTrustStore::load(trust_path()) {
        Ok(store) => store
            .entries()
            .into_iter()
            .map(|entry| entry.public_key)
            .collect(),
        Err(error) => {
            tracing::warn!(%error, "trust store unavailable; trusted sessions disabled");
            Vec::new()
        }
    }
}

/// Construct the platform backend for the current OS, if one exists.
fn platform_backend() -> Option<Box<dyn PlatformBackend>> {
    #[cfg(target_os = "macos")]
    {
        Some(Box::new(nexkvm_platform_macos::MacosBackend::new()))
    }
    #[cfg(target_os = "linux")]
    {
        Some(Box::new(nexkvm_platform_linux::LinuxBackend::new()))
    }
    #[cfg(target_os = "windows")]
    {
        Some(Box::new(nexkvm_platform_windows::WindowsBackend::new()))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        None
    }
}
