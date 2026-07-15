//! Least-privilege capability model for plugins.

use serde::{Deserialize, Serialize};

/// The set of host surfaces a plugin is permitted to access.
///
/// Declared in the [`PluginManifest`](crate::PluginManifest) and approved by the
/// user. The host consults these flags before dispatching sensitive events, so
/// a plugin only ever sees what it was granted (least privilege).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginCapabilities {
    /// Observe clipboard contents.
    pub read_clipboard: bool,
    /// Observe file and drag-and-drop transfer traffic.
    #[serde(default)]
    pub read_file_transfer: bool,
    /// Modify clipboard contents.
    pub write_clipboard: bool,
    /// Observe input events.
    pub read_input: bool,
    /// Synthesize input events.
    pub inject_input: bool,
    /// Send messages to connected peers.
    pub network_send: bool,
    /// Make outbound network requests (e.g. an AI action calling an API).
    pub network_external: bool,
    /// Access plugin-local persistent storage.
    pub storage: bool,
    /// Read device/discovery metadata.
    pub device_metadata: bool,
    /// Subscribe to audio/streaming control events.
    pub audio_control: bool,
}

impl PluginCapabilities {
    /// No permissions — the safe default for an untrusted plugin.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            read_clipboard: false,
            read_file_transfer: false,
            write_clipboard: false,
            read_input: false,
            inject_input: false,
            network_send: false,
            network_external: false,
            storage: false,
            device_metadata: false,
            audio_control: false,
        }
    }

    /// Whether `self` grants at least everything `required` asks for.
    #[must_use]
    pub fn satisfies(&self, required: &PluginCapabilities) -> bool {
        (!required.read_clipboard || self.read_clipboard)
            && (!required.read_file_transfer || self.read_file_transfer)
            && (!required.write_clipboard || self.write_clipboard)
            && (!required.read_input || self.read_input)
            && (!required.inject_input || self.inject_input)
            && (!required.network_send || self.network_send)
            && (!required.network_external || self.network_external)
            && (!required.storage || self.storage)
            && (!required.device_metadata || self.device_metadata)
            && (!required.audio_control || self.audio_control)
    }
}
