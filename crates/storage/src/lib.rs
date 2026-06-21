//! Configuration & persistent storage.
//!
//! # Decision: TOML config
//! nexkvm uses **TOML** for its user-facing config: it is human-friendly,
//! comment-friendly, and round-trips cleanly with `serde`. The schema is a flat
//! set of sections ([`Config`]) that mirror the crate boundaries, so each
//! subsystem owns its own settings block.
//!
//! The default config location follows the platform convention (resolved by the
//! desktop app, not hard-coded here so the type stays testable):
//! - Linux: `$XDG_CONFIG_HOME/nexkvm/config.toml`
//! - macOS: `~/Library/Application Support/nexkvm/config.toml`
//! - Windows: `%APPDATA%\nexkvm\config.toml`
//!
//! Secrets (private keys, paired-device material) are **not** stored in this
//! TOML file — they belong in the OS keychain, wired up in a later phase.

use std::path::Path;

use nexkvm_core::identity::OsKind;
use nexkvm_telemetry::TelemetryConfig;
use serde::{Deserialize, Serialize};
use thiserror::Error;

mod identity;
mod trust;

pub use identity::{FileDeviceIdentityStore, IdentityStoreError};
pub use trust::{FileTrustStore, TrustStoreError};

/// Errors loading or saving configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// Failed to read/write the config file.
    #[error("config io error: {0}")]
    Io(#[from] std::io::Error),

    /// Failed to parse TOML.
    #[error("config parse error: {0}")]
    Parse(#[from] toml::de::Error),

    /// Failed to serialize TOML.
    #[error("config serialize error: {0}")]
    Serialize(#[from] toml::ser::Error),
}

/// Top-level configuration, one section per subsystem.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// This device's identity/presentation.
    pub device: DeviceConfig,
    /// Network/transport settings.
    pub network: NetworkConfig,
    /// Security & pairing policy.
    pub security: SecurityConfig,
    /// Keyboard/mouse sharing runtime settings.
    pub input: InputConfig,
    /// Logging/diagnostics.
    pub telemetry: TelemetryConfig,
    /// Plugin runtime settings.
    pub plugins: PluginConfig,
    /// Shared workspace settings.
    pub workspace: WorkspaceConfig,
    /// Collaborative session settings.
    pub collaboration: CollaborationConfig,
}

/// `[device]` section.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DeviceConfig {
    /// Friendly device name shown to peers.
    pub name: String,
}

impl Default for DeviceConfig {
    fn default() -> Self {
        Self {
            name: default_device_name(),
        }
    }
}

/// `[network]` section.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NetworkConfig {
    /// Port to listen on for incoming connections.
    pub listen_port: u16,
    /// Whether to advertise this device on the LAN for discovery.
    pub enable_discovery: bool,
    /// Preferred transports in priority order (e.g. `["quic", "tcp"]`).
    pub transports: Vec<String>,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            listen_port: 47_654,
            enable_discovery: true,
            transports: vec!["quic".into(), "tcp".into()],
        }
    }
}

/// `[security]` section.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SecurityConfig {
    /// Require explicit pairing before accepting any session.
    pub require_pairing: bool,
    /// Auto-accept reconnects from already-trusted devices.
    pub trust_on_reconnect: bool,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            require_pairing: true,
            trust_on_reconnect: true,
        }
    }
}

/// `[input]` section.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct InputConfig {
    /// Runtime role for keyboard/mouse sharing.
    pub control_role: InputControlRole,
    /// Friendly trusted-peer name or fingerprint selected as the active target.
    pub active_peer: Option<String>,
    /// Which local desktop edge hands control to the active peer.
    pub handoff_edge: InputHandoffEdge,
    /// HID usage id for the emergency stop key. Default 41 is Escape.
    pub emergency_stop_keycode: u32,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            control_role: InputControlRole::Disabled,
            active_peer: None,
            handoff_edge: InputHandoffEdge::Right,
            emergency_stop_keycode: 41,
        }
    }
}

/// Local desktop edge used to hand control to a peer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InputHandoffEdge {
    /// Left edge.
    Left,
    /// Right edge.
    #[default]
    Right,
    /// Top edge.
    Top,
    /// Bottom edge.
    Bottom,
}

/// Whether this daemon captures, injects, both, or neither.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InputControlRole {
    /// Do not run keyboard/mouse sharing.
    #[default]
    Disabled,
    /// Capture local input and send it to `active_peer`.
    Source,
    /// Inject input received from a trusted peer.
    Target,
    /// Enable source and target behavior.
    Both,
}

/// `[plugins]` section.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginConfig {
    /// Master switch for the plugin runtime.
    pub enabled: bool,
    /// Plugin ids explicitly allowed to load.
    pub allowed: Vec<String>,
}

