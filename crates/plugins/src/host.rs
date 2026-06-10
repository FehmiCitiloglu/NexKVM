//! Brokered host-call boundary for sandboxed plugins.
//!
//! Plugins (especially WASM/WASI guests) never touch OS, clipboard, input, or
//! network surfaces directly. Every side effect is a [`HostCall`] the guest asks
//! the host to perform, and the host validates it against the plugin's granted
//! [`PluginCapabilities`] *before* acting. The [`HostBroker`] is that single
//! choke point: under the WASM runtime each variant maps to a host import
//! function, so the same enforcement covers native and sandboxed plugins alike.
//!
//! This module is intentionally engine-agnostic. The actual `wasmtime`
//! integration (behind the `runtime-wasm` feature) wires its host imports to
//! [`HostBroker::authorize`] without changing any policy here.

use nexkvm_protocol::MessageKind;

use crate::capability::PluginCapabilities;
use crate::error::PluginError;

/// A side-effecting operation a sandboxed plugin asks the host to perform.
///
/// Each variant requires a specific capability; the host denies any call the
/// plugin was not granted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum HostCall {
    /// Read the current clipboard contents.
    ReadClipboard,
    /// Replace the clipboard contents.
    WriteClipboard,
    /// Observe input events (pointer/key/scroll).
    ObserveInput,
    /// Synthesize input events.
    InjectInput,
    /// Send a message to a connected peer.
    SendToPeer,
    /// Make an outbound request to an external service (e.g. an AI API).
    ExternalRequest,
    /// Read plugin-local persistent storage.
    ReadStorage,
    /// Write plugin-local persistent storage.
    WriteStorage,
    /// Read device/discovery metadata.
    DeviceMetadata,
    /// Issue audio/streaming control.
    AudioControl,
}

impl HostCall {
    /// Human-readable capability name this call requires (used in errors).
    #[must_use]
    pub const fn required_capability(self) -> &'static str {
        match self {
            Self::ReadClipboard => "read_clipboard",
            Self::WriteClipboard => "write_clipboard",
            Self::ObserveInput => "read_input",
            Self::InjectInput => "inject_input",
            Self::SendToPeer => "network_send",
            Self::ExternalRequest => "network_external",
            Self::ReadStorage | Self::WriteStorage => "storage",
            Self::DeviceMetadata => "device_metadata",
            Self::AudioControl => "audio_control",
        }
    }

    /// Whether `granted` permits this call.
    #[must_use]
    pub const fn is_permitted(self, granted: &PluginCapabilities) -> bool {
        match self {
            Self::ReadClipboard => granted.read_clipboard,
            Self::WriteClipboard => granted.write_clipboard,
            Self::ObserveInput => granted.read_input,
            Self::InjectInput => granted.inject_input,
            Self::SendToPeer => granted.network_send,
            Self::ExternalRequest => granted.network_external,
            Self::ReadStorage | Self::WriteStorage => granted.storage,
            Self::DeviceMetadata => granted.device_metadata,
            Self::AudioControl => granted.audio_control,
        }
    }
}

/// The plugin-observable category a wire message maps to.
///
/// Used to gate event-hook delivery: a plugin only receives `Inbound`/`Outbound`
/// events whose hook its capabilities cover, so clipboard, input, and network
/// hooks are enforced independently rather than as one coarse grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum EventHook {
    /// Clipboard synchronization traffic.
    Clipboard,
    /// Input event traffic.
    Input,
    /// File / drag-and-drop transfer traffic.
    FileTransfer,
    /// Peer/network control and relay traffic.
    Network,
    /// Audio/media stream control traffic.
    Stream,
}

impl EventHook {
    /// Classify a wire [`MessageKind`] into a plugin-observable hook, or `None`
    /// for message kinds plugins never observe (handshake, pairing, control,
    /// and host-internal payloads).
    #[must_use]
    pub const fn for_message(kind: MessageKind) -> Option<Self> {
        match kind {
            MessageKind::Clipboard => Some(Self::Clipboard),
            MessageKind::Input => Some(Self::Input),
            MessageKind::FileTransfer => Some(Self::FileTransfer),
            MessageKind::Heartbeat
            | MessageKind::Discovery
            | MessageKind::Mesh
            | MessageKind::Relay
            | MessageKind::BrowserSession => Some(Self::Network),
            MessageKind::Stream => Some(Self::Stream),
            // Handshake, pairing, host-internal, and control payloads are not
            // plugin-observable. The wildcard also covers any future kind so
            // new wire messages default to *not* exposed (fail-closed).
            _ => None,
        }
    }

