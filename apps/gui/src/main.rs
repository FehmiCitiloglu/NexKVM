use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use eframe::egui;
use nexkvm_storage::{Config, InputControlRole, InputHandoffEdge};

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
    section: Section,
    status: String,
    daemon: Option<Child>,
    pairing_addr: String,
    pairing_uri: String,
    accept_uri: String,
    command_output: String,
}

impl NexkvmGui {
    fn load() -> Self {
        let config_path = config_path();
        let config = Config::load(&config_path).unwrap_or_default();
        Self {
            config_path,
            config,
            section: Section::Overview,
            status: "Ready".into(),
            daemon: None,
            pairing_addr: local_pairing_addr(),
            pairing_uri: String::new(),
            accept_uri: String::new(),
            command_output: String::new(),
        }
    }

    fn save_config(&mut self) {
        match self.config.save(&self.config_path) {
            Ok(()) => self.status = format!("Saved {}", self.config_path.display()),
            Err(error) => self.status = format!("Save failed: {error}"),
        }
    }

    fn persist_config(&mut self) -> bool {
        match self.config.save(&self.config_path) {
            Ok(()) => true,
            Err(error) => {
                self.status = format!("Save failed: {error}");
                false
            }
        }
    }

    fn run_command(&mut self, args: &[&str]) {
        match Command::new(nexkvm_binary()).args(args).output() {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                self.command_output = if stderr.trim().is_empty() {
                    stdout.to_string()
                } else {
                    format!("{stdout}\n{stderr}")
                };
                self.status = format!("Command exited: {}", output.status);
            }
            Err(error) => {
                self.status = format!("Command failed: {error}");
            }
        }
    }

    fn start_daemon(&mut self) {
        if self.daemon.is_some() {
            self.status = "Daemon already started from this window".into();
            return;
        }
        if !self.persist_config() {
            return;
        }
        match Command::new(nexkvm_binary())
            .arg("--debug")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => {
                self.daemon = Some(child);
                self.status = "Daemon started".into();
            }
            Err(error) => self.status = format!("Failed to start daemon: {error}"),
        }
    }

    fn stop_daemon(&mut self) {
        if let Some(mut child) = self.daemon.take() {
            let _ = child.kill();
            let _ = child.wait();
            self.status = "Daemon stopped".into();
            return;
        }
        #[cfg(unix)]
        let output = Command::new("pkill").args(["-9", "-x", "nexkvm"]).output();
        #[cfg(windows)]
        let output = Command::new("taskkill")
            .args(["/IM", "nexkvm.exe", "/F"])
            .output();
        match output {
            Ok(output) if output.status.success() => self.status = "Daemon stopped".into(),
            Ok(output) => {
                self.status = format!("Stop command exited: {}", output.status);
            }
            Err(error) => self.status = format!("Stop failed: {error}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Overview,
    Settings,
    Pairing,
}

impl eframe::App for NexkvmGui {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        apply_theme(ctx);
        self.refresh_daemon_state();

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
                nav_item(ui, &mut self.section, Section::Pairing, "Pairing & Output");

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
                    Section::Pairing => self.pairing_page(ui),
                });
        });
    }
}

