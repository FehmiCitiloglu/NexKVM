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
//! Secrets are **not** stored in this TOML file. The current supported path uses
//! separate owner-only filesystem stores for the long-term identity and encrypted
//! clipboard-history key; see `docs/security.md` for their exact protections and
//! limitations. Paired-device records contain public keys and metadata only.

use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

use nexkvm_core::identity::OsKind;
use nexkvm_telemetry::TelemetryConfig;
use serde::{Deserialize, Serialize};
use thiserror::Error;

mod bounded_file;
mod clipboard_history;

mod identity;
mod trust;

pub use clipboard_history::{
    ArchivedClipboardEntry, ClipboardHistoryArchive, ClipboardHistoryArchiveConfig,
    ClipboardHistoryStoreError,
};
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

    /// The configured path is a symlink or does not name a regular file.
    #[error("config path is a symlink or non-regular file")]
    UnsafePath,

    /// The config exceeds the bounded read/write size.
    #[error("config exceeds the {limit}-byte size limit")]
    TooLarge {
        /// Maximum accepted serialized config size.
        limit: u64,
    },
}

const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

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
    /// Clipboard runtime settings.
    pub clipboard: ClipboardConfig,
    /// Trusted-peer file transfer settings.
    pub file_transfer: FileTransferConfig,
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
    /// Optional explicit peer address to dial on startup (`host:port`).
    pub connect_addr: Option<String>,
    /// Whether to advertise this device on the LAN for discovery.
    pub enable_discovery: bool,
    /// Legacy transport preference retained for configuration compatibility.
    /// The supported desktop runtime currently uses TCP only and warns when this
    /// list contains unsupported values.
    pub transports: Vec<String>,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            listen_port: 47_654,
            connect_addr: None,
            enable_discovery: true,
            transports: vec!["tcp".into()],
        }
    }
}

/// `[security]` section.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SecurityConfig {
    /// Require explicit pairing before accepting any session.
    pub require_pairing: bool,
    /// Deprecated compatibility field; the supported runtime currently ignores
    /// this value and reconnects pinned peers whenever discovery is enabled.
    /// Disable LAN discovery to disable automatic trusted-peer rediscovery.
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
    /// Remote focus is released after this many milliseconds without captured
    /// input. Set to 0 to disable the timeout.
    pub remote_focus_timeout_millis: u64,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            control_role: InputControlRole::Disabled,
            active_peer: None,
            handoff_edge: InputHandoffEdge::Right,
            emergency_stop_keycode: 41,
            remote_focus_timeout_millis: 3_000,
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

/// `[clipboard]` section.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ClipboardConfig {
    /// Enable runtime clipboard synchronization with trusted peers.
    pub sync_enabled: bool,
    /// Retain non-secret local and received selections in encrypted history.
    pub history_enabled: bool,
    /// Maximum selections retained in encrypted history.
    pub history_capacity: usize,
    /// Maximum compact size of one retained selection.
    pub history_max_entry_bytes: usize,
}

impl Default for ClipboardConfig {
    fn default() -> Self {
        Self {
            sync_enabled: false,
            history_enabled: false,
            history_capacity: 50,
            history_max_entry_bytes: 2 * 1024 * 1024,
        }
    }
}

/// `[file_transfer]` section.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FileTransferConfig {
    /// Enable sending and receiving files over authenticated trusted sessions.
    pub enabled: bool,
    /// Optional receive directory; the platform Downloads directory is used
    /// when unset.
    pub download_dir: Option<String>,
    /// Maximum aggregate bytes accepted for one transfer.
    pub max_transfer_bytes: u64,
    /// Maximum number of manifest entries accepted for one transfer.
    pub max_entries: usize,
}

