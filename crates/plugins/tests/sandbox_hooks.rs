//! End-to-end plugin sandbox enforcement: a least-privilege plugin observes the
//! event hooks it was granted, is denied the rest, and the host-call broker
//! gates every side effect — all through the public API.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use coklu_core::{DeviceId, Event};
use coklu_plugins::{
    HostBroker, HostCall, Plugin, PluginCapabilities, PluginContext, PluginError, PluginManifest,
    PluginRegistry, PluginRuntimeKind,
};
use coklu_protocol::MessageKind;

#[derive(Debug)]
struct CountingPlugin {
    manifest: PluginManifest,
    seen: Arc<AtomicUsize>,
}

impl CountingPlugin {
    fn new(required: PluginCapabilities) -> Self {
        Self {
            manifest: PluginManifest {
                id: "dev.coklu.sandbox".into(),
                name: "Sandbox".into(),
                version: "1.0.0".into(),
                description: String::new(),
                runtime: PluginRuntimeKind::Wasm,
                entrypoint: "plugin.wasm".into(),
                required_capabilities: required,
            },
            seen: Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[async_trait]
impl Plugin for CountingPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn on_load(&self, _ctx: &PluginContext) -> Result<(), PluginError> {
        Ok(())
    }

    async fn on_event(&self, _ctx: &PluginContext, _event: &Event) -> Result<(), PluginError> {
        self.seen.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn inbound(kind: MessageKind) -> Event {
    Event::Inbound {
        from: DeviceId::generate(),
        kind,
        payload: Bytes::from_static(b"x"),
    }
}

/// A clipboard-only plugin receives clipboard hooks but never input or network.
#[tokio::test]
async fn clipboard_plugin_only_sees_clipboard_hooks() {
    let plugin = Arc::new(CountingPlugin::new(PluginCapabilities {
        read_clipboard: true,
        ..PluginCapabilities::none()
    }));
    let seen = plugin.seen.clone();

    let mut registry = PluginRegistry::new();
    registry
        .register(
            plugin,
            PluginCapabilities {
                read_clipboard: true,
                ..PluginCapabilities::none()
            },
        )
        .await
        .expect("grant satisfies requirement");

    registry.dispatch(&inbound(MessageKind::Clipboard)).await;
    registry.dispatch(&inbound(MessageKind::Input)).await;
    registry.dispatch(&inbound(MessageKind::Heartbeat)).await;

    assert_eq!(
        seen.load(Ordering::SeqCst),
        1,
        "only the clipboard event should reach a clipboard-only plugin"
    );
}

/// The host-call broker authorizes granted surfaces and denies the rest.
#[test]
fn host_broker_enforces_least_privilege() {
    let broker = HostBroker::new(
        "dev.coklu.sandbox",
        PluginCapabilities {
            read_clipboard: true,
            network_external: true,
            ..PluginCapabilities::none()
        },
    );

    assert!(broker.authorize(HostCall::ReadClipboard).is_ok());
    assert!(broker.authorize(HostCall::ExternalRequest).is_ok());

    let denied = broker.authorize(HostCall::InjectInput).unwrap_err();
    assert!(matches!(
        denied,
        PluginError::PermissionDenied { capability, .. } if capability == "inject_input"
    ));
}
