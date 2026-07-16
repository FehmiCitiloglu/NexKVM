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

const EFFECTIVE_TRANSPORT: &str = "tcp";

mod automatic_pairing;
mod cli;
mod clipboard_history;
mod clipboard_runtime;
mod connection;
mod file_transfer;
mod input_config;
mod input_session;
mod peer_session;
#[cfg(test)]
mod simulation;

use cli::{AudioSmokeAction, Command};

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
        Command::PortalSmoke => return portal_smoke().await,
        Command::PipeWireSmoke => return pipewire_smoke().await,
        Command::AudioSmoke { action } => return audio_smoke(action).await,
        Command::Protocol => return protocol_info(),
        Command::ConfigPath => {
            println!("{}", config_path().display());
            return Ok(());
        }
        Command::Devices => return list_devices(),
        Command::Pair { uri, accept } => return pair(&uri, accept),
        Command::PairAuto { peer } => {
            return automatic_pairing::initiate(&peer, config_path(), trust_path()).await;
        }
        Command::PairingUri { addr } => return pairing_uri(&addr),
        Command::ClipboardHistory { json } => return clipboard_history_list(json),
        Command::ClipboardRestore { fingerprint } => {
            return clipboard_history_restore(fingerprint).await;
        }
        Command::ClipboardClear => return clipboard_history_clear(),
        Command::FileSend { paths } => return file_send(paths),
        Command::Simulate { path, json_only } => return simulate(path, json_only),
        Command::Help => {
            print!("{}", cli::help_text());
            return Ok(());
        }
    }
}

fn file_send(paths: Vec<String>) -> anyhow::Result<()> {
    let config_path = config_path();
    let config = Config::load(&config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;
    let paths = paths
        .into_iter()
        .map(std::path::PathBuf::from)
        .collect::<Vec<_>>();
    let transfer_id = file_transfer::enqueue_paths(&config_path, &config.file_transfer, &paths)?;
    println!("queued file transfer {}", transfer_id.0);
    Ok(())
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

    if let Some(warning) = unsupported_transport_warning(&config.network.transports) {
        tracing::warn!(%warning);
    }

    info!(
        version = %PROTOCOL_VERSION,
        supported = %VersionRange::current(),
        "starting nexkvm daemon"
    );

    // 3. Identity for this device. The routing id is deterministically bound to
    // the persisted signing key so topology/history attribution survives restarts.
    let local_identity = load_local_identity(&config_path, &config.device.name)?;
    let local_public_key = local_identity.public_key();
    let device = DeviceInfo {
        id: stable_device_id(&local_public_key),
        name: config.device.name.clone(),
        os: current_os(),
    };
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
        pairing_policy = "required (authenticated trusted peers only)",
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
    let input_handoff_edges =
        input_config::spawn_input_handoff_reload(config_path.clone(), input_handoff_edge);
    let active_peer_selection = resolve_active_peer_selection(config.input.active_peer.as_deref());
    let input_tasks = input_session::InputTaskSupervisor::new();
    let input_peer_handler = input_peer_handler(
        InputPeerRuntimeConfig {
            plan: input_plan,
            capture_ready: input_can_capture,
            inject_ready: input_can_inject,
            forwarding: input_session::InputForwardingConfig {
                emergency_stop_keycode: config.input.emergency_stop_keycode,
                remote_focus_timeout_millis: config.input.remote_focus_timeout_millis,
            },
        },
        input_handoff_edges,
        active_peer_selection.clone(),
        input_tasks.clone(),
    );
    let clipboard_can_access = {
        #[cfg(target_os = "macos")]
        {
            let macos = nexkvm_platform_macos::MacosBackend::new();
            macos.capabilities().can_access_clipboard
        }
        #[cfg(not(target_os = "macos"))]
        {
            backend
                .as_ref()
                .map(|b| b.capabilities().can_access_clipboard)
                .unwrap_or(false)
        }
    };
    let clipboard_history =
        clipboard_history::ClipboardHistoryRecorder::open(&config_path, &config.clipboard)
            .context("opening encrypted clipboard history")?;
    let _clipboard_history_task =
        start_clipboard_history_runtime(clipboard_can_access, clipboard_history.clone(), device.id);
    let clipboard_peer_handler = create_clipboard_peer_handler(
        clipboard_runtime_enabled(config.clipboard.sync_enabled, clipboard_can_access),
        device.id,
        clipboard_history,
        active_peer_selection.clone(),
    );
    let file_transfer_peer_handler = file_transfer::create_peer_handler(
        config_path.clone(),
        config.file_transfer.clone(),
        device.id,
        active_peer_selection,
    );
    let local_fingerprint = local_public_key.fingerprint();
    let trust_path = trust_path();
    let session_config =
        connection::TrustedSessionConfig::from_trust_path(local_identity, trust_path.clone());
    let pairing_handler = automatic_pairing::spawn_responder(
        config_path.clone(),
        trust_path,
        local_public_key.clone(),
    );

    // Create handlers once at the top level so they can be used in all spawn sites
    let peer_handlers = merge_peer_handlers(
        local_public_key.clone(),
        input_peer_handler.clone(),
        input_plan.start_inject_receiver,
        clipboard_peer_handler.clone(),
        file_transfer_peer_handler.clone(),
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
                peer_handlers.clone(),
                Some(session_config.clone()),
                Some(pairing_handler),
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
                    peer_handlers.clone(),
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
            peer_handlers,
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

    // 8. Run until the terminal or GUI requests a graceful shutdown.
    let shutdown_signal = wait_for_shutdown_signal().await?;
    info!(?shutdown_signal, "shutdown requested");
    bus.publish(nexkvm_core::Event::Shutdown);
    if !input_tasks
        .shutdown(std::time::Duration::from_secs(3))
        .await
    {
        tracing::warn!("input runtime cleanup exceeded the shutdown deadline");
    }

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShutdownSignal {
    Interrupt,
    #[cfg(unix)]
    Terminate,
}

fn configured_shutdown_signals() -> &'static [ShutdownSignal] {
    #[cfg(unix)]
    {
        &[ShutdownSignal::Interrupt, ShutdownSignal::Terminate]
    }
    #[cfg(not(unix))]
    {
        &[ShutdownSignal::Interrupt]
    }
}

async fn wait_for_shutdown_signal() -> anyhow::Result<ShutdownSignal> {
    tracing::debug!(signals = ?configured_shutdown_signals(), "shutdown handlers configured");
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .context("installing SIGTERM shutdown handler")?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                result.context("waiting for Ctrl-C shutdown signal")?;
                Ok(ShutdownSignal::Interrupt)
            }
            received = terminate.recv() => {
                received.context("SIGTERM shutdown stream ended unexpectedly")?;
                Ok(ShutdownSignal::Terminate)
            }
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .context("waiting for Ctrl-C shutdown signal")?;
        Ok(ShutdownSignal::Interrupt)
    }
}