impl NexkvmGui {
    fn refresh_daemon_state(&mut self) {
        if let Some(child) = &mut self.daemon
            && matches!(child.try_wait(), Ok(Some(_)))
        {
            self.daemon = None;
            self.status = "Daemon exited".into();
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
                    status_chip(
                        ui,
                        if self.config.security.require_pairing {
                            "Paired only"
                        } else {
                            "Open pairing"
                        },
                        egui::Color32::from_rgb(73, 137, 94),
                    );
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
                            if self.config.security.require_pairing {
                                "Paired only"
                            } else {
                                "Open pairing"
                            },
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
                metric_row(
                    ui,
                    "Pairing",
                    if self.config.security.require_pairing {
                        "Required"
                    } else {
                        "Open"
                    },
                );
                metric_row(
                    ui,
                    "Active peer",
                    active_peer_label(&self.config.input.active_peer),
                );
                metric_row(ui, "Handoff", edge_label(self.config.input.handoff_edge));
            }),
            2 => card(ui, card_width, |ui| {
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
                });
            }),
            _ => {}
        });

        ui.add_space(16.0);
        card(ui, ui.available_width() - 8.0, |ui| {
            card_title(ui, "Desktop layout");
            ui.add_space(10.0);
            layout_preview(
                ui,
                self.config.input.handoff_edge,
                self.config.input.control_role,
                active_peer_label(&self.config.input.active_peer),
            );
            ui.add_space(10.0);
            ui.label(muted_text(
                "Move across the configured edge to hand control to the paired target.",
            ));
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
                ui.checkbox(&mut self.config.security.require_pairing, "Require pairing");
                ui.checkbox(
                    &mut self.config.security.trust_on_reconnect,
                    "Trust known reconnects",
                );
                ui.horizontal(|ui| {
                    ui.label(field_label("Listen port"));
                    ui.add(egui::DragValue::new(&mut self.config.network.listen_port));
                });
            }),
            _ => {}
        });

        ui.add_space(14.0);
        ui.horizontal_wrapped(|ui| {
            if primary_button(ui, "Save settings").clicked() {
                self.save_config();
            }
            if secondary_button(ui, "Start with these settings").clicked() {
                self.start_daemon();
            }
        });
    }

    fn pairing_page(&mut self, ui: &mut egui::Ui) {
        responsive_cards(ui, 2, 390.0, |ui, index, card_width| match index {
            0 => card(ui, card_width, |ui| {
                card_title(ui, "Generate pairing URI");
                ui.add_space(8.0);
                labeled_text(ui, "Peer address", &mut self.pairing_addr);
                if primary_button(ui, "Generate").clicked() {
                    match Command::new(nexkvm_binary())
                        .args(["pairing-uri", self.pairing_addr.as_str()])
                        .output()
                    {
                        Ok(output) => {
                            self.pairing_uri =
                                String::from_utf8_lossy(&output.stdout).trim().to_string();
                            self.status = format!("Command exited: {}", output.status);
                        }
                        Err(error) => self.status = format!("Pairing URI failed: {error}"),
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
                ui.add(
                    egui::TextEdit::multiline(&mut self.accept_uri)
                        .desired_rows(5)
                        .hint_text("nexkvm://pair/v1/..."),
                );
                if primary_button(ui, "Accept pairing").clicked() {
                    let uri = self.accept_uri.clone();
                    self.run_command(&["pair", "--accept", uri.as_str()]);
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
        Section::Pairing => (
            "Pairing & Output",
            "Pair devices and inspect command results.",
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
    if ui.available_width() < 260.0 {
        ui.vertical(|ui| {
            ui.label(field_label(label));
            ui.label(egui::RichText::new(value).color(egui::Color32::from_rgb(235, 239, 246)));
        });
        return;
    }
    ui.horizontal_wrapped(|ui| {
        ui.set_min_height(24.0);
        ui.label(field_label(label));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(egui::RichText::new(value).color(egui::Color32::from_rgb(235, 239, 246)));
        });
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
) {
    let width = ui.available_width().clamp(260.0, 820.0);
    let height = if width < 420.0 { 260.0 } else { 230.0 };
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
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

fn config_path() -> PathBuf {
    let base = if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(xdg)
    } else if let Ok(home) = std::env::var("HOME") {
        let home = PathBuf::from(home);
        if cfg!(target_os = "macos") {
            home.join("Library/Application Support")
        } else {
            home.join(".config")
        }
    } else if let Ok(appdata) = std::env::var("APPDATA") {
        PathBuf::from(appdata)
    } else {
        PathBuf::from(".")
    };
    base.join("nexkvm").join("config.toml")
}

fn nexkvm_binary() -> PathBuf {
    if let Ok(path) = std::env::var("NEXKVM_BIN") {
        return PathBuf::from(path);
    }
    PathBuf::from("nexkvm")
}

fn local_pairing_addr() -> String {
    "127.0.0.1:47654".into()
}
