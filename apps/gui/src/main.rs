use std::collections::VecDeque;
use std::ffi::OsString;
use std::fmt;
use std::fs::OpenOptions;
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

use eframe::egui;
use nexkvm_storage::{Config, InputControlRole, InputHandoffEdge};
use serde::Deserialize;

const MAX_GUI_HISTORY_ENTRIES: usize = 4_096;
const MAX_GUI_HISTORY_JSON_BYTES: usize = 4 * 1024 * 1024;
const MAX_GUI_HISTORY_PREVIEW_CHARS: usize = 240;
const MAX_DROPPED_PATHS: usize = 1_024;
const MAX_PENDING_FILE_SENDS: usize = 4;
const MIN_HISTORY_CAPACITY: usize = 1;
const MAX_HISTORY_CAPACITY: usize = 4_096;
const MIN_HISTORY_ENTRY_BYTES: usize = 1_024;
const MAX_HISTORY_ENTRY_BYTES: usize = 64 * 1024 * 1024;
const MIN_TRANSFER_BYTES: u64 = 1024 * 1024;
const MAX_TRANSFER_BYTES: u64 = 1024 * 1024 * 1024 * 1024;
const MAX_DOWNLOAD_DIR_CHARS: usize = 4_096;
const MAX_DAEMON_LOG_BYTES: u64 = 8 * 1024 * 1024;
const MAX_GUI_COMMAND_ERROR_BYTES: usize = 4 * 1024;
const MAX_GUI_COMMAND_ERROR_CHARS: usize = 512;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1120.0, 740.0]),
        ..Default::default()
    };
    eframe::run_native(
        "NexKVM",
        options,
        Box::new(|_cc| Ok(Box::new(NexkvmGui::load()))),
    )
}

#[derive(Debug)]
struct NexkvmGui {
    config_path: PathBuf,
    config: Config,
    config_load_error: Option<String>,
    section: Section,
    status: String,
    daemon: Option<Child>,
    runtime_restart_required: bool,
    daemon_log_path: PathBuf,
    pairing_addr: String,
    pairing_uri: String,
    accept_uri: String,
    pairing_accept_armed_uri: Option<String>,
    command_output: String,
    clipboard_history: Vec<ClipboardHistoryEntry>,
    clipboard_history_loaded: bool,
    clipboard_clear_armed: bool,
    pending_file_sends: Vec<PendingFileSend>,
    notifications: VecDeque<GuiNotification>,
    next_notification_id: u64,
}

#[derive(Clone, PartialEq, Eq)]
struct ClipboardHistoryEntry {
    fingerprint: String,
    preview: String,
    origin: String,
    at_millis: u64,
    pinned: bool,
    bytes: u64,
    formats: u64,
}

impl fmt::Debug for ClipboardHistoryEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClipboardHistoryEntry")
            .field("fingerprint", &self.fingerprint)
            .field("preview", &"[redacted]")
            .field("origin", &self.origin)
            .field("at_millis", &self.at_millis)
            .field("pinned", &self.pinned)
            .field("bytes", &self.bytes)
            .field("formats", &self.formats)
            .finish()
    }
}

#[derive(Deserialize)]
struct ClipboardHistoryWireEntry {
    fingerprint: String,
    #[serde(default)]
    preview: String,
    #[serde(default)]
    origin: String,
    #[serde(default)]
    at_millis: u64,
    #[serde(default)]
    pinned: bool,
    #[serde(default)]
    bytes: u64,
    #[serde(default)]
    formats: u64,
}

#[derive(Debug)]
struct PendingFileSend {
    child: Child,
    file_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PairingGenerationFeedback {
    uri: Option<String>,
    urgency: NotificationUrgency,
    title: &'static str,
    detail: String,
}

impl NexkvmGui {
    fn load() -> Self {
        Self::load_from(config_path())
    }

    fn load_from(config_path: PathBuf) -> Self {
        let (config, config_load_error) = match Config::load(&config_path) {
            Ok(config) => (config, None),
            Err(error) => (
                Config::default(),
                Some(format!(
                    "Failed to load {}: {error}. Saving is disabled until the file is repaired or removed and the GUI is restarted.",
                    config_path.display()
                )),
            ),
        };
        let daemon_log_path = daemon_log_path(&config_path);
        let mut notifications = initial_notifications();
        let mut next_notification_id = 1;
        if let Some(error) = &config_load_error {
            record_notification(
                &mut notifications,
                &mut next_notification_id,
                NotificationUrgency::High,
                "Configuration load failed",
                error,
            );
        }
        Self {
            config_path,
            config,
            config_load_error: config_load_error.clone(),
            section: Section::Overview,
            status: config_load_error.unwrap_or_else(|| "Ready".into()),
            daemon: None,
            runtime_restart_required: false,
            daemon_log_path,
            pairing_addr: local_pairing_addr(),
            pairing_uri: String::new(),
            accept_uri: String::new(),
            pairing_accept_armed_uri: None,
            command_output: String::new(),
            clipboard_history: Vec::new(),
            clipboard_history_loaded: false,
            clipboard_clear_armed: false,
            pending_file_sends: Vec::new(),
            notifications,
            next_notification_id,
        }
    }

    fn save_config(&mut self) {
        if let Some(error) = self.config_load_error.clone() {
            self.set_status(NotificationUrgency::High, "Settings save blocked", error);
            return;
        }
        match self.config.save(&self.config_path) {
            Ok(()) => self.apply_runtime_change("Settings were saved"),
            Err(error) => self.set_status(
                NotificationUrgency::High,
                "Settings save failed",
                format!("Save failed: {error}"),
            ),
        }
    }

    fn apply_runtime_change(&mut self, reason: &str) {
        match runtime_apply_action(self.daemon.is_some()) {
            RuntimeApplyAction::RestartOwned => self.restart_owned_daemon(reason),
            RuntimeApplyAction::RestartRequired => {
                self.runtime_restart_required = true;
                self.set_status(
                    NotificationUrgency::Normal,
                    "Daemon restart required",
                    format!(
                        "{reason}. Start or restart the daemon before relying on the new runtime settings."
                    ),
                );
            }
        }
    }

    fn restart_owned_daemon(&mut self, reason: &str) {
        let Some(mut child) = self.daemon.take() else {
            self.apply_runtime_change(reason);
            return;
        };
        if let Err(error) = terminate_owned_daemon(&mut child) {
            self.daemon = Some(child);
            self.runtime_restart_required = true;
            self.set_status(
                NotificationUrgency::High,
                "Daemon restart failed",
                format!("{reason}, but the GUI-owned daemon could not stop: {error}"),
            );
            return;
        }

        self.start_daemon();
        if self.daemon.is_some() {
            self.runtime_restart_required = false;
            self.set_status(
                NotificationUrgency::Normal,
                "Daemon restarted",
                format!("{reason}; the GUI-owned daemon restarted with the new runtime state"),
            );
        }
    }

    fn persist_config(&mut self) -> bool {
        if let Some(error) = self.config_load_error.clone() {
            self.set_status(NotificationUrgency::High, "Settings save blocked", error);
            return false;
        }
        match self.config.save(&self.config_path) {
            Ok(()) => true,
            Err(error) => {
                self.set_status(
                    NotificationUrgency::High,
                    "Settings save failed",
                    format!("Save failed: {error}"),
                );
                false
            }
        }
    }

    fn run_command(&mut self, args: &[&str]) -> bool {
        match Command::new(nexkvm_binary()).args(args).output() {
            Ok(output) => {
                let success = output.status.success();
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                self.command_output = if stderr.trim().is_empty() {
                    stdout.to_string()
                } else {
                    format!("{stdout}\n{stderr}")
                };
                self.set_status(
                    if success {
                        NotificationUrgency::Normal
                    } else {
                        NotificationUrgency::High
                    },
                    "Command finished",
                    format!("Command exited: {}", output.status),
                );
                success
            }
            Err(error) => {
                self.set_status(
                    NotificationUrgency::High,
                    "Command failed",
                    format!("Command failed: {error}"),
                );
                false
            }
        }
    }

    fn save_sharing_config(&mut self) {
        if let Err(error) = prepare_sharing_config(&mut self.config) {
            self.set_status(NotificationUrgency::High, "Sharing settings invalid", error);
            return;
        }
        self.save_config();
    }

    fn refresh_clipboard_history(&mut self) {
        match Command::new(nexkvm_binary())
            .args(["clipboard-history", "--json"])
            .output()
        {
            Ok(output) if output.status.success() => {
                match parse_clipboard_history_json(&output.stdout) {
                    Ok(entries) => {
                        let count = entries.len();
                        self.clipboard_history = entries;
                        self.clipboard_history_loaded = true;
                        self.clipboard_clear_armed = false;
                        self.set_status(
                            NotificationUrgency::Normal,
                            "Clipboard history refreshed",
                            format!("Loaded {count} clipboard history entries"),
                        );
                    }
                    Err(_) => self.set_status(
                        NotificationUrgency::High,
                        "Clipboard history unavailable",
                        "The clipboard-history command returned malformed JSON",
                    ),
                }
            }
            Ok(output) => self.set_status(
                NotificationUrgency::High,
                "Clipboard history failed",
                format!("Clipboard history command exited: {}", output.status),
            ),
            Err(error) => self.set_status(
                NotificationUrgency::High,
                "Clipboard history failed",
                format!("Could not start clipboard history command: {error}"),
            ),
        }
    }

    fn restore_clipboard_entry(&mut self, fingerprint: &str) {
        let args = match clipboard_restore_args(fingerprint) {
            Ok(args) => args,
            Err(error) => {
                self.set_status(
                    NotificationUrgency::High,
                    "Clipboard restore rejected",
                    error,
                );
                return;
            }
        };

        match Command::new(nexkvm_binary()).args(args).output() {
            Ok(output) if output.status.success() => self.set_status(
                NotificationUrgency::Normal,
                "Clipboard restored",
                "The selected clipboard history entry is active",
            ),
            Ok(output) => self.set_status(
                NotificationUrgency::High,
                "Clipboard restore failed",
                format!("Clipboard restore command exited: {}", output.status),
            ),
            Err(error) => self.set_status(
                NotificationUrgency::High,
                "Clipboard restore failed",
                format!("Could not start clipboard restore command: {error}"),
            ),
        }
    }