fn input_runtime_role(role: nexkvm_storage::InputControlRole) -> input_session::InputRuntimeRole {
    match role {
        nexkvm_storage::InputControlRole::Disabled => input_session::InputRuntimeRole::Disabled,
        nexkvm_storage::InputControlRole::Source => input_session::InputRuntimeRole::Source,
        nexkvm_storage::InputControlRole::Target => input_session::InputRuntimeRole::Target,
        nexkvm_storage::InputControlRole::Both => input_session::InputRuntimeRole::Both,
    }
}

fn storage_input_role_label(role: nexkvm_storage::InputControlRole) -> &'static str {
    match role {
        nexkvm_storage::InputControlRole::Disabled => "disabled",
        nexkvm_storage::InputControlRole::Source => "source",
        nexkvm_storage::InputControlRole::Target => "target",
        nexkvm_storage::InputControlRole::Both => "both",
    }
}

fn storage_input_edge_label(edge: nexkvm_storage::InputHandoffEdge) -> &'static str {
    match edge {
        nexkvm_storage::InputHandoffEdge::Left => "left",
        nexkvm_storage::InputHandoffEdge::Right => "right",
        nexkvm_storage::InputHandoffEdge::Top => "top",
        nexkvm_storage::InputHandoffEdge::Bottom => "bottom",
    }
}

#[derive(Debug, Clone, Copy)]
struct InputPeerRuntimeConfig {
    plan: input_session::InputRuntimePlan,
    capture_ready: bool,
    inject_ready: bool,
    forwarding: input_session::InputForwardingConfig,
}

fn input_peer_handler(
    config: InputPeerRuntimeConfig,
    handoff_edges: tokio::sync::watch::Receiver<input_session::HandoffEdge>,
    active_peer: ActivePeerSelection,
    task_supervisor: input_session::InputTaskSupervisor,
) -> Option<connection::PeerConnectionHandler> {
    if !config.plan.start_inject_receiver && !config.plan.start_capture_forwarder {
        return None;
    }
    #[cfg(target_os = "macos")]
    {
        let injector = if config.plan.start_inject_receiver {
            Some(nexkvm_platform_macos::MacosInputInjector::new(
                config.inject_ready,
            ))
        } else {
            None
        };
        let capture = if config.plan.start_capture_forwarder {
            Some(nexkvm_platform_macos::MacosInputCapture::new(
                config.capture_ready,
            ))
        } else {
            None
        };
        let input_forwarder_gate = Arc::new(input_session::InputForwarderGate::default());
        let handler: connection::PeerConnectionHandler =
            Arc::new(move |connection, mut context| {
                if !active_peer.allows(connection.peer_identity().as_ref()) {
                    tracing::warn!(
                        configured_peer = %active_peer.label(),
                        peer = %connection.peer_addr(),
                        "input lane rejected because this is not the selected trusted peer"
                    );
                    return;
                }
                let connection: Arc<dyn nexkvm_network::Connection> = Arc::from(connection);
                let forwarder_lease = capture
                    .as_ref()
                    .and_then(|_| input_forwarder_gate.try_acquire());
                if capture.is_some() && forwarder_lease.is_none() {
                    tracing::warn!(
                        peer = %connection.peer_addr(),
                        "duplicate input connection rejected; closing its physical session"
                    );
                    let connection = Arc::clone(&connection);
                    task_supervisor.spawn(async move {
                        input_session::close_input_connection(&*connection).await;
                    });
                    return;
                }
                if let Some(injector) = injector.clone() {
                    let connection = Arc::clone(&connection);
                    let shutdown = task_supervisor.subscribe();
                    task_supervisor.spawn(async move {
                        if let Err(error) =
                            input_session::inject_until_shutdown(&*connection, &injector, shutdown)
                                .await
                        {
                            tracing::warn!(%error, "input injection session ended");
                        }
                    });
                }
                if let (Some(capture), Some(lease)) = (capture.clone(), forwarder_lease) {
                    let connection = Arc::clone(&connection);
                    let handoff_edges = handoff_edges.clone();
                    let shutdown = task_supervisor.subscribe();
                    task_supervisor.spawn(async move {
                    let _lease = lease;
                    let capture_for_suppression = capture.clone();
                    let forward = async move {
                        capture.discard_pending().await;
                        input_session::forward_reconfigurable_until_shutdown(
                            &capture,
                            &*connection,
                            MessageId(0),
                            handoff_edges,
                            shutdown,
                            config.forwarding,
                            move |suppressed| capture_for_suppression.set_suppressed(suppressed),
                        )
                        .await
                    };
                    tokio::select! {
                        result = forward => {
                            if let Err(error) = result {
                                handle_input_capture_end(error);
                            }
                        }
                        () = context.wait_for_session_end() => {
                            tracing::debug!("input capture released after physical session end");
                        }
                    }
                });
                }
            });
        Some(handler)
    }
    #[cfg(target_os = "windows")]
    {
        let injector = if config.plan.start_inject_receiver {
            Some(nexkvm_platform_windows::WindowsInputInjector::new())
        } else {
            None
        };
        let capture = if config.plan.start_capture_forwarder {
            Some(nexkvm_platform_windows::WindowsInputCapture::new())
        } else {
            None
        };
        let input_forwarder_gate = Arc::new(input_session::InputForwarderGate::default());
        let handler: connection::PeerConnectionHandler = Arc::new(
            move |connection, mut context| {
                if !active_peer.allows(connection.peer_identity().as_ref()) {
                    tracing::warn!(
                        configured_peer = %active_peer.label(),
                        peer = %connection.peer_addr(),
                        "input lane rejected because this is not the selected trusted peer"
                    );
                    return;
                }
                let connection: Arc<dyn nexkvm_network::Connection> = Arc::from(connection);
                let forwarder_lease = capture
                    .as_ref()
                    .and_then(|_| input_forwarder_gate.try_acquire());
                if capture.is_some() && forwarder_lease.is_none() {
                    tracing::warn!(
                        peer = %connection.peer_addr(),
                        "duplicate input connection rejected; closing its physical session"
                    );
                    let connection = Arc::clone(&connection);
                    task_supervisor.spawn(async move {
                        input_session::close_input_connection(&*connection).await;
                    });
                    return;
                }
                if let Some(injector) = injector.clone() {
                    let connection = Arc::clone(&connection);
                    let shutdown = task_supervisor.subscribe();
                    task_supervisor.spawn(async move {
                        if let Err(error) =
                            input_session::inject_until_shutdown(&*connection, &injector, shutdown)
                                .await
                        {
                            tracing::warn!(%error, "Windows input injection session ended");
                        }
                    });
                }
                if let (Some(capture), Some(lease)) = (capture.clone(), forwarder_lease) {
                    let connection = Arc::clone(&connection);
                    let handoff_edges = handoff_edges.clone();
                    let shutdown = task_supervisor.subscribe();
                    task_supervisor.spawn(async move {
                    let _lease = lease;
                    let capture_for_suppression = capture.clone();
                    let forward = input_session::forward_reconfigurable_until_shutdown(
                        &capture,
                        &*connection,
                        MessageId(0),
                        handoff_edges,
                        shutdown,
                        config.forwarding,
                        move |suppressed| capture_for_suppression.set_suppressed(suppressed),
                    );
                    tokio::select! {
                        result = forward => {
                            if let Err(error) = result {
                                handle_input_capture_end(error);
                            }
                        }
                        () = context.wait_for_session_end() => {
                            tracing::debug!("Windows input capture released after physical session end");
                        }
                    }
                });
                }
            },
        );
        Some(handler)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = (config, handoff_edges, active_peer, task_supervisor);
        None
    }
}