impl Default for FileTransferConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            download_dir: None,
            max_transfer_bytes: 10 * 1024 * 1024 * 1024,
            max_entries: 1_024,
        }
    }
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
    /// Returns [`ConfigError`] on I/O failure (other than not-found), an unsafe
    /// file type, an oversized file, or a parse failure.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        match read_config_text(path.as_ref())? {
            Some(text) => Ok(toml::from_str(&text)?),
            None => Ok(Self::default()),
        }
    }

    /// Serialize and write configuration to `path`, creating parent dirs.
    ///
    /// # Errors
    /// Returns [`ConfigError`] on serialization or I/O failure, an unsafe
    /// target type, or an oversized serialized configuration.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), ConfigError> {
        let path = path.as_ref();
        if path.file_name().is_none() {
            return Err(ConfigError::UnsafePath);
        }
        let text = toml::to_string_pretty(self)?;
        if text.len() as u64 > MAX_CONFIG_BYTES {
            return Err(ConfigError::TooLarge {
                limit: MAX_CONFIG_BYTES,
            });
        }

        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        create_config_parent(parent)?;
        validate_config_target(path)?;

        let mut temporary = tempfile::Builder::new()
            .prefix(".nexkvm-config-")
            .suffix(".tmp")
            .tempfile_in(parent)?;
        harden_config_permissions(temporary.as_file())?;
        temporary.write_all(text.as_bytes())?;
        temporary.flush()?;
        temporary.as_file().sync_all()?;

        // Revalidate immediately before publication. Renaming a same-directory
        // temporary file never exposes a partially written configuration.
        validate_config_target(path)?;
        let persisted = temporary
            .persist(path)
            .map_err(|error| ConfigError::Io(error.error))?;
        persisted.sync_all()?;
        sync_config_parent(parent)?;
        Ok(())
    }
}

fn read_config_text(path: &Path) -> Result<Option<String>, ConfigError> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ConfigError::UnsafePath);
    }
    if metadata.len() > MAX_CONFIG_BYTES {
        return Err(ConfigError::TooLarge {
            limit: MAX_CONFIG_BYTES,
        });
    }

    let file = File::open(path)?;
    let opened_metadata = file.metadata()?;
    if !opened_metadata.is_file() {
        return Err(ConfigError::UnsafePath);
    }
    if opened_metadata.len() > MAX_CONFIG_BYTES {
        return Err(ConfigError::TooLarge {
            limit: MAX_CONFIG_BYTES,
        });
    }
    validate_config_target(path)?;

    let capacity = usize::try_from(opened_metadata.len().min(MAX_CONFIG_BYTES)).map_err(|_| {
        ConfigError::TooLarge {
            limit: MAX_CONFIG_BYTES,
        }
    })?;
    let mut bytes = Vec::with_capacity(capacity);
    file.take(MAX_CONFIG_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err(ConfigError::TooLarge {
            limit: MAX_CONFIG_BYTES,
        });
    }
    let text = String::from_utf8(bytes).map_err(|error| {
        ConfigError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    })?;
    Ok(Some(text))
}

fn validate_config_target(path: &Path) -> Result<(), ConfigError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(ConfigError::UnsafePath)
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
fn create_config_parent(parent: &Path) -> Result<(), std::io::Error> {
    use std::os::unix::fs::DirBuilderExt;

    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)
}

#[cfg(not(unix))]
fn create_config_parent(parent: &Path) -> Result<(), std::io::Error> {
    std::fs::create_dir_all(parent)
}