    fn clear_clipboard_history(&mut self) {
        match Command::new(nexkvm_binary())
            .arg("clipboard-clear")
            .output()
        {
            Ok(output) if output.status.success() => {
                self.clipboard_history.retain(|entry| entry.pinned);
                self.clipboard_history_loaded = true;
                self.clipboard_clear_armed = false;
                self.set_status(
                    NotificationUrgency::Normal,
                    "Clipboard history cleared",
                    "Unpinned clipboard history entries were cleared",
                );
            }
            Ok(output) => {
                self.clipboard_clear_armed = false;
                self.set_status(
                    NotificationUrgency::High,
                    "Clipboard clear failed",
                    format!("Clipboard clear command exited: {}", output.status),
                );
            }
            Err(error) => {
                self.clipboard_clear_armed = false;
                self.set_status(
                    NotificationUrgency::High,
                    "Clipboard clear failed",
                    format!("Could not start clipboard clear command: {error}"),
                );
            }
        }
    }

    fn handle_dropped_files(&mut self, dropped_files: Vec<egui::DroppedFile>) {
        if self.section != Section::Sharing {
            self.set_status(
                NotificationUrgency::Low,
                "File drop ignored",
                "Open Sharing before dropping local files",
            );
            return;
        }
        if !self.config.file_transfer.enabled {
            self.set_status(
                NotificationUrgency::High,
                "File transfer disabled",
                "Enable file transfer and save the Sharing settings first",
            );
            return;
        }
        if self.pending_file_sends.len() >= MAX_PENDING_FILE_SENDS {
            self.set_status(
                NotificationUrgency::High,
                "File transfer busy",
                "Wait for an active file transfer command to finish",
            );
            return;
        }

        let args = match file_send_args(dropped_files.into_iter().map(|file| file.path)) {
            Ok(args) => args,
            Err(error) => {
                self.set_status(NotificationUrgency::High, "File drop rejected", error);
                return;
            }
        };
        let file_count = args.len().saturating_sub(2);
        match Command::new(nexkvm_binary())
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => {
                self.pending_file_sends
                    .push(PendingFileSend { child, file_count });
                self.set_status(
                    NotificationUrgency::Normal,
                    "Preparing file transfer",
                    format!("Validating and queueing {file_count} local path(s)"),
                );
            }
            Err(error) => self.set_status(
                NotificationUrgency::High,
                "File transfer failed",
                format!("Could not start file transfer command: {error}"),
            ),
        }
    }

    fn refresh_pending_file_sends(&mut self) {
        let mut completions = Vec::new();
        for index in (0..self.pending_file_sends.len()).rev() {
            match self.pending_file_sends[index].child.try_wait() {
                Ok(None) => {}
                Ok(Some(status)) => {
                    let pending = self.pending_file_sends.remove(index);
                    completions.push((pending.file_count, Ok(status)));
                }
                Err(error) => {
                    let mut pending = self.pending_file_sends.remove(index);
                    let _ = pending.child.kill();
                    let _ = pending.child.wait();
                    completions.push((pending.file_count, Err(error)));
                }
            }
        }

        for (file_count, result) in completions {
            match result {
                Ok(status) if status.success() => self.set_status(
                    NotificationUrgency::Normal,
                    "File transfer queued",
                    format!("Queued {file_count} local path(s) for the selected peer"),
                ),
                Ok(status) => self.set_status(
                    NotificationUrgency::High,
                    "File transfer failed",
                    format!("File transfer command exited: {status}"),
                ),
                Err(error) => self.set_status(
                    NotificationUrgency::High,
                    "File transfer failed",
                    format!("Could not monitor file transfer command: {error}"),
                ),
            }
        }
    }

    fn start_daemon(&mut self) {
        if self.daemon.is_some() {
            self.set_status(
                NotificationUrgency::Low,
                "Daemon already running",
                "Daemon already started from this window",
            );
            return;
        }
        if !self.persist_config() {
            return;
        }
        let listen_port = self.config.network.listen_port;
        if !daemon_listen_port_available(listen_port) {
            self.set_status(
                NotificationUrgency::Normal,
                "Daemon already running",
                format!(
                    "Port {listen_port} is already in use; NexKVM will not terminate an unowned process"
                ),
            );
            return;
        }
        let log_file = match open_daemon_log(&self.daemon_log_path) {
            Ok(file) => file,
            Err(error) => {
                self.set_status(
                    NotificationUrgency::High,
                    "Daemon log failed",
                    format!("Failed to open daemon log: {error}"),
                );
                return;
            }
        };
        let stderr_log = match log_file.try_clone() {
            Ok(file) => file,
            Err(error) => {
                self.set_status(
                    NotificationUrgency::High,
                    "Daemon log failed",
                    format!("Failed to open daemon log: {error}"),
                );
                return;
            }
        };
        match Command::new(nexkvm_binary())
            .stdout(Stdio::from(log_file))
            .stderr(Stdio::from(stderr_log))
            .spawn()
        {
            Ok(child) => {
                self.daemon = Some(child);
                self.runtime_restart_required = false;
                self.set_status(
                    NotificationUrgency::Normal,
                    "Daemon started",
                    format!("Daemon started; log {}", self.daemon_log_path.display()),
                );
            }
            Err(error) => self.set_status(
                NotificationUrgency::High,
                "Daemon start failed",
                format!("Failed to start daemon: {error}"),
            ),
        }
    }

    fn stop_daemon(&mut self) {
        if let Some(mut child) = self.daemon.take() {
            match terminate_owned_daemon(&mut child) {
                Ok(()) => self.set_status(
                    NotificationUrgency::Normal,
                    "Daemon stopped",
                    "Daemon stopped gracefully",
                ),
                Err(error) => self.set_status(
                    NotificationUrgency::High,
                    "Daemon stop failed",
                    format!("Could not stop this window's daemon: {error}"),
                ),
            }
            return;
        }
        self.set_status(
            NotificationUrgency::Low,
            "Daemon not owned",
            "No daemon started by this window; no process was terminated",
        );
    }

    fn set_status(
        &mut self,
        urgency: NotificationUrgency,
        title: impl Into<String>,
        body: impl Into<String>,
    ) {
        let body = body.into();
        self.status = body.clone();
        record_notification(
            &mut self.notifications,
            &mut self.next_notification_id,
            urgency,
            title,
            body,
        );
    }
}