#[derive(Debug, Clone)]
enum ActivePeerSelection {
    AnyTrusted,
    Only(nexkvm_crypto::PublicKey),
    Unresolved(String),
}

impl ActivePeerSelection {
    fn allows(&self, peer: Option<&nexkvm_crypto::PublicKey>) -> bool {
        match (self, peer) {
            (Self::AnyTrusted, Some(_)) => true,
            (Self::Only(expected), Some(actual)) => expected == actual,
            (Self::AnyTrusted | Self::Only(_) | Self::Unresolved(_), None)
            | (Self::Unresolved(_), Some(_)) => false,
        }
    }

    fn label(&self) -> &str {
        match self {
            Self::AnyTrusted => "auto",
            Self::Only(_) => "selected trusted peer",
            Self::Unresolved(label) => label,
        }
    }
}

fn resolve_active_peer_selection(active_peer: Option<&str>) -> ActivePeerSelection {
    use nexkvm_storage::FileTrustStore;

    let Some(label) = active_peer.map(str::trim).filter(|label| !label.is_empty()) else {
        return ActivePeerSelection::AnyTrusted;
    };
    let Ok(store) = FileTrustStore::load(trust_path()) else {
        return ActivePeerSelection::Unresolved(label.into());
    };
    let entries = store.entries();
    resolve_active_peer_from_entries(label, &entries)
}

fn resolve_active_peer_from_entries(
    label: &str,
    entries: &[nexkvm_crypto::TrustEntry],
) -> ActivePeerSelection {
    if let Some(entry) = entries.iter().find(|entry| {
        entry.display_name.eq_ignore_ascii_case(label)
            || entry.public_key.fingerprint().eq_ignore_ascii_case(label)
    }) {
        return ActivePeerSelection::Only(entry.public_key.clone());
    }
    ActivePeerSelection::Unresolved(label.into())
}

fn merge_peer_handlers(
    local_identity: nexkvm_crypto::PublicKey,
    input: Option<connection::PeerConnectionHandler>,
    input_receives: bool,
    clipboard: Option<connection::PeerConnectionHandler>,
    file_transfer: Option<connection::PeerConnectionHandler>,
) -> Option<connection::PeerConnectionHandler> {
    let mut lanes = Vec::new();
    if let Some(input) = input {
        let inbound = if input_receives {
            vec![nexkvm_protocol::MessageKind::Input]
        } else {
            Vec::new()
        };
        lanes.push(peer_session::PeerLaneHandler::new(input, inbound));
    }
    if let Some(clipboard) = clipboard {
        lanes.push(peer_session::PeerLaneHandler::new(
            clipboard,
            [nexkvm_protocol::MessageKind::Clipboard],
        ));
    }
    if let Some(file_transfer) = file_transfer {
        lanes.push(peer_session::PeerLaneHandler::new(
            file_transfer,
            [nexkvm_protocol::MessageKind::FileTransfer],
        ));
    }
    peer_session::compose_peer_handler(local_identity, lanes)
}

fn clipboard_runtime_enabled(sync_enabled: bool, can_access_clipboard: bool) -> bool {
    sync_enabled && can_access_clipboard
}

