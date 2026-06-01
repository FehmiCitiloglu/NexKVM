//! Plugin manifest metadata.

use serde::{Deserialize, Serialize};

use crate::capability::PluginCapabilities;
use crate::runtime::PluginRuntimeKind;

/// Static metadata describing a plugin, declared by the author and surfaced to
/// the user for approval before the plugin is granted its capabilities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginManifest {
    /// Unique, stable plugin identifier (reverse-DNS recommended).
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Plugin semantic version.
    pub version: String,
    /// Short description shown in the UI.
    #[serde(default)]
    pub description: String,
    /// Runtime needed to execute this plugin.
    #[serde(default = "default_runtime")]
    pub runtime: PluginRuntimeKind,
    /// Runtime-specific entrypoint (e.g. `plugin.wasm`, `main.lua`).
    #[serde(default)]
    pub entrypoint: String,
    /// Capabilities the plugin requires to function.
    #[serde(default)]
    pub required_capabilities: PluginCapabilities,
}

fn default_runtime() -> PluginRuntimeKind {
    PluginRuntimeKind::Native
}