impl Drop for NexkvmGui {
    fn drop(&mut self) {
        if let Some(mut child) = self.daemon.take() {
            let _ = terminate_owned_daemon(&mut child);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Overview,
    Settings,
    Sharing,
    Pairing,
    Notifications,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NotificationUrgency {
    Low,
    Normal,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeApplyAction {
    RestartOwned,
    RestartRequired,
}

fn runtime_apply_action(daemon_owned_by_gui: bool) -> RuntimeApplyAction {
    if daemon_owned_by_gui {
        RuntimeApplyAction::RestartOwned
    } else {
        RuntimeApplyAction::RestartRequired
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GuiNotification {
    id: u64,
    urgency: NotificationUrgency,
    title: String,
    body: String,
}

impl eframe::App for NexkvmGui {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        apply_theme(ctx);
        self.refresh_daemon_state();
        self.refresh_pending_file_sends();
        if !self.pending_file_sends.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        let dropped_files = ctx.input(|input| input.raw.dropped_files.clone());
        if !dropped_files.is_empty() {
            self.handle_dropped_files(dropped_files);
        }

        egui::SidePanel::left("side")
            .resizable(false)
            .exact_width(248.0)
            .frame(egui::Frame::default().fill(surface_dark()))
            .show(ctx, |ui| {
                ui.add_space(18.0);
                ui.horizontal(|ui| {
                    app_mark(ui);
                    ui.vertical(|ui| {
                        ui.label(
                            egui::RichText::new("NexKVM")
                                .size(24.0)
                                .strong()
                                .color(egui::Color32::WHITE),
                        );
                        ui.label(muted_text("Cross-platform control"));
                    });
                });

                ui.add_space(24.0);
                nav_item(ui, &mut self.section, Section::Overview, "Overview");
                nav_item(ui, &mut self.section, Section::Settings, "Settings");
                nav_item(ui, &mut self.section, Section::Sharing, "Sharing");
                nav_item(ui, &mut self.section, Section::Pairing, "Pairing & Output");
                nav_item(
                    ui,
                    &mut self.section,
                    Section::Notifications,
                    "Notifications",
                );

                ui.add_space(24.0);
                status_panel(ui, self.daemon.is_some(), &self.status);

                ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                    ui.add_space(16.0);
                    ui.label(muted_text("Config path"));
                    ui.label(
                        egui::RichText::new(self.config_path.display().to_string())
                            .size(11.0)
                            .color(egui::Color32::from_rgb(188, 197, 210)),
                    );
                });
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(22.0);
            page_header(ui, self.section, &self.config.device.name);
            ui.add_space(16.0);
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| match self.section {
                    Section::Overview => self.overview_page(ui),
                    Section::Settings => self.settings_page(ui),
                    Section::Sharing => self.sharing_page(ui),
                    Section::Pairing => self.pairing_page(ui),
                    Section::Notifications => self.notifications_page(ui),
                });
        });
    }
}

impl NexkvmGui {
    fn refresh_daemon_state(&mut self) {
        let exit_status = self
            .daemon
            .as_mut()
            .and_then(|child| child.try_wait().ok().flatten());
        if let Some(status) = exit_status {
            self.daemon = None;
            self.set_status(
                NotificationUrgency::High,
                "Daemon exited",
                format!("Daemon exited: {status}"),
            );
            self.command_output = read_log_tail(&self.daemon_log_path, 16_000);
            self.section = Section::Pairing;
        }
    }

    fn overview_page(&mut self, ui: &mut egui::Ui) {
        card(ui, ui.available_width() - 8.0, |ui| {
            let narrow = ui.available_width() < 620.0;
            if narrow {
                ui.vertical(|ui| {
                    ui.label(
                        egui::RichText::new("Unified Desktop")
                            .size(22.0)
                            .strong()
                            .color(egui::Color32::WHITE),
                    );
                    ui.label(muted_text(
                        "One keyboard and mouse across trusted machines, with edge-based handoff.",
                    ));
                });
                ui.add_space(10.0);
                ui.horizontal_wrapped(|ui| {
                    status_chip(
                        ui,
                        role_label(self.config.input.control_role),
                        accent_blue(),
                    );
                    status_chip(ui, "Paired only", egui::Color32::from_rgb(73, 137, 94));
                });
            } else {
                ui.horizontal_top(|ui| {
                    ui.vertical(|ui| {
                        ui.label(
                            egui::RichText::new("Unified Desktop")
                                .size(22.0)
                                .strong()
                                .color(egui::Color32::WHITE),
                        );
                        ui.label(muted_text(
                            "One keyboard and mouse across trusted machines, with edge-based handoff.",
                        ));
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                        status_chip(
                            ui,
                            role_label(self.config.input.control_role),
                            accent_blue(),
                        );
                        status_chip(
                            ui,
                            "Paired only",
                            egui::Color32::from_rgb(73, 137, 94),
                        );
                    });
                });
            }
        });

        ui.add_space(14.0);
        responsive_cards(ui, 3, 280.0, |ui, index, card_width| match index {
            0 => card(ui, card_width, |ui| {
                card_title(ui, "Daemon");
                ui.add_space(6.0);
                metric_row(
                    ui,
                    "State",
                    if self.daemon.is_some() {
                        "Running from GUI"
                    } else {
                        "Not started here"
                    },
                );
                metric_row(ui, "Mode", role_label(self.config.input.control_role));
                metric_row(ui, "Port", &self.config.network.listen_port.to_string());
                metric_row(
                    ui,
                    "Connect to",
                    connect_addr_label(&self.config.network.connect_addr),
                );
                ui.add_space(12.0);
                ui.horizontal_wrapped(|ui| {
                    if primary_button(ui, "Start").clicked() {
                        self.start_daemon();
                    }
                    if danger_button(ui, "Stop now").clicked() {
                        self.stop_daemon();
                    }
                });
            }),
            1 => card(ui, card_width, |ui| {
                card_title(ui, "Connection");
                ui.add_space(6.0);
                metric_row(
                    ui,
                    "Discovery",
                    if self.config.network.enable_discovery {
                        "Enabled"
                    } else {
                        "Disabled"
                    },
                );
                metric_row(ui, "Pairing", "Required");
                metric_row(
                    ui,
                    "Active peer",
                    active_peer_label(&self.config.input.active_peer),
                );
                metric_row(
                    ui,
                    "Peer address",
                    connect_addr_label(&self.config.network.connect_addr),
                );
                metric_row(ui, "Handoff", edge_label(self.config.input.handoff_edge));
            }),
            2 => card(ui, card_width, |ui| {
                card_title(ui, "Events");
                ui.add_space(6.0);
                metric_row(
                    ui,
                    "Unread",
                    &self
                        .notifications
                        .iter()
                        .filter(|notification| notification.urgency != NotificationUrgency::Low)
                        .count()
                        .to_string(),
                );
                if let Some(notification) = self.notifications.front() {
                    metric_row(ui, "Latest", &notification.title);
                    metric_row(ui, "Level", urgency_label(notification.urgency));
                } else {
                    metric_row(ui, "Latest", "None");
                    metric_row(ui, "Level", "Low");
                }
                ui.add_space(12.0);
                ui.horizontal_wrapped(|ui| {
                    if secondary_button(ui, "Doctor").clicked() {
                        self.run_command(&["doctor"]);
                        self.section = Section::Pairing;
                    }
                    if secondary_button(ui, "Permissions").clicked() {
                        self.run_command(&["permissions"]);
                        self.section = Section::Pairing;
                    }
                    if secondary_button(ui, "View all").clicked() {
                        self.section = Section::Notifications;
                    }
                });
            }),
            _ => {}
        });

        ui.add_space(16.0);
        card(ui, ui.available_width() - 8.0, |ui| {
            card_title(ui, "Desktop layout");
            ui.add_space(10.0);
            let _ = layout_preview(
                ui,
                self.config.input.handoff_edge,
                self.config.input.control_role,
                active_peer_label(&self.config.input.active_peer),
                false,
            );
            ui.add_space(10.0);
            ui.label(muted_text(
                "Move across the configured edge to hand control to the paired target.",
            ));
        });

        ui.add_space(16.0);
        card(ui, ui.available_width() - 8.0, |ui| {
            card_title(ui, "Safety");
            ui.add_space(6.0);
            metric_row(
                ui,
                "Emergency key",
                emergency_label(self.config.input.emergency_stop_keycode).as_str(),
            );
            metric_row(
                ui,
                "Focus timeout",
                &format!("{} ms", self.config.input.remote_focus_timeout_millis),
            );
        });
    }

    fn settings_page(&mut self, ui: &mut egui::Ui) {
        card(ui, ui.available_width() - 8.0, |ui| {
            card_title(ui, "Device");
            ui.add_space(8.0);
            labeled_text(ui, "Name", &mut self.config.device.name);
            labeled_text_option(ui, "Active peer", &mut self.config.input.active_peer);
        });

        ui.add_space(14.0);
        responsive_cards(ui, 2, 340.0, |ui, index, card_width| match index {
            0 => card(ui, card_width, |ui| {
                card_title(ui, "Input sharing");
                ui.add_space(8.0);
                ui.label(field_label("Role"));
                segmented_role_selector(ui, &mut self.config.input.control_role);
                ui.add_space(10.0);
                ui.label(field_label("Handoff edge"));
                segmented_edge_selector(ui, &mut self.config.input.handoff_edge);
                ui.add_space(8.0);
                ui.label(muted_text(
                    "Drag the target screen toward the side where it sits.",
                ));
                let active_peer = active_peer_label(&self.config.input.active_peer).to_owned();
                if let Some(edge) = layout_preview(
                    ui,
                    self.config.input.handoff_edge,
                    self.config.input.control_role,
                    &active_peer,
                    true,
                ) {
                    self.config.input.handoff_edge = edge;
                }
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.label(field_label("Emergency key"));
                    ui.add(
                        egui::DragValue::new(&mut self.config.input.emergency_stop_keycode)
                            .speed(1)
                            .range(0..=255),
                    );
                    ui.label(muted_text(emergency_label(
                        self.config.input.emergency_stop_keycode,
                    )));
                });
                ui.horizontal(|ui| {
                    ui.label(field_label("Remote timeout"));
                    ui.add(
                        egui::DragValue::new(&mut self.config.input.remote_focus_timeout_millis)
                            .speed(100)
                            .suffix(" ms"),
                    );
                });
            }),
            1 => card(ui, card_width, |ui| {
                card_title(ui, "Network & security");
                ui.add_space(8.0);
                ui.checkbox(
                    &mut self.config.network.enable_discovery,
                    "Advertise on LAN",
                );
                ui.label(muted_text(
                    "Authenticated pairing is always required; only pinned peers may reconnect.",
                ));
                ui.horizontal(|ui| {
                    ui.label(field_label("Listen port"));
                    ui.add(egui::DragValue::new(&mut self.config.network.listen_port));
                });
                labeled_text_option(ui, "Connect address", &mut self.config.network.connect_addr);
            }),
            _ => {}
        });

        ui.add_space(14.0);
        ui.horizontal_wrapped(|ui| {
            if primary_button(ui, "Save & apply").clicked() {
                self.save_config();
            }
            if secondary_button(ui, "Start with these settings").clicked() {
                self.start_daemon();
            }
        });
        ui.add_space(6.0);
        ui.label(muted_text(
            "A GUI-owned daemon restarts after save; an external daemon must be restarted manually. Edge-only changes also hot-reload.",
        ));
        if self.runtime_restart_required {
            ui.colored_label(
                egui::Color32::from_rgb(240, 182, 84),
                "Runtime restart required before these settings take effect.",
            );
        }
    }

