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
use serde::Deserialize;
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
    let (input_can_capture, input_can_inject) = {
        let macos = nexkvm_platform_macos::MacosBackend::new();
        let report = macos.input_permission_report();
        (report.can_capture_input, report.can_inject_input)
    };
    #[cfg(not(target_os = "macos"))]
    let (input_can_capture, input_can_inject) = backend
        .as_ref()
        .map(|backend| {
            let caps = backend.capabilities();
            (
                caps.can_capture_input && !caps.permission_pending,
                caps.can_inject_input && !caps.permission_pending,
            )
        })
        .unwrap_or((false, false));
    let input_plan = input_session::plan_runtime(input_role, input_can_capture, input_can_inject);
    let input_permissions_ready =
        input_permissions_ready(input_role, input_can_capture, input_can_inject);
    info!(
        role = ?input_role,
        permissions_ready = input_permissions_ready,
        capture = input_plan.start_capture_forwarder,
        inject = input_plan.start_inject_receiver,
        "input runtime plan"
    );
    let input_handoff_edge = input_handoff_edge(config.input.handoff_edge);
    let input_peer_handler = input_peer_handler(
        input_plan,
        input_can_capture,
        input_can_inject,
        input_handoff_edge,
        config.input.emergency_stop_keycode,
        config.input.remote_focus_timeout_millis,
    );
    let trusted_peer_keys = trusted_public_keys();
    let local_identity = load_local_identity(&config_path, &config.device.name)?;
    let local_fingerprint = local_identity.public_key().fingerprint();
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
                input_peer_handler.clone(),
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

    if let Some(connect_addr) = config
        .network
        .connect_addr
        .as_deref()
        .map(str::trim)
        .filter(|addr| !addr.is_empty())
    {
        match transport.as_ref() {
            Some(transport) => {
                info!(endpoint = connect_addr, "explicit peer connect configured");
                connection::spawn_explicit_connect_driver(
                    Arc::clone(transport),
                    connect_addr.to_owned(),
                    Some(session_config.clone()),
                    input_peer_handler.clone(),
                );
            }
            None => {
                tracing::warn!(
                    endpoint = connect_addr,
                    "explicit peer connect disabled because transport is unavailable"
                );
            }
        }
    }

    // 7. LAN discovery: advertise this device and auto-reconnect trusted peers.
    //    Kept alive for the daemon's lifetime; dropping it aborts its tasks.
    let _discovery = if config.network.enable_discovery {
        match start_discovery(
            &device,
            &config,
            &config_path,
            transport,
            session_config,
            local_fingerprint,
            input_peer_handler.clone(),
        ) {
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
    capture_ready: bool,
    inject_ready: bool,
    handoff_edge: input_session::HandoffEdge,
    emergency_stop_keycode: u32,
    remote_focus_timeout_millis: u64,
) -> Option<connection::PeerConnectionHandler> {
    if !plan.start_inject_receiver && !plan.start_capture_forwarder {
        return None;
    }
    #[cfg(target_os = "macos")]
    {
        let injector = if plan.start_inject_receiver {
            Some(nexkvm_platform_macos::MacosInputInjector::new(inject_ready))
        } else {
            None
        };
        let capture = if plan.start_capture_forwarder {
            Some(nexkvm_platform_macos::MacosInputCapture::new(capture_ready))
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
                    let capture_for_suppression = capture.clone();
                    if let Err(error) = input_session::forward_extended_until_error(
                        &capture,
                        &*connection,
                        MessageId(0),
                        handoff_edge,
                        emergency_stop_keycode,
                        remote_focus_timeout_millis,
                        move |suppressed| capture_for_suppression.set_suppressed(suppressed),
                    )
                    .await
                    {
                        handle_input_capture_end(error);
                    }
                });
            }
        });
        Some(handler)
    }
    #[cfg(target_os = "windows")]
    {
        let _ = (capture_ready, inject_ready);
        let injector = if plan.start_inject_receiver {
            Some(nexkvm_platform_windows::WindowsInputInjector::new())
        } else {
            None
        };
        let capture = if plan.start_capture_forwarder {
            Some(nexkvm_platform_windows::WindowsInputCapture::new())
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
                        tracing::warn!(%error, "Windows input injection session ended");
                    }
                });
            }
            if let Some(capture) = capture.clone() {
                let connection = Arc::clone(&connection);
                tokio::spawn(async move {
                    let capture_for_suppression = capture.clone();
                    if let Err(error) = input_session::forward_extended_until_error(
                        &capture,
                        &*connection,
                        MessageId(0),
                        handoff_edge,
                        emergency_stop_keycode,
                        remote_focus_timeout_millis,
                        move |suppressed| capture_for_suppression.set_suppressed(suppressed),
                    )
                    .await
                    {
                        handle_input_capture_end(error);
                    }
                });
            }
        });
        Some(handler)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = (capture_ready, inject_ready);
        None
    }
}

