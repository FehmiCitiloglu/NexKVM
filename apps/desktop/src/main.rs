//! coklu desktop daemon entry point.
//!
//! Foundation-phase wiring: initialize telemetry, load config, construct the
//! event bus and platform backend, report negotiated protocol version and
//! resolved capabilities, then run until interrupted. Networking, discovery,
//! input pipelines, and the plugin host are attached in subsequent phases.

use anyhow::Context;
use coklu_core::platform::PlatformBackend;
use coklu_core::{DeviceInfo, EventBus};
use coklu_protocol::{PROTOCOL_VERSION, VersionRange};
use coklu_storage::{Config, current_os};
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("doctor") => return doctor(),
        Some("protocol") => return protocol_info(),
        Some("config-path") => {
            println!("{}", config_path().display());
            return Ok(());
        }
        Some("simulate") => return simulate(args.next()),
        Some("help" | "--help" | "-h") => {
            print_help();
            return Ok(());
        }
        Some(other) => anyhow::bail!("unknown command `{other}`; run `coklu help`"),
        None => {}
    }

    // 1. Config: load from the platform config dir (falls back to defaults).
    let config_path = config_path();
    let config = Config::load(&config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;

    // 2. Telemetry: install the tracing subscriber before anything else logs.
    coklu_telemetry::init(&config.telemetry).context("initializing telemetry")?;

    info!(
        version = %PROTOCOL_VERSION,
        supported = %VersionRange::current(),
        "starting coklu daemon"
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

    // 6. LAN discovery: advertise this device and auto-reconnect trusted peers.
    //    Kept alive for the daemon's lifetime; dropping it aborts its tasks.
    let _discovery = if config.network.enable_discovery {
        match start_discovery(&device, &config, &config_path) {
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

    // 7. Run until Ctrl-C, then signal a graceful shutdown on the bus.
    tokio::signal::ctrl_c()
        .await
        .context("waiting for shutdown signal")?;
    info!("shutdown requested");
    bus.publish(coklu_core::Event::Shutdown);

    Ok(())
}

/// Start LAN discovery: advertise over UDP broadcast and stream trusted-peer
/// reconnect targets to a logging driver. Returns the live service so the caller
/// keeps it alive; dropping it stops discovery.
///
/// Networking is wired in a later phase, so reconnect targets are logged rather
/// than dialed; the discovery, trust-gating, and backoff machinery all run.
fn start_discovery(
    device: &DeviceInfo,
    config: &Config,
    config_path: &std::path::Path,
) -> anyhow::Result<std::sync::Arc<coklu_discovery::DiscoveryService>> {
    use std::net::{Ipv4Addr, SocketAddr};
    use std::sync::Arc;

    use coklu_discovery::{DiscoveryService, FingerprintAllowlist, ServiceConfig, UdpDiscovery};
    use coklu_storage::FileTrustStore;

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

    let backend = UdpDiscovery::bind(device.id, coklu_discovery::UdpConfig::default())
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
        while let Some(target) = targets.recv().await {
            // Phase placeholder: network crate will dial and report back.
            info!(
                device = %target.device.info.name,
                addr = %target.device.addr,
                attempt = target.attempt,
                "trusted peer rediscovered; reconnect pending network integration"
            );
        }
    });

    Ok(service)
}

fn print_help() {
    println!("coklu developer CLI");
    println!();
    println!("USAGE:");
    println!("  coklu                 Run the desktop daemon");
    println!("  coklu doctor          Print local platform/config diagnostics");
    println!("  coklu protocol        Print protocol compatibility info");
    println!("  coklu config-path     Print the resolved config path");
    println!("  coklu simulate [toml] Validate a local simulation config");
}

fn doctor() -> anyhow::Result<()> {
    let path = config_path();
    let config =
        Config::load(&path).with_context(|| format!("loading config from {}", path.display()))?;
    println!("coklu doctor");
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
        Some(backend) => println!("  platform capabilities: {:?}", backend.capabilities()),
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
    base.join("coklu").join("config.toml")
}

/// Construct the platform backend for the current OS, if one exists.
fn platform_backend() -> Option<Box<dyn PlatformBackend>> {
    #[cfg(target_os = "macos")]
    {
        Some(Box::new(coklu_platform_macos::MacosBackend::new()))
    }
    #[cfg(target_os = "linux")]
    {
        Some(Box::new(coklu_platform_linux::LinuxBackend::new()))
    }
    #[cfg(target_os = "windows")]
    {
        Some(Box::new(coklu_platform_windows::WindowsBackend::new()))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        None
    }
}