    fn sharing_page(&mut self, ui: &mut egui::Ui) {
        responsive_cards(ui, 2, 390.0, |ui, index, card_width| match index {
            0 => card(ui, card_width, |ui| {
                card_title(ui, "Clipboard sharing");
                ui.add_space(8.0);
                ui.checkbox(
                    &mut self.config.clipboard.sync_enabled,
                    "Sync clipboard with trusted peers",
                );
                ui.checkbox(
                    &mut self.config.clipboard.history_enabled,
                    "Keep encrypted clipboard history",
                );
                ui.add_space(8.0);
                let history_enabled = self.config.clipboard.history_enabled;
                ui.add_enabled_ui(history_enabled, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(field_label("History capacity"));
                        ui.add(
                            egui::DragValue::new(&mut self.config.clipboard.history_capacity)
                                .speed(1)
                                .range(MIN_HISTORY_CAPACITY..=MAX_HISTORY_CAPACITY),
                        );
                        ui.label(muted_text("entries"));
                    });
                    ui.horizontal_wrapped(|ui| {
                        ui.label(field_label("Max entry size"));
                        ui.add(
                            egui::DragValue::new(
                                &mut self.config.clipboard.history_max_entry_bytes,
                            )
                            .speed(1024)
                            .range(MIN_HISTORY_ENTRY_BYTES..=MAX_HISTORY_ENTRY_BYTES)
                            .suffix(" bytes"),
                        );
                    });
                });
                ui.add_space(8.0);
                ui.label(muted_text(
                    "History is encrypted on disk. Previews are shown only in this page and are not copied into GUI notifications.",
                ));
            }),
            1 => card(ui, card_width, |ui| {
                card_title(ui, "File transfer");
                ui.add_space(8.0);
                ui.checkbox(
                    &mut self.config.file_transfer.enabled,
                    "Allow files from trusted peers",
                );
                let file_transfer_enabled = self.config.file_transfer.enabled;
                ui.add_enabled_ui(file_transfer_enabled, |ui| {
                    ui.add_space(8.0);
                    labeled_text_option_limited(
                        ui,
                        "Download directory",
                        &mut self.config.file_transfer.download_dir,
                        MAX_DOWNLOAD_DIR_CHARS,
                    );
                    ui.horizontal_wrapped(|ui| {
                        ui.label(field_label("Max transfer size"));
                        ui.add(
                            egui::DragValue::new(&mut self.config.file_transfer.max_transfer_bytes)
                                .speed(1024 * 1024)
                                .range(MIN_TRANSFER_BYTES..=MAX_TRANSFER_BYTES)
                                .suffix(" bytes"),
                        );
                    });
                });
                ui.add_space(8.0);
                ui.label(muted_text(
                    "An empty download directory uses the platform Downloads folder.",
                ));
            }),
            _ => {}
        });

        ui.add_space(14.0);
        ui.horizontal_wrapped(|ui| {
            if primary_button(ui, "Save sharing settings").clicked() {
                self.save_sharing_config();
            }
        });
        if self.runtime_restart_required {
            ui.colored_label(
                egui::Color32::from_rgb(240, 182, 84),
                "Start or restart the daemon before clipboard/file changes take effect.",
            );
        }

        ui.add_space(14.0);
        let hovered_files = ui.ctx().input(|input| input.raw.hovered_files.len());
        card(ui, ui.available_width() - 8.0, |ui| {
            card_title(ui, "Send files");
            ui.add_space(8.0);
            if hovered_files > 0 {
                ui.colored_label(
                    egui::Color32::from_rgb(91, 199, 132),
                    format!("Release to send {hovered_files} local item(s)"),
                );
            } else {
                ui.label(
                    egui::RichText::new("Drop local files or folders here")
                        .size(19.0)
                        .strong()
                        .color(egui::Color32::WHITE),
                );
            }
            ui.add_space(6.0);
            ui.label(muted_text(
                "Web links and drops without a local filesystem path are rejected.",
            ));
            if !self.pending_file_sends.is_empty() {
                ui.add_space(8.0);
                ui.label(muted_text(format!(
                    "{} file transfer command(s) active",
                    self.pending_file_sends.len()
                )));
            }
        });

        ui.add_space(14.0);
        let mut refresh_requested = false;
        let mut clear_requested = false;
        card(ui, ui.available_width() - 8.0, |ui| {
            ui.horizontal_wrapped(|ui| {
                card_title(ui, "Clipboard history");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.clipboard_clear_armed {
                        if danger_button(ui, "Confirm clear unpinned").clicked() {
                            clear_requested = true;
                        }
                        if secondary_button(ui, "Cancel clear").clicked() {
                            self.clipboard_clear_armed = false;
                        }
                    } else if danger_button(ui, "Clear unpinned").clicked() {
                        self.clipboard_clear_armed = true;
                    }
                    if secondary_button(ui, "Refresh").clicked() {
                        refresh_requested = true;
                    }
                });
            });
            ui.add_space(10.0);
            if !self.clipboard_history_loaded {
                ui.label(muted_text("Refresh to load encrypted clipboard history."));
            } else if self.clipboard_history.is_empty() {
                ui.label(muted_text("Clipboard history is empty."));
            }
        });
        if refresh_requested {
            self.refresh_clipboard_history();
        }
        if clear_requested {
            self.clear_clipboard_history();
        }

        if !self.clipboard_history.is_empty() {
            ui.add_space(10.0);
            let mut restore_request = None;
            card(ui, ui.available_width() - 8.0, |ui| {
                for entry in &self.clipboard_history {
                    if clipboard_history_row(ui, entry) {
                        restore_request = Some(entry.fingerprint.clone());
                    }
                    ui.add_space(8.0);
                }
            });
            if let Some(fingerprint) = restore_request {
                self.restore_clipboard_entry(&fingerprint);
            }
        }
    }

    fn pairing_page(&mut self, ui: &mut egui::Ui) {
        responsive_cards(ui, 2, 390.0, |ui, index, card_width| match index {
            0 => card(ui, card_width, |ui| {
                card_title(ui, "Generate pairing URI");
                ui.add_space(8.0);
                labeled_text(ui, "This Mac's reachable address", &mut self.pairing_addr);
                if primary_button(ui, "Generate").clicked() {
                    match Command::new(nexkvm_binary())
                        .args(["pairing-uri", self.pairing_addr.as_str()])
                        .output()
                    {
                        Ok(output) => {
                            let feedback = pairing_generation_feedback(
                                output.status.success(),
                                &output.stdout,
                                &output.stderr,
                            );
                            self.pairing_uri = feedback.uri.unwrap_or_default();
                            if feedback.urgency == NotificationUrgency::High {
                                self.command_output = feedback.detail.clone();
                            }
                            self.set_status(feedback.urgency, feedback.title, feedback.detail);
                        }
                        Err(error) => self.set_status(
                            NotificationUrgency::High,
                            "Pairing URI failed",
                            format!("Pairing URI failed: {error}"),
                        ),
                    }
                }
                ui.add_space(8.0);
                ui.add(
                    egui::TextEdit::multiline(&mut self.pairing_uri)
                        .desired_rows(5)
                        .hint_text("Generated URI appears here"),
                );
            }),
            1 => card(ui, card_width, |ui| {
                card_title(ui, "Accept pairing");
                ui.add_space(8.0);
                ui.label(muted_text(
                    "Paste the peer URI, then accept it into the trusted device store.",
                ));
                let response = ui.add(
                    egui::TextEdit::multiline(&mut self.accept_uri)
                        .desired_rows(5)
                        .hint_text("nexkvm://pair/v1/..."),
                );
                if response.changed() {
                    self.pairing_accept_armed_uri = None;
                }
                let uri = self.accept_uri.trim().to_owned();
                if pairing_accept_ready(&uri, self.pairing_accept_armed_uri.as_deref()) {
                    ui.label(muted_text(
                        "Compare the fingerprint shown below with the peer, then confirm.",
                    ));
                    if danger_button(ui, "Confirm verified fingerprint").clicked()
                        && self.run_command(&["pair", "--accept", uri.as_str()])
                    {
                        self.pairing_accept_armed_uri = None;
                        self.apply_runtime_change("The trusted peer store changed");
                    }
                } else if primary_button(ui, "Inspect fingerprint").clicked()
                    && !uri.is_empty()
                    && self.run_command(&["pair", uri.as_str()])
                {
                    self.pairing_accept_armed_uri = Some(uri);
                }
            }),
            _ => {}
        });

        ui.add_space(14.0);
        card(ui, ui.available_width() - 8.0, |ui| {
            ui.horizontal(|ui| {
                card_title(ui, "Command output");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if secondary_button(ui, "Clear").clicked() {
                        self.command_output.clear();
                    }
                });
            });
            ui.add_space(8.0);
            ui.add(
                egui::TextEdit::multiline(&mut self.command_output)
                    .desired_rows(14)
                    .code_editor()
                    .hint_text("doctor, permissions and pairing output appears here"),
            );
        });
    }

    fn notifications_page(&mut self, ui: &mut egui::Ui) {
        card(ui, ui.available_width() - 8.0, |ui| {
            ui.horizontal_wrapped(|ui| {
                card_title(ui, "Notification center");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if secondary_button(ui, "Clear").clicked() {
                        self.notifications.clear();
                        self.set_status(
                            NotificationUrgency::Low,
                            "Notifications cleared",
                            "Notification center cleared",
                        );
                    }
                });
            });
            ui.add_space(10.0);
            if self.notifications.is_empty() {
                ui.label(muted_text("No recent runtime events."));
            } else {
                for notification in &self.notifications {
                    notification_row(ui, notification);
                    ui.add_space(8.0);
                }
            }
        });
    }
}

fn apply_theme(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.window_fill = bg_dark();
    visuals.panel_fill = bg_dark();
    visuals.faint_bg_color = egui::Color32::from_rgb(32, 38, 48);
    visuals.extreme_bg_color = egui::Color32::from_rgb(14, 18, 24);
    visuals.selection.bg_fill = accent_blue();
    ctx.set_visuals(visuals);
}

fn page_header(ui: &mut egui::Ui, section: Section, device_name: &str) {
    let (title, subtitle) = match section {
        Section::Overview => (
            "Overview",
            "Connection state, quick actions and desktop layout.",
        ),
        Section::Settings => (
            "Settings",
            "Runtime, safety, network and security configuration.",
        ),
        Section::Sharing => (
            "Sharing",
            "Clipboard history and authenticated local file transfer.",
        ),
        Section::Pairing => (
            "Pairing & Output",
            "Pair devices and inspect command results.",
        ),
        Section::Notifications => (
            "Notifications",
            "Runtime events, command results and peer-readiness signals.",
        ),
    };
    if ui.available_width() < 520.0 {
        ui.vertical(|ui| {
            ui.label(
                egui::RichText::new(title)
                    .size(28.0)
                    .strong()
                    .color(egui::Color32::WHITE),
            );
            ui.label(muted_text(subtitle));
            ui.add_space(8.0);
            pill(ui, device_name);
        });
    } else {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    egui::RichText::new(title)
                        .size(28.0)
                        .strong()
                        .color(egui::Color32::WHITE),
                );
                ui.label(muted_text(subtitle));
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                pill(ui, device_name);
            });
        });
    }
}

