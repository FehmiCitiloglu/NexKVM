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
use nexkvm_protocol::{PROTOCOL_VERSION, VersionRange};
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
        Command::Protocol => return protocol_info(),
        Command::ConfigPath => {
            println!("{}", config_path().display());
            return Ok(());
        }
        Command::Devices => return list_devices(),
        Command::Pair { uri } => return pair(&uri),
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

    // 6. Cross-platform TCP transport: universal desktop fallback for inbound
    //    and trusted rediscovery connections.
    let listen_addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, config.network.listen_port));
    let transport = match nexkvm_network::TcpTransport::bind(listen_addr).await {
        Ok(tcp) => {
            let local_addr = tcp.local_addr().context("resolving TCP listen address")?;
            let transport: Arc<dyn Transport> = Arc::new(tcp);
            connection::spawn_inbound_accept_loop(Arc::clone(&transport));
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
        match start_discovery(&device, &config, &config_path, transport) {
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

/// Start LAN discovery: advertise over UDP broadcast and stream trusted-peer
/// reconnect targets to the transport driver. Returns the live service so the
/// caller keeps it alive; dropping it stops discovery.
fn start_discovery(
    device: &DeviceInfo,
    config: &Config,
    config_path: &std::path::Path,
    transport: Option<Arc<dyn Transport>>,
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
            connection::spawn_reconnect_driver(Arc::clone(&driver), transport, targets);
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

/// Decode a `nexkvm://` pairing bootstrap and print it for fingerprint
/// confirmation. The network handshake itself is wired in a later phase; this
/// surfaces the out-of-band authenticator the user must verify.
fn pair(uri: &str) -> anyhow::Result<()> {
    use nexkvm_crypto::PairingBootstrap;

    let bootstrap = PairingBootstrap::from_uri(uri)
        .context("decoding pairing uri (expected nexkvm://pair/v1/…)")?;
    println!("{}", cli::format_pairing(&bootstrap));
    Ok(())
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