    /// Whether `granted` permits observing this hook.
    #[must_use]
    pub const fn is_permitted(self, granted: &PluginCapabilities) -> bool {
        match self {
            Self::Clipboard => granted.read_clipboard,
            Self::Input => granted.read_input,
            Self::FileTransfer => granted.read_clipboard,
            Self::Network => granted.network_send,
            Self::Stream => granted.audio_control,
        }
    }
}

/// Capability-enforcing gateway between a sandboxed plugin and the host.
///
/// Construct one per loaded plugin from its granted capabilities. Every host
/// call a plugin makes passes through [`authorize`](HostBroker::authorize),
/// which returns [`PluginError::PermissionDenied`] for any ungranted surface.
#[derive(Debug, Clone)]
pub struct HostBroker {
    plugin_id: String,
    granted: PluginCapabilities,
}

impl HostBroker {
    /// Create a broker for `plugin_id` with the capabilities it was granted.
    #[must_use]
    pub fn new(plugin_id: impl Into<String>, granted: PluginCapabilities) -> Self {
        Self {
            plugin_id: plugin_id.into(),
            granted,
        }
    }

    /// The capabilities this broker enforces.
    #[must_use]
    pub const fn granted(&self) -> &PluginCapabilities {
        &self.granted
    }

    /// Authorize a host call.
    ///
    /// # Errors
    /// Returns [`PluginError::PermissionDenied`] if the plugin lacks the
    /// capability the call requires.
    pub fn authorize(&self, call: HostCall) -> Result<(), PluginError> {
        if call.is_permitted(&self.granted) {
            Ok(())
        } else {
            Err(PluginError::PermissionDenied {
                plugin: self.plugin_id.clone(),
                capability: call.required_capability(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps_with_clipboard() -> PluginCapabilities {
        PluginCapabilities {
            read_clipboard: true,
            ..PluginCapabilities::none()
        }
    }

    #[test]
    fn broker_authorizes_granted_call() {
        let broker = HostBroker::new("dev.nexkvm.test", caps_with_clipboard());
        assert!(broker.authorize(HostCall::ReadClipboard).is_ok());
    }

    #[test]
    fn broker_denies_ungranted_call() {
        let broker = HostBroker::new("dev.nexkvm.test", caps_with_clipboard());
        let err = broker.authorize(HostCall::WriteClipboard).unwrap_err();
        match err {
            PluginError::PermissionDenied { plugin, capability } => {
                assert_eq!(plugin, "dev.nexkvm.test");
                assert_eq!(capability, "write_clipboard");
            }
            other => panic!("expected PermissionDenied, got {other:?}"),
        }
    }

    #[test]
    fn host_call_capability_names_are_stable() {
        assert_eq!(HostCall::ReadStorage.required_capability(), "storage");
        assert_eq!(HostCall::WriteStorage.required_capability(), "storage");
        assert_eq!(HostCall::InjectInput.required_capability(), "inject_input");
    }

    #[test]
    fn clipboard_hook_requires_clipboard_capability() {
        let hook = EventHook::for_message(MessageKind::Clipboard).unwrap();
        assert!(hook.is_permitted(&caps_with_clipboard()));
        assert!(!hook.is_permitted(&PluginCapabilities::none()));
    }

    #[test]
    fn network_only_plugin_cannot_observe_clipboard_or_input() {
        let net_only = PluginCapabilities {
            network_send: true,
            ..PluginCapabilities::none()
        };
        let clipboard = EventHook::for_message(MessageKind::Clipboard).unwrap();
        let input = EventHook::for_message(MessageKind::Input).unwrap();
        let network = EventHook::for_message(MessageKind::Heartbeat).unwrap();
        assert!(!clipboard.is_permitted(&net_only));
        assert!(!input.is_permitted(&net_only));
        assert!(network.is_permitted(&net_only));
    }

    #[test]
    fn control_messages_have_no_plugin_hook() {
        assert!(EventHook::for_message(MessageKind::Control).is_none());
        assert!(EventHook::for_message(MessageKind::Handshake).is_none());
        assert!(EventHook::for_message(MessageKind::Plugin).is_none());
    }
}