fn nav_item(ui: &mut egui::Ui, section: &mut Section, target: Section, label: &str) {
    let selected = *section == target;
    let button = egui::Button::new(egui::RichText::new(label).size(15.0).color(if selected {
        egui::Color32::WHITE
    } else {
        egui::Color32::from_rgb(190, 199, 212)
    }))
    .fill(if selected {
        accent_blue()
    } else {
        surface_dark()
    })
    .min_size(egui::vec2(ui.available_width(), 38.0));
    if ui.add(button).clicked() {
        *section = target;
    }
    ui.add_space(6.0);
}

fn status_panel(ui: &mut egui::Ui, running: bool, status: &str) {
    egui::Frame::default()
        .fill(egui::Color32::from_rgb(23, 29, 39))
        .inner_margin(12.0)
        .show(ui, |ui| {
            ui.label(muted_text("Daemon"));
            ui.horizontal(|ui| {
                let dot = if running {
                    egui::Color32::from_rgb(91, 199, 132)
                } else {
                    egui::Color32::from_rgb(238, 184, 82)
                };
                ui.colored_label(dot, if running { "ON" } else { "IDLE" });
                ui.label(if running { "Running" } else { "Idle" });
            });
            ui.add_space(8.0);
            ui.label(muted_text(status));
        });
}

fn status_chip(ui: &mut egui::Ui, text: &str, color: egui::Color32) {
    egui::Frame::default()
        .fill(color)
        .inner_margin(egui::Margin::symmetric(10.0, 6.0))
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new(text)
                    .size(12.0)
                    .strong()
                    .color(egui::Color32::WHITE),
            );
        });
}

const MAX_GUI_NOTIFICATIONS: usize = 24;

fn initial_notifications() -> VecDeque<GuiNotification> {
    let mut notifications = VecDeque::new();
    let mut next_id = 0;
    record_notification(
        &mut notifications,
        &mut next_id,
        NotificationUrgency::Low,
        "GUI ready",
        "NexKVM control panel is ready",
    );
    notifications
}

fn record_notification(
    notifications: &mut VecDeque<GuiNotification>,
    next_id: &mut u64,
    urgency: NotificationUrgency,
    title: impl Into<String>,
    body: impl Into<String>,
) {
    notifications.push_front(GuiNotification {
        id: *next_id,
        urgency,
        title: title.into(),
        body: body.into(),
    });
    *next_id = next_id.saturating_add(1);
    while notifications.len() > MAX_GUI_NOTIFICATIONS {
        notifications.pop_back();
    }
}

fn notification_row(ui: &mut egui::Ui, notification: &GuiNotification) {
    egui::Frame::default()
        .fill(egui::Color32::from_rgb(23, 29, 39))
        .stroke(egui::Stroke::new(
            1.0,
            urgency_color(notification.urgency).linear_multiply(0.75),
        ))
        .inner_margin(12.0)
        .show(ui, |ui| {
            ui.horizontal_top(|ui| {
                ui.colored_label(urgency_color(notification.urgency), "●");
                ui.vertical(|ui| {
                    ui.label(
                        egui::RichText::new(&notification.title)
                            .strong()
                            .color(egui::Color32::WHITE),
                    );
                    ui.label(muted_text(notification.body.as_str()));
                });
            });
        });
}

fn urgency_label(urgency: NotificationUrgency) -> &'static str {
    match urgency {
        NotificationUrgency::Low => "Low",
        NotificationUrgency::Normal => "Normal",
        NotificationUrgency::High => "High",
    }
}

fn urgency_color(urgency: NotificationUrgency) -> egui::Color32 {
    match urgency {
        NotificationUrgency::Low => egui::Color32::from_rgb(151, 162, 178),
        NotificationUrgency::Normal => egui::Color32::from_rgb(91, 199, 132),
        NotificationUrgency::High => egui::Color32::from_rgb(238, 184, 82),
    }
}

fn app_mark(ui: &mut egui::Ui) {
    egui::Frame::default()
        .fill(accent_blue())
        .inner_margin(9.0)
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new("N")
                    .size(20.0)
                    .strong()
                    .color(egui::Color32::WHITE),
            );
        });
}

fn card(ui: &mut egui::Ui, width: f32, add_contents: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::default()
        .fill(card_bg())
        .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(47, 57, 72)))
        .inner_margin(16.0)
        .show(ui, |ui| {
            ui.set_width(width.max(220.0));
            add_contents(ui);
        });
}

fn responsive_cards(
    ui: &mut egui::Ui,
    count: usize,
    preferred_width: f32,
    mut add_card: impl FnMut(&mut egui::Ui, usize, f32),
) {
    let gap = ui.spacing().item_spacing.x;
    let available = ui.available_width().max(240.0);
    let columns = ((available + gap) / (preferred_width + gap))
        .floor()
        .max(1.0)
        .min(count as f32) as usize;
    let card_width =
        ((available - gap * (columns.saturating_sub(1) as f32)) / columns as f32).max(220.0);

    for row_start in (0..count).step_by(columns) {
        ui.horizontal_top(|ui| {
            for index in row_start..(row_start + columns).min(count) {
                add_card(ui, index, card_width);
            }
        });
        if row_start + columns < count {
            ui.add_space(12.0);
        }
    }
}

fn card_title(ui: &mut egui::Ui, text: &str) {
    ui.label(
        egui::RichText::new(text)
            .size(17.0)
            .strong()
            .color(egui::Color32::WHITE),
    );
}

fn metric_row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.set_min_height(24.0);
        let label_width = ui.available_width().min(118.0);
        ui.add_sized([label_width, 20.0], egui::Label::new(field_label(label)));
        let value_width = ui.available_width().max(64.0);
        let value = truncate_for_width(value, value_width, 7.0);
        ui.add_sized(
            [value_width, 20.0],
            egui::Label::new(
                egui::RichText::new(value).color(egui::Color32::from_rgb(235, 239, 246)),
            ),
        );
    });
}

fn labeled_text(ui: &mut egui::Ui, label: &str, value: &mut String) {
    ui.horizontal_wrapped(|ui| {
        ui.label(field_label(label));
        let width = (ui.available_width() - 8.0).clamp(180.0, 360.0);
        ui.add_sized(
            [width, 28.0],
            egui::TextEdit::singleline(value).hint_text(label),
        );
    });
}

fn labeled_text_option(ui: &mut egui::Ui, label: &str, value: &mut Option<String>) {
    let text = value.get_or_insert_with(String::new);
    labeled_text(ui, label, text);
    if text.trim().is_empty() {
        *value = None;
    }
}

fn labeled_text_option_limited(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut Option<String>,
    max_chars: usize,
) {
    let empty = {
        let text = value.get_or_insert_with(String::new);
        ui.horizontal_wrapped(|ui| {
            ui.label(field_label(label));
            let width = (ui.available_width() - 8.0).clamp(180.0, 360.0);
            ui.add_sized(
                [width, 28.0],
                egui::TextEdit::singleline(text)
                    .hint_text(label)
                    .char_limit(max_chars),
            );
        });
        text.trim().is_empty()
    };
    if empty {
        *value = None;
    }
}

fn clipboard_history_row(ui: &mut egui::Ui, entry: &ClipboardHistoryEntry) -> bool {
    let mut restore_requested = false;
    egui::Frame::default()
        .fill(egui::Color32::from_rgb(23, 29, 39))
        .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(47, 57, 72)))
        .inner_margin(12.0)
        .show(ui, |ui| {
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    let preview = if entry.preview.is_empty() {
                        "[non-text clipboard item]"
                    } else {
                        entry.preview.as_str()
                    };
                    ui.label(
                        egui::RichText::new(preview)
                            .size(14.0)
                            .color(egui::Color32::WHITE),
                    );
                    ui.add_space(4.0);
                    ui.label(muted_text(format!(
                        "{} · {} · {} format(s) · at {}{}",
                        entry.fingerprint,
                        human_bytes(entry.bytes),
                        entry.formats,
                        entry.at_millis,
                        if entry.pinned { " · pinned" } else { "" }
                    )));
                    if !entry.origin.is_empty() {
                        ui.label(muted_text(format!("Origin: {}", entry.origin)));
                    }
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                    if secondary_button(ui, "Restore").clicked() {
                        restore_requested = true;
                    }
                });
            });
        });
    restore_requested
}

fn human_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

fn primary_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.add(
        egui::Button::new(
            egui::RichText::new(label)
                .color(egui::Color32::WHITE)
                .strong(),
        )
        .fill(accent_blue())
        .min_size(egui::vec2(96.0, 34.0)),
    )
}

fn secondary_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.add(
        egui::Button::new(egui::RichText::new(label).color(egui::Color32::from_rgb(232, 236, 244)))
            .fill(egui::Color32::from_rgb(43, 52, 66))
            .min_size(egui::vec2(96.0, 34.0)),
    )
}

fn danger_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.add(
        egui::Button::new(
            egui::RichText::new(label)
                .color(egui::Color32::WHITE)
                .strong(),
        )
        .fill(egui::Color32::from_rgb(180, 62, 72))
        .min_size(egui::vec2(96.0, 34.0)),
    )
}

fn pill(ui: &mut egui::Ui, text: &str) {
    egui::Frame::default()
        .fill(egui::Color32::from_rgb(34, 43, 57))
        .inner_margin(8.0)
        .show(ui, |ui| {
            ui.label(egui::RichText::new(text).color(egui::Color32::from_rgb(232, 236, 244)));
        });
}

fn field_label(label: &str) -> egui::RichText {
    egui::RichText::new(label)
        .size(13.0)
        .color(egui::Color32::from_rgb(166, 176, 192))
}

fn muted_text(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text.into())
        .size(12.0)
        .color(egui::Color32::from_rgb(151, 162, 178))
}

fn role_label(role: InputControlRole) -> &'static str {
    match role {
        InputControlRole::Disabled => "Disabled",
        InputControlRole::Source => "Source",
        InputControlRole::Target => "Target",
        InputControlRole::Both => "Source + Target",
    }
}