#[cfg(unix)]
fn harden_config_permissions(file: &File) -> Result<(), std::io::Error> {
    use std::os::unix::fs::PermissionsExt;

    file.set_permissions(std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn harden_config_permissions(_file: &File) -> Result<(), std::io::Error> {
    Ok(())
}

#[cfg(unix)]
fn sync_config_parent(parent: &Path) -> Result<(), std::io::Error> {
    File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_config_parent(_parent: &Path) -> Result<(), std::io::Error> {
    Ok(())
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
        assert_eq!(parsed.clipboard.sync_enabled, cfg.clipboard.sync_enabled);
    }

    #[test]
    fn network_defaults_to_the_only_supported_tcp_transport() {
        let cfg = Config::default();

        assert_eq!(cfg.network.transports, ["tcp"]);
    }

    #[test]
    fn clipboard_config_defaults_to_disabled() {
        let cfg = Config::default();
        assert!(!cfg.clipboard.sync_enabled);
    }

    #[test]
    fn clipboard_config_round_trips_through_toml() {
        let text = r#"
[clipboard]
sync_enabled = true
"#;

        let parsed: Config = toml::from_str(text).unwrap();
        assert!(parsed.clipboard.sync_enabled);

        let rendered = toml::to_string_pretty(&parsed).unwrap();
        assert!(rendered.contains("[clipboard]"));
        assert!(rendered.contains("sync_enabled = true"));
    }

    #[test]
    fn input_config_round_trips_through_toml() {
        let text = r#"
[input]
control_role = "source"
active_peer = "studio-mac"
handoff_edge = "right"
emergency_stop_keycode = 41
remote_focus_timeout_millis = 3000
"#;

        let parsed: Config = toml::from_str(text).unwrap();

        assert_eq!(parsed.input.control_role, InputControlRole::Source);
        assert_eq!(parsed.input.active_peer.as_deref(), Some("studio-mac"));
        assert_eq!(parsed.input.handoff_edge, InputHandoffEdge::Right);
        assert_eq!(parsed.input.emergency_stop_keycode, 41);
        assert_eq!(parsed.input.remote_focus_timeout_millis, 3_000);

        let rendered = toml::to_string_pretty(&parsed).unwrap();
        assert!(rendered.contains("[input]"));
        assert!(rendered.contains("control_role = \"source\""));
    }

    #[test]
    fn network_connect_addr_round_trips_through_toml() {
        let text = r#"
[network]
listen_port = 47654
enable_discovery = true
connect_addr = "192.168.1.27:47654"
transports = ["tcp"]
"#;

        let parsed: Config = toml::from_str(text).unwrap();

        assert_eq!(
            parsed.network.connect_addr.as_deref(),
            Some("192.168.1.27:47654")
        );

        let rendered = toml::to_string_pretty(&parsed).unwrap();
        assert!(rendered.contains("connect_addr = \"192.168.1.27:47654\""));
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

    #[test]
    fn oversized_config_is_rejected_before_reading_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_CONFIG_BYTES + 1).unwrap();

        assert!(matches!(
            Config::load(&path),
            Err(ConfigError::TooLarge { .. })
        ));
    }

    #[test]
    fn oversized_save_does_not_replace_existing_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut initial = Config::default();
        initial.device.name = "Old Device".into();
        initial.save(&path).unwrap();

        let mut oversized = Config::default();
        oversized.device.name = "x".repeat(MAX_CONFIG_BYTES as usize + 1);
        assert!(matches!(
            oversized.save(&path),
            Err(ConfigError::TooLarge { .. })
        ));
        assert_eq!(Config::load(path).unwrap().device.name, "Old Device");
    }

    #[test]
    fn non_regular_config_target_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::create_dir(&path).unwrap();

        assert!(matches!(Config::load(&path), Err(ConfigError::UnsafePath)));
        assert!(matches!(
            Config::default().save(&path),
            Err(ConfigError::UnsafePath)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_config_target_is_rejected_without_touching_its_target() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.toml");
        std::fs::write(&target, b"unchanged").unwrap();
        let path = dir.path().join("config.toml");
        symlink(&target, &path).unwrap();

        assert!(matches!(Config::load(&path), Err(ConfigError::UnsafePath)));
        assert!(matches!(
            Config::default().save(&path),
            Err(ConfigError::UnsafePath)
        ));
        assert_eq!(std::fs::read(target).unwrap(), b"unchanged");
    }

    #[cfg(unix)]
    #[test]
    fn save_creates_owner_only_config_file_and_directories() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("nested").join("nexkvm");
        let path = config_dir.join("config.toml");

        Config::default().save(&path).unwrap();

        assert_eq!(
            std::fs::metadata(dir.path().join("nested"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&config_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn save_atomically_replaces_an_existing_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let old_view = dir.path().join("old-config.toml");
        let mut initial = Config::default();
        initial.device.name = "Old Device".into();
        initial.save(&path).unwrap();
        std::fs::hard_link(&path, &old_view).unwrap();

        let mut replacement = Config::default();
        replacement.device.name = "New Device".into();
        replacement.save(&path).unwrap();

        assert_eq!(Config::load(&path).unwrap().device.name, "New Device");
        assert_eq!(Config::load(&old_view).unwrap().device.name, "Old Device");
    }
}