fn create_clipboard_peer_handler(
    can_access_clipboard: bool,
    local_device_id: nexkvm_core::DeviceId,
    history: Option<clipboard_history::ClipboardHistoryRecorder>,
    active_peer: ActivePeerSelection,
) -> Option<connection::PeerConnectionHandler> {
    if !can_access_clipboard {
        return None;
    }
    #[cfg(target_os = "macos")]
    {
        let clipboard = Arc::new(nexkvm_platform_macos::MacosClipboard::new());
        let gate = Arc::new(clipboard_runtime::ClipboardPeerGate::default());

        let handler: connection::PeerConnectionHandler = Arc::new(move |connection, _context| {
            let Some(peer_identity) = connection.peer_identity() else {
                tracing::warn!(
                    peer = %connection.peer_addr(),
                    "clipboard lane rejected an unauthenticated connection"
                );
                tokio::spawn(async move {
                    let _ = connection.close().await;
                });
                return;
            };
            if !active_peer.allows(Some(&peer_identity)) {
                tracing::warn!(
                    configured_peer = %active_peer.label(),
                    peer = %connection.peer_addr(),
                    "clipboard lane rejected because this is not the selected trusted peer"
                );
                tokio::spawn(async move {
                    let _ = connection.close().await;
                });
                return;
            }
            let authenticated_peer = stable_device_id(&peer_identity);
            if authenticated_peer == local_device_id {
                tracing::warn!("clipboard lane rejected a self-identity connection");
                tokio::spawn(async move {
                    let _ = connection.close().await;
                });
                return;
            }
            let Some(lease) = gate.try_acquire(peer_identity) else {
                tracing::debug!(
                    peer = %connection.peer_addr(),
                    "clipboard lane already belongs to another authenticated connection"
                );
                tokio::spawn(async move {
                    let _ = connection.close().await;
                });
                return;
            };

            let connection: Arc<dyn nexkvm_network::Connection> = Arc::from(connection);
            let clipboard = Arc::clone(&clipboard);
            let history = history.clone();
            tokio::spawn(async move {
                let result = clipboard_runtime::run_peer_session(
                    clipboard,
                    Arc::clone(&connection),
                    local_device_id,
                    authenticated_peer,
                    history,
                )
                .await;
                if let Err(error) = result {
                    tracing::warn!(%error, "clipboard peer session ended");
                }
                if let Err(error) = connection.close().await
                    && !matches!(error, nexkvm_network::NetworkError::Closed)
                {
                    tracing::debug!(%error, "clipboard connection close failed");
                }
                drop(lease);
            });
        });
        Some(handler)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (local_device_id, history, active_peer);
        None
    }
}