fn edge_label(edge: InputHandoffEdge) -> &'static str {
    match edge {
        InputHandoffEdge::Left => "Left edge",
        InputHandoffEdge::Right => "Right edge",
        InputHandoffEdge::Top => "Top edge",
        InputHandoffEdge::Bottom => "Bottom edge",
    }
}

fn active_peer_label(active_peer: &Option<String>) -> &str {
    active_peer.as_deref().unwrap_or("Auto")
}

fn connect_addr_label(connect_addr: &Option<String>) -> &str {
    connect_addr.as_deref().unwrap_or("Discovery")
}

fn emergency_label(keycode: u32) -> String {
    if keycode == 41 {
        "Esc".into()
    } else {
        format!("HID {keycode}")
    }
}

fn bg_dark() -> egui::Color32 {
    egui::Color32::from_rgb(17, 22, 30)
}

fn surface_dark() -> egui::Color32 {
    egui::Color32::from_rgb(20, 26, 36)
}

fn card_bg() -> egui::Color32 {
    egui::Color32::from_rgb(27, 34, 45)
}

fn accent_blue() -> egui::Color32 {
    egui::Color32::from_rgb(54, 112, 214)
}

fn segmented_role_selector(ui: &mut egui::Ui, role: &mut InputControlRole) {
    ui.horizontal_wrapped(|ui| {
        role_segment(ui, role, InputControlRole::Disabled, "Disabled");
        role_segment(ui, role, InputControlRole::Source, "Source");
        role_segment(ui, role, InputControlRole::Target, "Target");
        role_segment(ui, role, InputControlRole::Both, "Both");
    });
}

fn role_segment(
    ui: &mut egui::Ui,
    role: &mut InputControlRole,
    target: InputControlRole,
    label: &str,
) {
    let selected = *role == target;
    if segment_button(ui, label, selected).clicked() {
        *role = target;
    }
}

fn segmented_edge_selector(ui: &mut egui::Ui, edge: &mut InputHandoffEdge) {
    ui.horizontal_wrapped(|ui| {
        edge_segment(ui, edge, InputHandoffEdge::Left, "Left");
        edge_segment(ui, edge, InputHandoffEdge::Right, "Right");
        edge_segment(ui, edge, InputHandoffEdge::Top, "Top");
        edge_segment(ui, edge, InputHandoffEdge::Bottom, "Bottom");
    });
}

fn edge_segment(
    ui: &mut egui::Ui,
    edge: &mut InputHandoffEdge,
    target: InputHandoffEdge,
    label: &str,
) {
    let selected = *edge == target;
    if segment_button(ui, label, selected).clicked() {
        *edge = target;
    }
}

fn segment_button(ui: &mut egui::Ui, label: &str, selected: bool) -> egui::Response {
    ui.add(
        egui::Button::new(egui::RichText::new(label).size(13.0).color(if selected {
            egui::Color32::WHITE
        } else {
            egui::Color32::from_rgb(196, 205, 218)
        }))
        .fill(if selected {
            accent_blue()
        } else {
            egui::Color32::from_rgb(38, 47, 61)
        })
        .min_size(egui::vec2(78.0, 32.0)),
    )
}

fn layout_preview(
    ui: &mut egui::Ui,
    edge: InputHandoffEdge,
    role: InputControlRole,
    active_peer: &str,
    interactive: bool,
) -> Option<InputHandoffEdge> {
    let width = ui.available_width().clamp(260.0, 820.0);
    let height = if width < 420.0 { 260.0 } else { 230.0 };
    let sense = if interactive {
        egui::Sense::click_and_drag()
    } else {
        egui::Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, height), sense);
    let selected_edge = interactive
        .then(|| edge_from_drag_delta(response.drag_delta()))
        .flatten();
    let edge = selected_edge.unwrap_or(edge);
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 8.0, egui::Color32::from_rgb(18, 24, 34));
    painter.rect_stroke(
        rect,
        8.0,
        egui::Stroke::new(1.0, egui::Color32::from_rgb(50, 62, 80)),
    );

    let source = layout_source_rect(rect, edge);
    let target = layout_target_rect(rect, edge);
    draw_screen_node(
        &painter,
        source,
        "This device",
        role_label(role),
        accent_blue(),
    );
    draw_screen_node(
        &painter,
        target,
        active_peer,
        "Target peer",
        egui::Color32::from_rgb(68, 139, 100),
    );
    draw_handoff_arrow(&painter, source, target, edge);
    selected_edge
}

fn edge_from_drag_delta(delta: egui::Vec2) -> Option<InputHandoffEdge> {
    const DRAG_THRESHOLD: f32 = 24.0;
    if delta.length_sq() < DRAG_THRESHOLD * DRAG_THRESHOLD {
        return None;
    }
    if delta.x.abs() >= delta.y.abs() {
        Some(if delta.x < 0.0 {
            InputHandoffEdge::Left
        } else {
            InputHandoffEdge::Right
        })
    } else {
        Some(if delta.y < 0.0 {
            InputHandoffEdge::Top
        } else {
            InputHandoffEdge::Bottom
        })
    }
}

fn layout_source_rect(bounds: egui::Rect, edge: InputHandoffEdge) -> egui::Rect {
    let margin = 20.0;
    let gap = if bounds.width() < 430.0 { 18.0 } else { 54.0 };
    let horizontal = egui::vec2(
        ((bounds.width() - gap - margin * 2.0) / 2.0).max(108.0),
        118.0,
    );
    let vertical = egui::vec2((bounds.width() - margin * 2.0).clamp(180.0, 340.0), 78.0);
    match edge {
        InputHandoffEdge::Left => egui::Rect::from_min_size(
            egui::pos2(
                bounds.right() - margin - horizontal.x,
                bounds.center().y - horizontal.y / 2.0,
            ),
            horizontal,
        ),
        InputHandoffEdge::Right => egui::Rect::from_min_size(
            egui::pos2(
                bounds.left() + margin,
                bounds.center().y - horizontal.y / 2.0,
            ),
            horizontal,
        ),
        InputHandoffEdge::Top => egui::Rect::from_center_size(
            egui::pos2(bounds.center().x, bounds.bottom() - margin - 42.0),
            vertical,
        ),
        InputHandoffEdge::Bottom => egui::Rect::from_center_size(
            egui::pos2(bounds.center().x, bounds.top() + margin + 42.0),
            vertical,
        ),
    }
}

fn layout_target_rect(bounds: egui::Rect, edge: InputHandoffEdge) -> egui::Rect {
    let margin = 20.0;
    let gap = if bounds.width() < 430.0 { 18.0 } else { 54.0 };
    let horizontal = egui::vec2(
        ((bounds.width() - gap - margin * 2.0) / 2.0).max(108.0),
        118.0,
    );
    let vertical = egui::vec2((bounds.width() - margin * 2.0).clamp(180.0, 340.0), 78.0);
    match edge {
        InputHandoffEdge::Left => egui::Rect::from_min_size(
            egui::pos2(
                bounds.left() + margin,
                bounds.center().y - horizontal.y / 2.0,
            ),
            horizontal,
        ),
        InputHandoffEdge::Right => egui::Rect::from_min_size(
            egui::pos2(
                bounds.right() - margin - horizontal.x,
                bounds.center().y - horizontal.y / 2.0,
            ),
            horizontal,
        ),
        InputHandoffEdge::Top => egui::Rect::from_center_size(
            egui::pos2(bounds.center().x, bounds.top() + margin + 42.0),
            vertical,
        ),
        InputHandoffEdge::Bottom => egui::Rect::from_center_size(
            egui::pos2(bounds.center().x, bounds.bottom() - margin - 42.0),
            vertical,
        ),
    }
}

fn draw_screen_node(
    painter: &egui::Painter,
    rect: egui::Rect,
    title: &str,
    subtitle: &str,
    color: egui::Color32,
) {
    let title = truncate_for_width(title, rect.width(), 9.0);
    let subtitle = truncate_for_width(subtitle, rect.width(), 7.0);
    painter.rect_filled(rect, 8.0, color);
    painter.rect_stroke(
        rect,
        8.0,
        egui::Stroke::new(1.0, egui::Color32::from_rgb(225, 234, 246)),
    );
    painter.text(
        rect.center() + egui::vec2(0.0, -10.0),
        egui::Align2::CENTER_CENTER,
        title,
        egui::FontId::proportional(20.0),
        egui::Color32::WHITE,
    );
    painter.text(
        rect.center() + egui::vec2(0.0, 18.0),
        egui::Align2::CENTER_CENTER,
        subtitle,
        egui::FontId::proportional(13.0),
        egui::Color32::from_rgb(229, 235, 244),
    );
}

fn truncate_for_width(text: &str, width: f32, average_char_width: f32) -> String {
    let max_chars = ((width - 24.0) / average_char_width).floor().max(4.0) as usize;
    if text.chars().count() <= max_chars {
        return text.into();
    }
    let mut truncated: String = text.chars().take(max_chars.saturating_sub(3)).collect();
    truncated.push_str("...");
    truncated
}

fn draw_handoff_arrow(
    painter: &egui::Painter,
    source: egui::Rect,
    target: egui::Rect,
    edge: InputHandoffEdge,
) {
    let (from, to, label_pos, label) = match edge {
        InputHandoffEdge::Left => (
            source.left_center(),
            target.right_center(),
            egui::pos2(
                (source.left() + target.right()) / 2.0,
                source.center().y - 18.0,
            ),
            "left edge",
        ),
        InputHandoffEdge::Right => (
            source.right_center(),
            target.left_center(),
            egui::pos2(
                (source.right() + target.left()) / 2.0,
                source.center().y - 18.0,
            ),
            "right edge",
        ),
        InputHandoffEdge::Top => (
            source.center_top(),
            target.center_bottom(),
            egui::pos2(
                source.center().x + 78.0,
                (source.top() + target.bottom()) / 2.0,
            ),
            "top edge",
        ),
        InputHandoffEdge::Bottom => (
            source.center_bottom(),
            target.center_top(),
            egui::pos2(
                source.center().x + 92.0,
                (source.bottom() + target.top()) / 2.0,
            ),
            "bottom edge",
        ),
    };
    painter.line_segment(
        [from, to],
        egui::Stroke::new(3.0, egui::Color32::from_rgb(235, 190, 92)),
    );
    painter.circle_filled(to, 5.0, egui::Color32::from_rgb(235, 190, 92));
    painter.text(
        label_pos,
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::proportional(12.0),
        egui::Color32::from_rgb(238, 217, 174),
    );
}