/// `[workspace]` section.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspaceConfig {
    /// Enable the unified virtual desktop/spatial navigation surface.
    pub unified_desktop: bool,
    /// Allow trusted peers to request app launches on this device.
    pub allow_remote_app_launch: bool,
    /// Include this device in global search federation.
    pub global_search: bool,
    /// Sync shared workspace memory to trusted peers.
    pub shared_memory: bool,
    /// Maximum local shared memory entries retained before pruning.
    pub memory_max_entries: usize,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            unified_desktop: true,
            allow_remote_app_launch: false,
            global_search: true,
            shared_memory: true,
            memory_max_entries: 1_000,
        }
    }
}

/// `[collaboration]` section.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CollaborationConfig {
    /// Enable shared cursor sessions.
    pub shared_cursor: bool,
    /// Enable pair-programming mode.
    pub pair_programming: bool,
    /// Allow trusted peers to request delegated control.
    pub allow_control_requests: bool,
    /// Allow this device to grant input control to trusted peers.
    pub allow_delegated_control: bool,
    /// Enable remote teaching sessions.
    pub remote_teaching: bool,
    /// Maximum active participants in collaborative sessions.
    pub max_participants: usize,
    /// Default delegated-control lease duration in milliseconds.
    pub default_control_lease_millis: u64,
}

impl Default for CollaborationConfig {
    fn default() -> Self {
        Self {
            shared_cursor: true,
            pair_programming: true,
            allow_control_requests: true,
            allow_delegated_control: false,
            remote_teaching: true,
            max_participants: 8,
            default_control_lease_millis: 5 * 60 * 1000,
        }
    }
}

impl Config {
    /// Load configuration from `path`, returning defaults if it does not exist.
    ///
    /// # Errors
    /// Returns [`ConfigError`] on I/O failure (other than not-found) or parse
    /// failure.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(toml::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(ConfigError::Io(e)),
        }
    }

    /// Serialize and write configuration to `path`, creating parent dirs.
    ///
    /// # Errors
    /// Returns [`ConfigError`] on serialization or I/O failure.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), ConfigError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)?;
        std::fs::write(path, text)?;
        Ok(())
    }
}

fn default_device_name() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| match std::env::consts::OS {
            "macos" => "nexkvm (macOS)".into(),
            "windows" => "nexkvm (Windows)".into(),
            "linux" => "nexkvm (Linux)".into(),
            _ => "nexkvm device".into(),
        })
}

/// Map the running OS to the core [`OsKind`].
#[must_use]
pub fn current_os() -> OsKind {
    match std::env::consts::OS {
        "macos" => OsKind::MacOs,
        "windows" => OsKind::Windows,
        "linux" => OsKind::Linux,
        "android" => OsKind::Android,
        "ios" => OsKind::Ios,
        _ => OsKind::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip_through_toml() {
        let cfg = Config::default();
        let text = toml::to_string_pretty(&cfg).unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed.network.listen_port, cfg.network.listen_port);
        assert_eq!(
            parsed.security.require_pairing,
            cfg.security.require_pairing
        );
        assert_eq!(parsed.workspace.global_search, cfg.workspace.global_search);
        assert_eq!(
            parsed.workspace.allow_remote_app_launch,
            cfg.workspace.allow_remote_app_launch
        );
        assert_eq!(
            parsed.collaboration.allow_delegated_control,
            cfg.collaboration.allow_delegated_control
        );
        assert_eq!(
            parsed.collaboration.default_control_lease_millis,
            cfg.collaboration.default_control_lease_millis
        );
        assert_eq!(parsed.input.control_role, cfg.input.control_role);
        assert_eq!(
            parsed.input.emergency_stop_keycode,
            cfg.input.emergency_stop_keycode
        );
    }

    #[test]
    fn input_config_round_trips_through_toml() {
        let text = r#"
[input]
control_role = "source"
active_peer = "studio-mac"
handoff_edge = "right"
emergency_stop_keycode = 41
"#;

        let parsed: Config = toml::from_str(text).unwrap();

        assert_eq!(parsed.input.control_role, InputControlRole::Source);
        assert_eq!(parsed.input.active_peer.as_deref(), Some("studio-mac"));
        assert_eq!(parsed.input.handoff_edge, InputHandoffEdge::Right);
        assert_eq!(parsed.input.emergency_stop_keycode, 41);

        let rendered = toml::to_string_pretty(&parsed).unwrap();
        assert!(rendered.contains("[input]"));
        assert!(rendered.contains("control_role = \"source\""));
    }

    #[test]
    fn missing_file_yields_defaults() {
        let cfg = Config::load("/nonexistent/nexkvm/config.toml").unwrap();
        assert!(cfg.security.require_pairing);
    }

    #[test]
    fn save_then_load_is_stable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut cfg = Config::default();
        cfg.device.name = "Test Device".into();
        cfg.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.device.name, "Test Device");
    }
}