fn start_clipboard_history_runtime(
    can_access_clipboard: bool,
    history: Option<clipboard_history::ClipboardHistoryRecorder>,
    local_device_id: nexkvm_core::DeviceId,
) -> Option<tokio::task::JoinHandle<()>> {
    if !can_access_clipboard {
        return None;
    }
    #[cfg(target_os = "macos")]
    {
        history.map(|history| {
            clipboard_history::spawn_local_history_poll(
                Arc::new(nexkvm_platform_macos::MacosClipboard::new()),
                history,
                local_device_id,
            )
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (history, local_device_id);
        None
    }
}

fn clipboard_history_list(json: bool) -> anyhow::Result<()> {
    let config_path = config_path();
    let config = Config::load(&config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;
    if !config.clipboard.history_enabled {
        if json {
            println!("[]");
        } else {
            println!("clipboard history is disabled");
        }
        return Ok(());
    }
    let path = clipboard_history::archive_path(&config_path);
    if !path.exists() {
        if json {
            println!("[]");
        } else {
            println!("clipboard history is empty");
        }
        return Ok(());
    }
    let archive = nexkvm_storage::ClipboardHistoryArchive::open(
        path,
        clipboard_history::archive_config(&config.clipboard),
    )
    .context("opening encrypted clipboard history")?;

    if json {
        let entries: Vec<_> = archive
            .entries()
            .map(|entry| {
                serde_json::json!({
                    "fingerprint": format!("{:016x}", entry.fingerprint().0),
                    "preview": clipboard_preview(&entry.snapshot, 240),
                    "origin": entry.origin.to_string(),
                    "at_millis": entry.at_millis,
                    "pinned": entry.pinned,
                    "bytes": entry.snapshot.total_len(),
                    "formats": entry.snapshot.formats().len(),
                })
            })
            .collect();
        println!("{}", serde_json::to_string(&entries)?);
        return Ok(());
    }

    let mut count = 0usize;
    for entry in archive.entries() {
        count += 1;
        println!(
            "{:016x}  {}  {} bytes  origin={}  at={}",
            entry.fingerprint().0,
            clipboard_preview(&entry.snapshot, 80),
            entry.snapshot.total_len(),
            entry.origin,
            entry.at_millis,
        );
    }
    if count == 0 {
        println!("clipboard history is empty");
    }
    Ok(())
}

async fn clipboard_history_restore(fingerprint: u64) -> anyhow::Result<()> {
    let config_path = config_path();
    let config = Config::load(&config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;
    anyhow::ensure!(
        config.clipboard.history_enabled,
        "clipboard history is disabled"
    );
    let path = clipboard_history::archive_path(&config_path);
    anyhow::ensure!(path.exists(), "clipboard history is empty");
    let archive = nexkvm_storage::ClipboardHistoryArchive::open(
        path,
        clipboard_history::archive_config(&config.clipboard),
    )
    .context("opening encrypted clipboard history")?;
    let snapshot = archive
        .entries()
        .find(|entry| entry.fingerprint().0 == fingerprint)
        .map(|entry| entry.snapshot.clone())
        .ok_or_else(|| anyhow::anyhow!("clipboard history entry was not found"))?;

    #[cfg(target_os = "macos")]
    {
        use nexkvm_clipboard::Clipboard;
        nexkvm_platform_macos::MacosClipboard::new()
            .write(snapshot)
            .await
            .context("restoring the macOS clipboard")?;
        println!("clipboard history entry restored");
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = snapshot;
        anyhow::bail!("clipboard history restore is currently available on macOS")
    }
}

fn clipboard_history_clear() -> anyhow::Result<()> {
    let config_path = config_path();
    let config = Config::load(&config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;
    anyhow::ensure!(
        config.clipboard.history_enabled,
        "clipboard history is disabled"
    );
    let path = clipboard_history::archive_path(&config_path);
    if !path.exists() {
        println!("clipboard history is already empty");
        return Ok(());
    }
    clipboard_history::clear_unpinned(&config_path, &config.clipboard)
        .context("clearing encrypted clipboard history")?;
    println!("unpinned clipboard history cleared");
    Ok(())
}

fn clipboard_preview(snapshot: &nexkvm_clipboard::ClipboardSnapshot, max_chars: usize) -> String {
    let text = snapshot.best_text().unwrap_or("[non-text clipboard item]");
    let mut preview: String = text
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(max_chars)
        .collect();
    if text.chars().count() > max_chars {
        preview.push('…');
    }
    preview
}

fn handle_input_capture_end(error: input_session::InputSessionError) {
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
    peer_handlers: Option<connection::PeerConnectionHandler>,
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
                peer_handlers.clone(),
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

    let addr = validate_pairing_address(addr)?;
    let config_path = config_path();
    let config = Config::load(&config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;

    let public_key = load_local_identity(&config_path, &config.device.name)?.public_key();
    let nonce = fresh_pairing_nonce()?;

    let bootstrap = PairingBootstrap::new(config.device.name, public_key, nonce, addr.to_string());
    println!("{}", bootstrap.to_uri()?);
    Ok(())
}

fn validate_pairing_address(addr: &str) -> anyhow::Result<SocketAddr> {
    const REQUIREMENT: &str =
        "pairing address must be a reachable non-loopback unicast IP:port for this Mac";

    let addr: SocketAddr = addr
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!(REQUIREMENT))?;
    let ip = addr.ip();
    let is_broadcast = match ip {
        std::net::IpAddr::V4(ip) => ip.is_broadcast(),
        std::net::IpAddr::V6(_) => false,
    };
    anyhow::ensure!(
        addr.port() != 0
            && !ip.is_loopback()
            && !ip.is_unspecified()
            && !ip.is_multicast()
            && !is_broadcast,
        REQUIREMENT
    );
    Ok(addr)
}

fn unsupported_transport_warning(configured: &[String]) -> Option<String> {
    let mut unsupported = configured
        .iter()
        .map(|transport| transport.trim())
        .filter(|transport| {
            !transport.is_empty() && !transport.eq_ignore_ascii_case(EFFECTIVE_TRANSPORT)
        })
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();
    unsupported.sort_unstable();
    unsupported.dedup();
    (!unsupported.is_empty()).then(|| {
        format!(
            "unsupported configured transports ignored: {}; effective transport: {EFFECTIVE_TRANSPORT}",
            unsupported.join(",")
        )
    })
}

fn fresh_pairing_nonce() -> anyhow::Result<[u8; nexkvm_crypto::NONCE_LEN]> {
    let mut nonce = [0u8; nexkvm_crypto::NONCE_LEN];
    getrandom::fill(&mut nonce).map_err(|error| {
        anyhow::anyhow!("generating a cryptographically secure pairing nonce: {error}")
    })?;
    Ok(nonce)
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

fn stable_device_id(public_key: &nexkvm_crypto::PublicKey) -> nexkvm_core::DeviceId {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(b"nexkvm stable device id v1");
    hasher.update(public_key.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    // RFC 9562 variant plus UUIDv8 (application-defined) version bits.
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    nexkvm_core::DeviceId(uuid::Uuid::from_bytes(bytes))
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
    println!("  effective transport: {EFFECTIVE_TRANSPORT}");
    if let Some(warning) = unsupported_transport_warning(&config.network.transports) {
        println!("  {warning}");
    }
    println!("  pairing policy: required (authenticated trusted peers only)");
    for line in cli::format_input_alpha_runtime(
        storage_input_role_label(config.input.control_role),
        config.input.active_peer.as_deref(),
        storage_input_edge_label(config.input.handoff_edge),
        config.input.emergency_stop_keycode,
        config.input.remote_focus_timeout_millis,
        config.network.connect_addr.as_deref(),
        config.clipboard.sync_enabled,
    )
    .lines()
    {
        println!("  {line}");
    }
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

async fn portal_smoke() -> anyhow::Result<()> {
    println!("nexkvm portal-smoke");
    #[cfg(target_os = "linux")]
    {
        use anyhow::Context as _;
        use nexkvm_platform_linux::{
            PortalInputGrant, ReisPortalEisEventDecoder, WaylandPortalInputClient,
            XdgDesktopPortalInputClient, ZbusXdgDesktopPortalInputTransport,
        };
        use tokio::time::{Duration, timeout};

        println!("  transport: xdg-desktop-portal session bus");
        let transport = ZbusXdgDesktopPortalInputTransport::session()
            .await
            .context("connect xdg-desktop-portal session bus")?;
        let client = XdgDesktopPortalInputClient::with_event_decoder(
            transport,
            ReisPortalEisEventDecoder::default(),
        );

        println!("  grant: requesting RemoteDesktop + InputCapture");
        let grant = client
            .request_input_session(PortalInputGrant {
                remote_desktop: true,
                input_capture: true,
            })
            .await
            .context("request portal input session")?;
        println!(
            "  grant: remote_desktop={} input_capture={}",
            grant.remote_desktop, grant.input_capture
        );

        let zones = client
            .configure_first_zone_right_edge_barrier()
            .await
            .context("configure first-zone right-edge pointer barrier")?;
        println!(
            "  zones: {} zone(s), zone_set={}",
            zones.zones.len(),
            zones.id
        );
        if let Some(zone) = zones.zones.first() {
            println!(
                "  barrier: first zone right edge x={} y={}..{}",
                zone.x + i32::try_from(zone.width.saturating_sub(1)).unwrap_or(i32::MAX),
                zone.y,
                zone.y + i32::try_from(zone.height.saturating_sub(1)).unwrap_or(i32::MAX)
            );
        }

        println!("  event: waiting up to 10s for an EIS input event");
        match timeout(Duration::from_secs(10), client.next_event()).await {
            Ok(Ok(event)) => println!("  event: {event:?}"),
            Ok(Err(error)) => anyhow::bail!("portal EIS event failed: {error}"),
            Err(_) => anyhow::bail!(
                "timed out waiting for EIS input event; move the pointer through the configured edge barrier"
            ),
        }
        println!("  status: ok");
    }
    #[cfg(not(target_os = "linux"))]
    {
        println!("  status: unavailable");
        println!("  reason: Linux Wayland portal smoke is only available on Linux targets");
    }
    Ok(())
}

async fn pipewire_smoke() -> anyhow::Result<()> {
    println!("nexkvm pipewire-smoke");
    #[cfg(target_os = "linux")]
    {
        use anyhow::Context as _;
        use nexkvm_core::DeviceId;
        use nexkvm_platform_linux::{
            LinuxPipeWireScreenCapture, NativePipeWireFrameReader,
            ZbusXdgDesktopPortalScreenCastTransport,
        };
        use nexkvm_streaming::{
            GpuMemoryKind, HardwareEncoder, ScreenCaptureBackend, ScreenCodec, ScreenResolution,
            ScreenStreamIntent, ScreenStreamPlan,
        };
        use tokio::time::{Duration, timeout};

        println!("  transport: xdg-desktop-portal ScreenCast session bus");
        let transport = ZbusXdgDesktopPortalScreenCastTransport::session()
            .await
            .context("connect xdg-desktop-portal session bus")?;
        let backend =
            LinuxPipeWireScreenCapture::with_frame_reader(transport, NativePipeWireFrameReader);

        println!("  grant: requesting ScreenCast portal session");
        let permissions = backend
            .request_permissions()
            .await
            .context("request ScreenCast portal permissions")?;
        println!(
            "  grant: display_capture={} window_capture={} pending={}",
            permissions.display_capture, permissions.window_capture, permissions.permission_pending
        );

        let sources = backend
            .list_sources()
            .await
            .context("list PipeWire ScreenCast sources")?;
        println!("  sources: {} source(s)", sources.len());
        let source = sources
            .first()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("ScreenCast portal returned no sources"))?;
        println!("  source: {}", source_label(&source));

        let resolution = source_resolution(&source).unwrap_or(ScreenResolution::new(1920, 1080));
        println!(
            "  frame: requesting one {}x{} System-memory raw frame",
            resolution.width, resolution.height
        );
        let plan = ScreenStreamPlan {
            from: DeviceId::generate(),
            to: DeviceId::generate(),
            source,
            intent: ScreenStreamIntent::MiniRemotePreview,
            codec: ScreenCodec::RawRgba,
            encoder: HardwareEncoder::Software,
            memory: GpuMemoryKind::System,
            resolution,
            fps: 30,
            bitrate_kbps: 0,
            zero_copy: false,
            requires_encrypted_transport: false,
        };
        let frame = timeout(Duration::from_secs(10), backend.capture_frame(&plan))
            .await
            .context("timed out waiting for first PipeWire frame")?
            .context("capture first PipeWire frame")?;
        println!(
            "  frame: sequence={} resolution={}x{} pixel_format={:?} memory={:?} bytes={}",
            frame.sequence,
            frame.resolution.width,
            frame.resolution.height,
            frame.pixel_format,
            frame.memory,
            frame.payload.len()
        );
        println!("  status: ok");
    }
    #[cfg(not(target_os = "linux"))]
    {
        println!("  status: unavailable");
        println!("  reason: Linux PipeWire ScreenCast smoke is only available on Linux targets");
    }
    Ok(())
}

async fn audio_smoke(action: Option<AudioSmokeAction>) -> anyhow::Result<()> {
    println!("nexkvm audio-smoke");
    #[cfg(target_os = "linux")]
    {
        use anyhow::Context as _;
        use nexkvm_platform_linux::{
            NativePipeWireAudioGraph, NativePipeWireAudioStream, PipeWireAudioBackend,
        };
        use nexkvm_streaming::{
            AudioBackend, AudioCodec, AudioDeviceId, AudioDeviceRole, AudioFormat,
            AudioStreamBackend, route_audio_frame_once,
        };

        println!("  graph: PipeWire user-session registry");
        let stream_format = AudioFormat {
            codec: AudioCodec::Pcm,
            ..AudioFormat::default()
        };
        let backend = PipeWireAudioBackend::with_stream_format(
            NativePipeWireAudioGraph,
            NativePipeWireAudioStream,
            stream_format,
        );
        println!(
            "  stream-format: rate={} channels={} sample={:?} codec={:?} frame_ms={}",
            stream_format.sample_rate_hz,
            stream_format.channels,
            stream_format.sample_format,
            stream_format.codec,
            stream_format.frame_duration_ms
        );
        let devices = backend
            .devices()
            .await
            .context("enumerate PipeWire audio devices")?;
        println!("  devices: {} device(s)", devices.len());
        for device in &devices {
            println!("  device: {}", audio_device_label(device));
        }

        match action {
            None => {}
            Some(AudioSmokeAction::SetDefault(target)) => {
                let device = find_audio_device(&devices, &target)?;
                ensure_audio_role(
                    device,
                    &[AudioDeviceRole::Playback, AudioDeviceRole::Duplex],
                )?;
                backend
                    .switch_playback_device(&AudioDeviceId::new(target.clone()))
                    .await
                    .context("set default PipeWire playback device")?;
                println!("  set-default: {target}");
            }
            Some(AudioSmokeAction::CaptureFrame(target)) => {
                let device = find_audio_device(&devices, &target)?;
                ensure_audio_role(device, &[AudioDeviceRole::Capture, AudioDeviceRole::Duplex])?;
                println!("  capture-frame: waiting for one frame from {target}");
                let frame = backend
                    .capture_audio_frame(&AudioDeviceId::new(target.clone()))
                    .await
                    .context("capture one PipeWire audio frame")?;
                println!(
                    "  frame: sequence={} samples_per_channel={} codec={:?} bytes={}",
                    frame.sequence,
                    frame.samples_per_channel,
                    frame.codec,
                    frame.payload.len()
                );
            }
            Some(AudioSmokeAction::Loopback { source, sink }) => {
                let source_device = find_audio_device(&devices, &source)?;
                ensure_audio_role(
                    source_device,
                    &[AudioDeviceRole::Capture, AudioDeviceRole::Duplex],
                )?;
                let sink_device = find_audio_device(&devices, &sink)?;
                ensure_audio_role(
                    sink_device,
                    &[AudioDeviceRole::Playback, AudioDeviceRole::Duplex],
                )?;
                println!("  loopback: {source} -> {sink}");
                let frame = route_audio_frame_once(
                    &backend,
                    &AudioDeviceId::new(source.clone()),
                    &AudioDeviceId::new(sink.clone()),
                )
                .await
                .context("route one PipeWire audio frame")?;
                println!(
                    "  frame: sequence={} samples_per_channel={} codec={:?} bytes={}",
                    frame.sequence,
                    frame.samples_per_channel,
                    frame.codec,
                    frame.payload.len()
                );
            }
        }

        println!("  status: ok");
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = action;
        println!("  status: unavailable");
        println!("  reason: Linux PipeWire audio smoke is only available on Linux targets");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn find_audio_device<'a>(
    devices: &'a [nexkvm_streaming::AudioDevice],
    target: &str,
) -> anyhow::Result<&'a nexkvm_streaming::AudioDevice> {
    devices
        .iter()
        .find(|device| device.id.0 == target)
        .ok_or_else(|| anyhow::anyhow!("audio device `{target}` was not enumerated"))
}

#[cfg(target_os = "linux")]
fn ensure_audio_role(
    device: &nexkvm_streaming::AudioDevice,
    allowed: &[nexkvm_streaming::AudioDeviceRole],
) -> anyhow::Result<()> {
    if allowed.contains(&device.role) {
        Ok(())
    } else {
        anyhow::bail!(
            "audio device `{}` has role {:?}, expected one of {:?}",
            device.id.0,
            device.role,
            allowed
        )
    }
}

#[cfg(target_os = "linux")]
fn audio_device_label(device: &nexkvm_streaming::AudioDevice) -> String {
    format!(
        "{} role={} default={} label={}",
        device.id.0,
        audio_role_label(device.role),
        device.is_default,
        device.label
    )
}

#[cfg(target_os = "linux")]
fn audio_role_label(role: nexkvm_streaming::AudioDeviceRole) -> &'static str {
    match role {
        nexkvm_streaming::AudioDeviceRole::Capture => "capture",
        nexkvm_streaming::AudioDeviceRole::Playback => "playback",
        nexkvm_streaming::AudioDeviceRole::Duplex => "duplex",
    }
}

#[cfg(target_os = "linux")]
fn source_label(source: &nexkvm_streaming::CaptureSource) -> &str {
    match source {
        nexkvm_streaming::CaptureSource::Display { label, .. } => label,
        nexkvm_streaming::CaptureSource::Window { title, .. } => title,
        nexkvm_streaming::CaptureSource::Application { name, .. } => name,
    }
}

#[cfg(target_os = "linux")]
fn source_resolution(
    source: &nexkvm_streaming::CaptureSource,
) -> Option<nexkvm_streaming::ScreenResolution> {
    let id = match source {
        nexkvm_streaming::CaptureSource::Display { id, .. }
        | nexkvm_streaming::CaptureSource::Window { id, .. }
        | nexkvm_streaming::CaptureSource::Application { id, .. } => id.0.as_str(),
    };
    let (_, dims) = id.rsplit_once('@')?;
    let (width, height) = dims.split_once('x')?;
    Some(nexkvm_streaming::ScreenResolution::new(
        width.parse().ok()?,
        height.parse().ok()?,
    ))
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

    if let Some(address) = device.address.as_deref()
        && address.parse::<SocketAddr>().is_err()
    {
        return SimConnectionPlanEntry {
            device,
            kind: SimConnectionPlanKind::InvalidConfiguration,
            detail: format!("invalid address `{address}` (expected ip:port)"),
        };
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
        "    - latency: smoothed={smoothed}ms jitter={estimated_jitter}ms timeout={timeout}ms"
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

fn build_simulation_report_json(
    config: &SimConfig,
    plans: &[SimConnectionPlanEntry<'_>],
    runtime_devices: &[SimRuntimeDevice],
) -> serde_json::Value {
    use serde_json::json;

    let devices = config
        .device
        .iter()
        .map(|device| {
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
            json!({
                "id": id,
                "display_name": display_name,
                "name": device.name,
                "os": device.os,
                "address": address,
                "trust": trust_state,
                "role": device.role,
                "x": device.x,
                "y": device.y,
                "width": device.width,
                "height": device.height,
            })
        })
        .collect::<Vec<_>>();

    let connection_planning = plans
        .iter()
        .map(|plan| {
            let display_name = plan
                .device
                .display_name
                .as_deref()
                .unwrap_or(plan.device.name.as_str());
            json!({
                "device": display_name,
                "kind": plan.kind.label(),
                "detail": plan.detail,
            })
        })
        .collect::<Vec<_>>();

    let discovery_json = {
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

        let ranked_count = tracker.ranked(now_millis).len();
        let best_active = tracker
            .best_active(now_millis)
            .and_then(|id| runtime_devices.iter().find(|device| device.id == id))
            .map(|device| device.name.clone())
            .unwrap_or_else(|| "none".to_string());
        json!({
            "ranked_count": ranked_count,
            "best_active": best_active,
        })
    };

    let latency_json = {
        use nexkvm_network::RttTracker;

        let mut tracker = RttTracker::default();
        let base = config.network.rtt_ms.max(1);
        let jitter = config.network.jitter_ms;
        let low = base.saturating_sub(jitter).max(1);
        let high = base.saturating_add(jitter).max(1);
        tracker.record(std::time::Duration::from_millis(low));
        tracker.record(std::time::Duration::from_millis(base));
        tracker.record(std::time::Duration::from_millis(high));
        json!({
            "smoothed_ms": tracker.smoothed().unwrap_or_default().as_millis(),
            "jitter_ms": tracker.jitter().unwrap_or_default().as_millis(),
            "timeout_ms": tracker.timeout(std::time::Duration::from_millis(250)).as_millis(),
        })
    };

    let workspace_json = {
        use nexkvm_core::{
            SnapDirection, UnifiedVirtualDesktop, WindowId, WindowSnapshot, WorkspaceDevice,
            plan_window_snap,
        };

        if runtime_devices.is_empty() {
            json!({ "status": "unavailable", "reason": "no-devices" })
        } else {
            let mut desktop = UnifiedVirtualDesktop::new();
            for device in runtime_devices {
                desktop.upsert(
                    WorkspaceDevice::new(device.id, device.name.clone(), device.bounds)
                        .with_online(true),
                );
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
                    json!({
                        "status": "ok",
                        "snap_right_target": target,
                        "cross_device": plan.cross_device,
                    })
                }
                Err(error) => json!({
                    "status": "unavailable",
                    "reason": error.to_string(),
                }),
            }
        }
    };

    let screen_json = {
        use nexkvm_streaming::{
            CaptureSource, CaptureSourceId, ScreenStreamCapabilities, ScreenStreamRequest,
            negotiate_screen_stream,
        };

        if runtime_devices.len() < 2 {
            json!({ "status": "unavailable", "reason": "need-at-least-2-devices" })
        } else {
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
                Ok(plan) => json!({
                    "status": "ok",
                    "codec": format!("{:?}", plan.codec),
                    "fps": plan.fps,
                    "resolution": {
                        "width": plan.resolution.width,
                        "height": plan.resolution.height,
                    },
                    "encrypted": plan.requires_encrypted_transport,
                }),
                Err(error) => json!({
                    "status": "unavailable",
                    "reason": error.to_string(),
                }),
            }
        }
    };

    let collaboration_json = {
        use nexkvm_core::{
            CollaborationMode, CollaborationParticipant, CollaborationSession, ParticipantRole,
        };

        if runtime_devices.len() < 2 {
            json!({ "status": "unavailable", "reason": "need-at-least-2-devices" })
        } else {
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
                let _ = session.grant_control(
                    host_id,
                    peer_id,
                    runtime_devices[0].id,
                    1_000,
                    Some(30_000),
                )?;
                Ok(())
            })();

            match result {
                Ok(()) => json!({
                    "status": "ok",
                    "participants": session.participants().len(),
                    "pending_requests": session.pending_requests().len(),
                    "control_active": session.can_control(peer_id, runtime_devices[0].id, 1_001),
                }),
                Err(error) => json!({
                    "status": "unavailable",
                    "reason": error.to_string(),
                }),
            }
        }
    };

    json!({
        "network": {
            "profile": config.network.profile,
            "rtt_ms": config.network.rtt_ms,
            "jitter_ms": config.network.jitter_ms,
            "loss": config.network.loss,
            "throughput_bps": config.network.throughput_bps,
        },
        "devices": devices,
        "connection_planning": connection_planning,
        "features": {
            "clipboard": config.features.clipboard,
            "file_transfer": config.features.file_transfer,
            "screen_preview": config.features.screen_preview,
            "shared_cursor": config.features.shared_cursor,
            "plugins": config.features.plugins,
        },
        "simulators": {
            "discovery": discovery_json,
            "latency": latency_json,
            "workspace": workspace_json,
            "screen": screen_json,
            "collaboration": collaboration_json,
        },
    })
}

fn simulate(path: Option<String>, json_only: bool) -> anyhow::Result<()> {
    let path = path.unwrap_or_else(|| "tools/sim/local-workspace.toml".into());
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading simulation config from {path}"))?;
    let config: SimConfig = toml::from_str(&text)
        .with_context(|| format!("parsing simulation config TOML from {path}"))?;
    validate_sim_config(&config)?;

    let runtime_devices = build_runtime_devices(&config);
    let plans = config
        .device
        .iter()
        .map(build_connection_plan)
        .collect::<Vec<_>>();
    let machine_report = build_simulation_report_json(&config, &plans, &runtime_devices);

    if json_only {
        println!("{machine_report}");
        return Ok(());
    }

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
    for device in &config.device {
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
    print_simulator_report(&runtime_devices, &config.network);
    println!("  simulation_report_json: {machine_report}");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_runtime_requires_config_and_platform_access() {
        assert!(!clipboard_runtime_enabled(false, false));
        assert!(!clipboard_runtime_enabled(false, true));
        assert!(!clipboard_runtime_enabled(true, false));
        assert!(clipboard_runtime_enabled(true, true));
    }

    #[test]
    fn stable_device_id_is_bound_to_the_persisted_identity_key() {
        let first = nexkvm_crypto::DeviceKeypair::from_seed([1; 32]).public_key();
        let same = nexkvm_crypto::DeviceKeypair::from_seed([1; 32]).public_key();
        let other = nexkvm_crypto::DeviceKeypair::from_seed([2; 32]).public_key();

        assert_eq!(stable_device_id(&first), stable_device_id(&same));
        assert_ne!(stable_device_id(&first), stable_device_id(&other));
    }

    #[test]
    fn configured_active_peer_rejects_other_authenticated_connections() {
        let selected = nexkvm_crypto::DeviceKeypair::from_seed([1; 32]).public_key();
        let other = nexkvm_crypto::DeviceKeypair::from_seed([2; 32]).public_key();
        let policy = ActivePeerSelection::Only(selected.clone());

        assert!(policy.allows(Some(&selected)));
        assert!(!policy.allows(Some(&other)));
        assert!(!policy.allows(None));
        assert!(!ActivePeerSelection::Unresolved("missing".into()).allows(Some(&selected)));
    }

    #[test]
    fn unmatched_configured_peer_never_falls_back_to_the_only_trusted_entry() {
        let only = nexkvm_crypto::TrustEntry {
            display_name: "studio-mac".into(),
            public_key: nexkvm_crypto::DeviceKeypair::from_seed([7; 32]).public_key(),
            paired_at: 1,
        };
        let selection = resolve_active_peer_from_entries("different-device", &[only]);
        assert!(
            matches!(selection, ActivePeerSelection::Unresolved(label) if label == "different-device")
        );
    }

    #[test]
    fn pairing_nonces_are_fresh_csprng_values() {
        let first = fresh_pairing_nonce().unwrap();
        let second = fresh_pairing_nonce().unwrap();
        assert_ne!(first, second);
        assert!(first.iter().any(|byte| *byte != 0));
    }

    #[test]
    fn graceful_shutdown_plan_includes_gui_sigterm_on_unix() {
        assert!(configured_shutdown_signals().contains(&ShutdownSignal::Interrupt));
        #[cfg(unix)]
        assert!(configured_shutdown_signals().contains(&ShutdownSignal::Terminate));
    }
}