fn handle_input_capture_end(error: input_session::InputSessionError) {
    if matches!(error, input_session::InputSessionError::EmergencyStop) {
        tracing::warn!("emergency stop requested; exiting nexkvm");
        std::process::exit(0);
    }
    tracing::warn!(%error, "input capture forwarding ended");
}

fn input_handoff_edge(edge: nexkvm_storage::InputHandoffEdge) -> input_session::HandoffEdge {
    match edge {
        nexkvm_storage::InputHandoffEdge::Left => input_session::HandoffEdge::Left,
        nexkvm_storage::InputHandoffEdge::Right => input_session::HandoffEdge::Right,
        nexkvm_storage::InputHandoffEdge::Top => input_session::HandoffEdge::Top,
        nexkvm_storage::InputHandoffEdge::Bottom => input_session::HandoffEdge::Bottom,
    }
}

fn input_permissions_ready(
    role: input_session::InputRuntimeRole,
    can_capture_input: bool,
    can_inject_input: bool,
) -> bool {
    match role {
        input_session::InputRuntimeRole::Disabled => true,
        input_session::InputRuntimeRole::Source => can_capture_input,
        input_session::InputRuntimeRole::Target => can_inject_input,
        input_session::InputRuntimeRole::Both => can_capture_input && can_inject_input,
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
    local_fingerprint: String,
    input_peer_handler: Option<connection::PeerConnectionHandler>,
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
        let mut targets = match driver
            .start(&info, listen_addr, Some(&local_fingerprint))
            .await
        {
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
                input_peer_handler,
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
    println!(
        "  explicit connect: {}",
        config.network.connect_addr.as_deref().unwrap_or("disabled")
    );
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
        if !report.can_capture_input || !report.can_inject_input {
            open_macos_accessibility_settings();
            println!(
                "  opened settings: add nexkvm.app or the terminal app you use, then restart nexkvm"
            );
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn open_macos_accessibility_settings() {
    let _ = std::process::Command::new("open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
        .status();
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SimConfig {
    network: SimNetwork,
    device: Vec<SimDevice>,
    features: SimFeatures,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SimNetwork {
    profile: String,
    rtt_ms: u64,
    jitter_ms: u64,
    loss: f64,
    throughput_bps: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SimDevice {
    name: String,
    os: String,
    role: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    address: Option<String>,
    #[serde(default)]
    trusted: Option<bool>,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SimFeatures {
    clipboard: bool,
    file_transfer: bool,
    screen_preview: bool,
    shared_cursor: bool,
    plugins: bool,
}

fn validate_sim_config(config: &SimConfig) -> anyhow::Result<()> {
    if config.device.is_empty() {
        anyhow::bail!("simulation config must define at least one [[device]]")
    }

    let mut seen_names = std::collections::HashSet::new();
    for device in &config.device {
        if !seen_names.insert(device.name.as_str()) {
            anyhow::bail!(
                "duplicate device name `{}` in simulation config",
                device.name
            );
        }
        if !matches!(
            device.os.as_str(),
            "macos" | "windows" | "linux-wayland" | "linux-x11" | "android" | "ios"
        ) {
            anyhow::bail!(
                "unknown device os `{}` for `{}`; expected one of macos, windows, linux-wayland, linux-x11, android, ios",
                device.os,
                device.name
            );
        }
    }

    Ok(())
}

fn simulated_device_id(device: &SimDevice) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(b"nexkvm simulate device id v1");
    hasher.update(device.name.as_bytes());
    hasher.update(device.os.as_bytes());
    hasher.update(device.address.as_deref().unwrap_or(""));
    let digest = hasher.finalize();
    format!(
        "sim-{:02x}{:02x}{:02x}{:02x}",
        digest[0], digest[1], digest[2], digest[3]
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SimConnectionPlanKind {
    DirectLan,
    ReconnectCandidate,
    MissingTrust,
    InvalidConfiguration,
}

impl SimConnectionPlanKind {
    fn label(self) -> &'static str {
        match self {
            Self::DirectLan => "direct-lan",
            Self::ReconnectCandidate => "reconnect-candidate",
            Self::MissingTrust => "missing-trust",
            Self::InvalidConfiguration => "invalid-configuration",
        }
    }
}

struct SimConnectionPlanEntry<'a> {
    device: &'a SimDevice,
    kind: SimConnectionPlanKind,
    detail: String,
}

fn build_connection_plan<'a>(device: &'a SimDevice) -> SimConnectionPlanEntry<'a> {
    if !matches!(device.role.as_str(), "server" | "client") {
        return SimConnectionPlanEntry {
            device,
            kind: SimConnectionPlanKind::InvalidConfiguration,
            detail: format!("unsupported role `{}`", device.role),
        };
    }

    if let Some(address) = device.address.as_deref() {
        if address.parse::<SocketAddr>().is_err() {
            return SimConnectionPlanEntry {
                device,
                kind: SimConnectionPlanKind::InvalidConfiguration,
                detail: format!("invalid address `{address}` (expected ip:port)"),
            };
        }
    }

    if !device.trusted.unwrap_or(false) {
        return SimConnectionPlanEntry {
            device,
            kind: SimConnectionPlanKind::MissingTrust,
            detail: "device is not trusted".to_string(),
        };
    }

    if let Some(address) = device.address.as_deref() {
        return SimConnectionPlanEntry {
            device,
            kind: SimConnectionPlanKind::DirectLan,
            detail: format!("connect directly to {address}"),
        };
    }

    SimConnectionPlanEntry {
        device,
        kind: SimConnectionPlanKind::ReconnectCandidate,
        detail: "trusted device without address; wait for discovery".to_string(),
    }
}

#[derive(Debug, Clone)]
struct SimRuntimeDevice {
    id: nexkvm_core::DeviceId,
    name: String,
    address: Option<SocketAddr>,
    trusted: bool,
    bounds: nexkvm_core::WorkspaceRect,
}

fn build_runtime_devices(config: &SimConfig) -> Vec<SimRuntimeDevice> {
    config
        .device
        .iter()
        .map(|device| SimRuntimeDevice {
            id: nexkvm_core::DeviceId::generate(),
            name: device
                .display_name
                .clone()
                .unwrap_or_else(|| device.name.clone()),
            address: device
                .address
                .as_deref()
                .and_then(|address| address.parse::<SocketAddr>().ok()),
            trusted: device.trusted.unwrap_or(false),
            bounds: nexkvm_core::WorkspaceRect::new(
                device.x,
                device.y,
                device.width,
                device.height,
            ),
        })
        .collect()
}

fn print_simulator_report(runtime_devices: &[SimRuntimeDevice], network: &SimNetwork) {
    println!("  simulators:");
    print_discovery_simulator(runtime_devices);
    print_latency_simulator(network);
    print_workspace_simulator(runtime_devices);
    print_screen_simulator(runtime_devices);
    print_collaboration_simulator(runtime_devices);
}

fn print_discovery_simulator(runtime_devices: &[SimRuntimeDevice]) {
    use nexkvm_discovery::{
        PresencePolicy, PresenceTracker, ProximityObservation, ProximitySignalKind,
    };

    let mut tracker = PresenceTracker::new(PresencePolicy::lan_default());
    let now_millis = 1_000;
    for device in runtime_devices {
        let mut confidence: f64 = if device.trusted { 0.85 } else { 0.35 };
        if device.address.is_none() {
            confidence = (confidence - 0.2_f64).max(0.05_f64);
        }
        tracker.observe(ProximityObservation::new(
            device.id,
            ProximitySignalKind::LanHeartbeat,
            confidence,
            now_millis,
        ));
        if device.trusted {
            tracker.observe(ProximityObservation::new(
                device.id,
                ProximitySignalKind::PresenceHint,
                0.9,
                now_millis,
            ));
        }
    }

    let ranked = tracker.ranked(now_millis);
    let best_active = tracker
        .best_active(now_millis)
        .and_then(|id| runtime_devices.iter().find(|device| device.id == id))
        .map(|device| device.name.clone())
        .unwrap_or_else(|| "none".to_string());

    println!(
        "    - discovery: ranked={} best_active={}",
        ranked.len(),
        best_active
    );
}

fn print_latency_simulator(network: &SimNetwork) {
    use nexkvm_network::RttTracker;

    let mut tracker = RttTracker::default();
    let base = network.rtt_ms.max(1);
    let jitter = network.jitter_ms;
    let low = base.saturating_sub(jitter).max(1);
    let high = base.saturating_add(jitter).max(1);
    tracker.record(std::time::Duration::from_millis(low));
    tracker.record(std::time::Duration::from_millis(base));
    tracker.record(std::time::Duration::from_millis(high));

    let smoothed = tracker.smoothed().unwrap_or_default().as_millis();
    let estimated_jitter = tracker.jitter().unwrap_or_default().as_millis();
    let timeout = tracker
        .timeout(std::time::Duration::from_millis(250))
        .as_millis();

    println!(
        "    - latency: smoothed={}ms jitter={}ms timeout={}ms",
        smoothed, estimated_jitter, timeout,
    );
}

fn print_workspace_simulator(runtime_devices: &[SimRuntimeDevice]) {
    use nexkvm_core::{
        SnapDirection, UnifiedVirtualDesktop, WindowId, WindowSnapshot, WorkspaceDevice,
        plan_window_snap,
    };

    let mut desktop = UnifiedVirtualDesktop::new();
    for device in runtime_devices {
        desktop.upsert(
            WorkspaceDevice::new(device.id, device.name.clone(), device.bounds).with_online(true),
        );
    }

    if runtime_devices.is_empty() {
        println!("    - workspace: no devices");
        return;
    }

    let source = &runtime_devices[0];
    let window = WindowSnapshot {
        id: WindowId::new("sim-window-main"),
        device: source.id,
        title: "Simulation Window".to_string(),
        app_id: None,
        bounds: source.bounds,
        visible: true,
    };

    match plan_window_snap(&desktop, &window, SnapDirection::Right) {
        Ok(plan) => {
            let target = runtime_devices
                .iter()
                .find(|device| device.id == plan.to)
                .map(|device| device.name.as_str())
                .unwrap_or("unknown");
            println!(
                "    - workspace: snap_right target={} cross_device={}",
                target, plan.cross_device
            );
        }
        Err(error) => println!("    - workspace: unavailable ({error})"),
    }
}

fn print_screen_simulator(runtime_devices: &[SimRuntimeDevice]) {
    use nexkvm_streaming::{
        CaptureSource, CaptureSourceId, ScreenStreamCapabilities, ScreenStreamRequest,
        negotiate_screen_stream,
    };

    if runtime_devices.len() < 2 {
        println!("    - screen: unavailable (need at least 2 devices)");
        return;
    }

    let sender = &runtime_devices[0];
    let receiver = &runtime_devices[1];
    let request = ScreenStreamRequest::interactive(
        sender.id,
        receiver.id,
        CaptureSource::Display {
            id: CaptureSourceId::new("sim-display-0"),
            label: sender.name.clone(),
        },
    );

    match negotiate_screen_stream(
        &ScreenStreamCapabilities::software_h264(),
        &ScreenStreamCapabilities::software_h264(),
        request,
    ) {
        Ok(plan) => println!(
            "    - screen: codec={:?} fps={} resolution={}x{} encrypted={}",
            plan.codec,
            plan.fps,
            plan.resolution.width,
            plan.resolution.height,
            plan.requires_encrypted_transport
        ),
        Err(error) => println!("    - screen: unavailable ({error})"),
    }
}

fn print_collaboration_simulator(runtime_devices: &[SimRuntimeDevice]) {
    use nexkvm_core::{
        CollaborationMode, CollaborationParticipant, CollaborationSession, ParticipantRole,
    };

    if runtime_devices.len() < 2 {
        println!("    - collaboration: unavailable (need at least 2 devices)");
        return;
    }

    let host = CollaborationParticipant::new(
        runtime_devices[0].id,
        runtime_devices[0].name.clone(),
        ParticipantRole::Host,
    );
    let host_id = host.id;
    let peer = CollaborationParticipant::new(
        runtime_devices[1].id,
        runtime_devices[1].name.clone(),
        ParticipantRole::Driver,
    );
    let peer_id = peer.id;

    let mut session = CollaborationSession::new(host, CollaborationMode::PairProgramming);
    let result = (|| -> Result<(), nexkvm_core::CollaborationError> {
        session.join(peer)?;
        let _ = session.request_control(peer_id, runtime_devices[0].id, 1_000)?;
        let _ =
            session.grant_control(host_id, peer_id, runtime_devices[0].id, 1_000, Some(30_000))?;
        Ok(())
    })();

    match result {
        Ok(()) => println!(
            "    - collaboration: participants={} pending_requests={} control_active={}",
            session.participants().len(),
            session.pending_requests().len(),
            session.can_control(peer_id, runtime_devices[0].id, 1_001)
        ),
        Err(error) => println!("    - collaboration: unavailable ({error})"),
    }
}

fn simulate(path: Option<String>) -> anyhow::Result<()> {
    let path = path.unwrap_or_else(|| "tools/sim/local-workspace.toml".into());
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading simulation config from {path}"))?;
    let config: SimConfig = toml::from_str(&text)
        .with_context(|| format!("parsing simulation config TOML from {path}"))?;
    validate_sim_config(&config)?;

    println!("simulation config: {path}");
    println!(
        "  network: profile={} rtt={}ms jitter={}ms loss={} throughput={}bps",
        config.network.profile,
        config.network.rtt_ms,
        config.network.jitter_ms,
        config.network.loss,
        config.network.throughput_bps,
    );
    println!("  devices: {}", config.device.len());
    let mut plans = Vec::with_capacity(config.device.len());
    for device in &config.device {
        plans.push(build_connection_plan(device));
        let id = device
            .id
            .clone()
            .unwrap_or_else(|| simulated_device_id(device));
        let display_name = device
            .display_name
            .clone()
            .unwrap_or_else(|| device.name.clone());
        let address = device.address.as_deref().unwrap_or("unassigned");
        let trust_state = if device.trusted.unwrap_or(false) {
            "trusted"
        } else {
            "untrusted"
        };
        println!(
            "    - id={} display_name={} os={} address={} trust={} role={} pos=({}, {}) size={}x{}",
            id,
            display_name,
            device.os,
            address,
            trust_state,
            device.role,
            device.x,
            device.y,
            device.width,
            device.height,
        );
    }
    println!("  connection planning:");
    for plan in &plans {
        let display_name = plan
            .device
            .display_name
            .as_deref()
            .unwrap_or(plan.device.name.as_str());
        println!(
            "    - {}: {} ({})",
            display_name,
            plan.kind.label(),
            plan.detail
        );
    }
    println!(
        "  features: clipboard={} file_transfer={} screen_preview={} shared_cursor={} plugins={}",
        config.features.clipboard,
        config.features.file_transfer,
        config.features.screen_preview,
        config.features.shared_cursor,
        config.features.plugins,
    );
    let runtime_devices = build_runtime_devices(&config);
    print_simulator_report(&runtime_devices, &config.network);
    println!("  bytes: {}", text.len());
    println!("  status: typed TOML parsed and validated");
    Ok(())
}

/// Resolve the config file path for the current platform.
fn config_path() -> std::path::PathBuf {
    let base = if cfg!(target_os = "windows") {
        std::env::var("APPDATA")
            .map(std::path::PathBuf::from)
            .or_else(|_| {
                std::env::var("USERPROFILE")
                    .map(std::path::PathBuf::from)
                    .map(|home| home.join("AppData").join("Roaming"))
            })
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
    } else if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        std::path::PathBuf::from(xdg)
    } else if let Ok(home) = std::env::var("HOME") {
        let home = std::path::PathBuf::from(home);
        if cfg!(target_os = "macos") {
            home.join("Library/Application Support")
        } else {
            home.join(".config")
        }
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