fn parse_clipboard_history_json(contents: &[u8]) -> Result<Vec<ClipboardHistoryEntry>, String> {
    if contents.len() > MAX_GUI_HISTORY_JSON_BYTES {
        return Err(format!(
            "clipboard history JSON exceeds {MAX_GUI_HISTORY_JSON_BYTES} bytes"
        ));
    }
    let entries: Vec<ClipboardHistoryWireEntry> = serde_json::from_slice(contents)
        .map_err(|_| "invalid clipboard history JSON".to_string())?;
    if entries.len() > MAX_GUI_HISTORY_ENTRIES {
        return Err(format!(
            "clipboard history exceeds {MAX_GUI_HISTORY_ENTRIES} entries"
        ));
    }

    entries
        .into_iter()
        .map(|entry| {
            Ok(ClipboardHistoryEntry {
                fingerprint: normalize_fingerprint(&entry.fingerprint)?,
                preview: sanitize_history_text(&entry.preview, MAX_GUI_HISTORY_PREVIEW_CHARS),
                origin: sanitize_history_text(&entry.origin, 128),
                at_millis: entry.at_millis,
                pinned: entry.pinned,
                bytes: entry.bytes,
                formats: entry.formats,
            })
        })
        .collect()
}

fn pairing_generation_feedback(
    success: bool,
    stdout: &[u8],
    stderr: &[u8],
) -> PairingGenerationFeedback {
    let uri = String::from_utf8_lossy(stdout).trim().to_owned();
    if success && !uri.is_empty() {
        return PairingGenerationFeedback {
            uri: Some(uri),
            urgency: NotificationUrgency::Normal,
            title: "Pairing URI generated",
            detail: "Pairing URI generated for this Mac".to_string(),
        };
    }

    let detail = if success {
        "Pairing URI command returned no URI".to_string()
    } else {
        bounded_command_error(stderr)
    };
    PairingGenerationFeedback {
        uri: None,
        urgency: NotificationUrgency::High,
        title: "Pairing URI failed",
        detail,
    }
}

fn bounded_command_error(stderr: &[u8]) -> String {
    let byte_truncated = stderr.len() > MAX_GUI_COMMAND_ERROR_BYTES;
    let stderr = &stderr[..stderr.len().min(MAX_GUI_COMMAND_ERROR_BYTES)];
    let decoded = String::from_utf8_lossy(stderr);
    let char_truncated = decoded.chars().count() > MAX_GUI_COMMAND_ERROR_CHARS;
    let mut sanitized = decoded
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(MAX_GUI_COMMAND_ERROR_CHARS)
        .collect::<String>()
        .trim()
        .to_string();
    if sanitized.is_empty() {
        return "Pairing URI command failed without an error message".to_string();
    }
    if byte_truncated || char_truncated {
        sanitized.push('…');
    }
    sanitized
}

fn sanitize_history_text(text: &str, max_chars: usize) -> String {
    let mut sanitized: String = text
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
        sanitized.push('…');
    }
    sanitized
}

fn normalize_fingerprint(fingerprint: &str) -> Result<String, String> {
    let fingerprint = fingerprint.trim();
    let fingerprint = fingerprint
        .strip_prefix("0x")
        .or_else(|| fingerprint.strip_prefix("0X"))
        .unwrap_or(fingerprint);
    if fingerprint.is_empty()
        || fingerprint.len() > 16
        || !fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("Clipboard fingerprint must be 1 to 16 hexadecimal digits".to_string());
    }
    let fingerprint = u64::from_str_radix(fingerprint, 16)
        .map_err(|_| "Clipboard fingerprint is outside the supported range".to_string())?;
    Ok(format!("{fingerprint:016x}"))
}

fn clipboard_restore_args(fingerprint: &str) -> Result<Vec<OsString>, String> {
    Ok(vec![
        OsString::from("clipboard-restore"),
        OsString::from(normalize_fingerprint(fingerprint)?),
    ])
}

fn file_send_args<I>(dropped_paths: I) -> Result<Vec<OsString>, String>
where
    I: IntoIterator<Item = Option<PathBuf>>,
{
    let mut args = vec![OsString::from("file-send"), OsString::from("--")];
    for path in dropped_paths.into_iter().flatten() {
        if path.as_os_str().is_empty() {
            continue;
        }
        if path.to_str().is_none() {
            return Err("Dropped paths must be valid Unicode for the file-send CLI".to_string());
        }
        if args.len().saturating_sub(2) >= MAX_DROPPED_PATHS {
            return Err(format!(
                "A single drop may contain at most {MAX_DROPPED_PATHS} local paths"
            ));
        }
        args.push(path.into_os_string());
    }
    if args.len() == 2 {
        return Err(
            "Drop at least one local file or folder; URL-only drops are not supported".to_string(),
        );
    }
    Ok(args)
}

fn prepare_sharing_config(config: &mut Config) -> Result<(), String> {
    config.clipboard.history_capacity = config
        .clipboard
        .history_capacity
        .clamp(MIN_HISTORY_CAPACITY, MAX_HISTORY_CAPACITY);
    config.clipboard.history_max_entry_bytes = config
        .clipboard
        .history_max_entry_bytes
        .clamp(MIN_HISTORY_ENTRY_BYTES, MAX_HISTORY_ENTRY_BYTES);
    config.file_transfer.max_transfer_bytes = config
        .file_transfer
        .max_transfer_bytes
        .clamp(MIN_TRANSFER_BYTES, MAX_TRANSFER_BYTES);

    let download_dir = match config.file_transfer.download_dir.as_deref() {
        Some(path) if path.trim().chars().count() > MAX_DOWNLOAD_DIR_CHARS => {
            return Err(format!(
                "Download directory must be at most {MAX_DOWNLOAD_DIR_CHARS} characters"
            ));
        }
        Some(path) if path.chars().any(char::is_control) => {
            return Err("Download directory cannot contain control characters".to_string());
        }
        Some(path) if !path.trim().is_empty() => Some(path.trim().to_owned()),
        _ => None,
    };
    config.file_transfer.download_dir = download_dir;
    Ok(())
}

fn config_path() -> PathBuf {
    let base = if cfg!(target_os = "windows") {
        std::env::var("APPDATA")
            .map(PathBuf::from)
            .or_else(|_| {
                std::env::var("USERPROFILE")
                    .map(PathBuf::from)
                    .map(|home| home.join("AppData").join("Roaming"))
            })
            .unwrap_or_else(|_| PathBuf::from("."))
    } else if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(xdg)
    } else if let Ok(home) = std::env::var("HOME") {
        let home = PathBuf::from(home);
        if cfg!(target_os = "macos") {
            home.join("Library/Application Support")
        } else {
            home.join(".config")
        }
    } else {
        PathBuf::from(".")
    };
    base.join("nexkvm").join("config.toml")
}

fn daemon_log_path(config_path: &std::path::Path) -> PathBuf {
    config_path
        .parent()
        .map(|parent| parent.join("daemon.log"))
        .unwrap_or_else(|| PathBuf::from("nexkvm-daemon.log"))
}

fn previous_daemon_log_path(path: &std::path::Path) -> PathBuf {
    path.with_extension("log.previous")
}

fn open_daemon_log(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    open_daemon_log_with_limit(path, MAX_DAEMON_LOG_BYTES)
}

fn open_daemon_log_with_limit(
    path: &std::path::Path,
    max_bytes: u64,
) -> std::io::Result<std::fs::File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        }
    }

    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "daemon log path must be a regular file",
            ));
        }
        Ok(metadata) if metadata.len() >= max_bytes && metadata.len() > 0 => {
            let previous = previous_daemon_log_path(path);
            match std::fs::symlink_metadata(&previous) {
                Ok(previous_metadata)
                    if previous_metadata.file_type().is_symlink()
                        || !previous_metadata.is_file() =>
                {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "previous daemon log path must be a regular file",
                    ));
                }
                Ok(_) => std::fs::remove_file(&previous)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            std::fs::rename(path, previous)?;
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "daemon log path must be a regular file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

fn nexkvm_binary() -> PathBuf {
    if let Ok(path) = std::env::var("NEXKVM_BIN") {
        return PathBuf::from(path);
    }
    if let Ok(current_exe) = std::env::current_exe()
        && let Some(dir) = current_exe.parent()
    {
        let sibling = dir.join(executable_name("nexkvm"));
        if sibling.is_file() {
            return sibling;
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        let local_bin = PathBuf::from(home)
            .join(".local")
            .join("bin")
            .join(executable_name("nexkvm"));
        if local_bin.is_file() {
            return local_bin;
        }
    }
    PathBuf::from("nexkvm")
}

fn executable_name(base: &str) -> String {
    if cfg!(windows) {
        format!("{base}.exe")
    } else {
        base.into()
    }
}

fn read_log_tail(path: &std::path::Path, max_bytes: usize) -> String {
    match std::fs::File::open(path).and_then(|mut file| read_log_tail_from(&mut file, max_bytes)) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(error) => format!("Failed to read daemon log {}: {error}", path.display()),
    }
}

fn read_log_tail_from<R>(reader: &mut R, max_bytes: usize) -> std::io::Result<Vec<u8>>
where
    R: std::io::Read + std::io::Seek,
{
    use std::io::Read as _;

    let length = reader.seek(std::io::SeekFrom::End(0))?;
    let requested = u64::try_from(max_bytes).unwrap_or(u64::MAX);
    let suffix_len = length.min(requested);
    reader.seek(std::io::SeekFrom::Start(length - suffix_len))?;
    let capacity = usize::try_from(suffix_len).unwrap_or(max_bytes);
    let mut bytes = Vec::with_capacity(capacity);
    reader.take(suffix_len).read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn terminate_owned_daemon(child: &mut Child) -> std::io::Result<()> {
    if child.try_wait()?.is_some() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        let status = Command::new("kill")
            .arg("-TERM")
            .arg(child.id().to_string())
            .status()?;
        if !status.success() {
            child.kill()?;
        }
    }
    #[cfg(windows)]
    child.kill()?;

    for _ in 0..12 {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(150));
    }
    child.kill()?;
    child.wait().map(|_| ())
}

fn daemon_listen_port_available(port: u16) -> bool {
    tcp_port_available(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)))
}

fn tcp_port_available(addr: SocketAddr) -> bool {
    TcpListener::bind(addr).is_ok()
}

fn local_pairing_addr() -> String {
    String::new()
}

fn pairing_accept_ready(current_uri: &str, inspected_uri: Option<&str>) -> bool {
    let current_uri = current_uri.trim();
    !current_uri.is_empty() && inspected_uri == Some(current_uri)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddr, TcpListener};

    #[test]
    fn occupied_port_is_never_treated_as_authority_to_kill_a_process() {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
        let addr = listener.local_addr().unwrap();

        assert!(!tcp_port_available(addr));
    }

    #[test]
    fn pairing_form_starts_blank_and_file_drop_copy_includes_folders() {
        assert!(local_pairing_addr().is_empty());
        let error = file_send_args(Vec::<Option<PathBuf>>::new()).unwrap_err();
        assert!(error.contains("file or folder"));
    }

    #[test]
    fn rejected_pairing_generation_shows_a_bounded_sanitized_failure() {
        let stderr = format!(
            "invalid\naddress\0{}",
            "x".repeat(MAX_GUI_COMMAND_ERROR_BYTES * 2)
        );

        let feedback = pairing_generation_feedback(false, b"", stderr.as_bytes());

        assert_eq!(feedback.title, "Pairing URI failed");
        assert_eq!(feedback.urgency, NotificationUrgency::High);
        assert!(feedback.uri.is_none());
        assert!(feedback.detail.starts_with("invalid address "));
        assert!(!feedback.detail.chars().any(char::is_control));
        assert!(feedback.detail.chars().count() <= MAX_GUI_COMMAND_ERROR_CHARS + 1);
        assert!(feedback.detail.ends_with('…'));
    }

    #[test]
    fn occupied_tcp_port_is_not_available() {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
        let addr = listener.local_addr().unwrap();

        assert!(!tcp_port_available(addr));
    }

    #[test]
    fn tcp_port_becomes_available_after_listener_drops() {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        assert!(tcp_port_available(addr));
    }

    #[test]
    fn gui_notifications_keep_newest_events() {
        let mut notifications = VecDeque::new();
        let mut next_id = 0;

        for index in 0..(MAX_GUI_NOTIFICATIONS + 3) {
            record_notification(
                &mut notifications,
                &mut next_id,
                NotificationUrgency::Normal,
                format!("event {index}"),
                "body",
            );
        }

        assert_eq!(notifications.len(), MAX_GUI_NOTIFICATIONS);
        assert_eq!(notifications.front().unwrap().title, "event 26");
        assert_eq!(notifications.back().unwrap().title, "event 3");
    }

    #[test]
    fn initial_notification_marks_gui_ready() {
        let notifications = initial_notifications();

        assert_eq!(notifications.len(), 1);
        assert_eq!(notifications.front().unwrap().title, "GUI ready");
        assert_eq!(
            notifications.front().unwrap().urgency,
            NotificationUrgency::Low
        );
    }

    #[test]
    fn malformed_config_is_reported_and_cannot_be_silently_overwritten() {
        let unique = format!(
            "nexkvm-gui-config-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let directory = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("config.toml");
        std::fs::write(&path, b"[input\nhandoff_edge = ").unwrap();
        let before = std::fs::read(&path).unwrap();

        let mut gui = NexkvmGui::load_from(path.clone());

        assert!(gui.config_load_error.is_some());
        assert_eq!(
            gui.notifications.front().unwrap().urgency,
            NotificationUrgency::High
        );
        assert!(!gui.persist_config());
        assert_eq!(std::fs::read(&path).unwrap(), before);

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn pairing_accept_requires_the_exact_inspected_uri() {
        let uri = "nexkvm://pair/v1/0011";

        assert!(!pairing_accept_ready(uri, None));
        assert!(pairing_accept_ready("  nexkvm://pair/v1/0011\n", Some(uri)));
        assert!(!pairing_accept_ready("nexkvm://pair/v1/0022", Some(uri)));
    }

    #[test]
    fn log_tail_reader_seeks_to_a_bounded_suffix() {
        let mut cursor = std::io::Cursor::new(b"prefix-secret-tail".to_vec());

        let bytes = read_log_tail_from(&mut cursor, 4).unwrap();

        assert_eq!(bytes, b"tail");
        assert_eq!(cursor.position(), 18);
    }

    #[test]
    fn saved_runtime_settings_restart_only_a_gui_owned_daemon() {
        assert_eq!(runtime_apply_action(true), RuntimeApplyAction::RestartOwned);
        assert_eq!(
            runtime_apply_action(false),
            RuntimeApplyAction::RestartRequired
        );
    }

    #[test]
    fn daemon_log_rotates_before_append_when_size_limit_is_reached() {
        use std::io::Write as _;

        let unique = format!(
            "nexkvm-gui-log-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let directory = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("daemon.log");
        std::fs::write(&path, b"old-log").unwrap();

        let mut file = open_daemon_log_with_limit(&path, 4).unwrap();
        file.write_all(b"new").unwrap();
        file.sync_all().unwrap();
        drop(file);

        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert_eq!(
            std::fs::read(previous_daemon_log_path(&path)).unwrap(),
            b"old-log"
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn dragging_target_selects_the_dominant_screen_side() {
        assert_eq!(
            edge_from_drag_delta(egui::vec2(-80.0, 10.0)),
            Some(InputHandoffEdge::Left)
        );
        assert_eq!(
            edge_from_drag_delta(egui::vec2(80.0, -10.0)),
            Some(InputHandoffEdge::Right)
        );
        assert_eq!(
            edge_from_drag_delta(egui::vec2(10.0, -80.0)),
            Some(InputHandoffEdge::Top)
        );
        assert_eq!(
            edge_from_drag_delta(egui::vec2(-10.0, 80.0)),
            Some(InputHandoffEdge::Bottom)
        );
        assert_eq!(edge_from_drag_delta(egui::vec2(10.0, 10.0)), None);
    }

    #[test]
    fn malformed_clipboard_history_json_is_rejected_without_panicking() {
        assert!(parse_clipboard_history_json(br#"[{\"fingerprint\":]"#).is_err());
        assert!(parse_clipboard_history_json(br#"{\"entries\":[]}"#).is_err());
        assert!(parse_clipboard_history_json(&vec![b' '; MAX_GUI_HISTORY_JSON_BYTES + 1]).is_err());
    }

    #[test]
    fn clipboard_history_parser_keeps_metadata_but_redacts_debug_preview() {
        let entries = parse_clipboard_history_json(
            br#"[{"fingerprint":"00000000000000ff","preview":"private clipboard text","origin":"peer-1","at_millis":42,"pinned":false,"bytes":22,"formats":1}]"#,
        )
        .unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].fingerprint, "00000000000000ff");
        assert_eq!(entries[0].preview, "private clipboard text");
        assert_eq!(entries[0].bytes, 22);
        assert!(!format!("{entries:?}").contains("private clipboard text"));
    }

    #[test]
    fn clipboard_restore_arguments_require_one_u64_hex_fingerprint() {
        let args = clipboard_restore_args("0x00000000000000ff").unwrap();
        assert_eq!(
            args,
            [
                std::ffi::OsString::from("clipboard-restore"),
                std::ffi::OsString::from("00000000000000ff"),
            ]
        );

        assert!(clipboard_restore_args("").is_err());
        assert!(clipboard_restore_args("not-hex").is_err());
        assert!(clipboard_restore_args("10000000000000000").is_err());
    }

    #[test]
    fn dropped_file_arguments_reject_empty_and_url_only_drops() {
        assert!(file_send_args(Vec::<Option<PathBuf>>::new()).is_err());
        assert!(file_send_args([None]).is_err());

        let args = file_send_args([
            Some(PathBuf::from("/tmp/a file.txt")),
            None,
            Some(PathBuf::from("-notes.txt")),
        ])
        .unwrap();
        assert_eq!(
            args,
            [
                std::ffi::OsString::from("file-send"),
                std::ffi::OsString::from("--"),
                std::ffi::OsString::from("/tmp/a file.txt"),
                std::ffi::OsString::from("-notes.txt"),
            ]
        );
    }

    #[test]
    fn sharing_config_is_bounded_and_rejects_control_characters_in_paths() {
        let mut config = Config::default();
        config.clipboard.history_capacity = 0;
        config.clipboard.history_max_entry_bytes = usize::MAX;
        config.file_transfer.max_transfer_bytes = u64::MAX;
        config.file_transfer.download_dir = Some("unsafe\npath".into());

        assert!(prepare_sharing_config(&mut config).is_err());

        config.file_transfer.download_dir = Some("  /tmp/NexKVM Downloads  ".into());
        prepare_sharing_config(&mut config).unwrap();
        assert_eq!(config.clipboard.history_capacity, MIN_HISTORY_CAPACITY);
        assert_eq!(
            config.clipboard.history_max_entry_bytes,
            MAX_HISTORY_ENTRY_BYTES
        );
        assert_eq!(config.file_transfer.max_transfer_bytes, MAX_TRANSFER_BYTES);
        assert_eq!(
            config.file_transfer.download_dir.as_deref(),
            Some("/tmp/NexKVM Downloads")
        );
    }
}
