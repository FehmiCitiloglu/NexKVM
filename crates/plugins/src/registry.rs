//! Plugin registry: loads plugins under capability enforcement.

use std::collections::HashMap;
use std::sync::Arc;

use coklu_core::Event;

use crate::capability::PluginCapabilities;
use crate::error::PluginError;
use crate::plugin::{Plugin, PluginContext};

struct Loaded {
    plugin: Arc<dyn Plugin>,
    ctx: PluginContext,
}

impl std::fmt::Debug for Loaded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Loaded")
            .field("id", &self.plugin.manifest().id)
            .field("granted", &self.ctx.granted)
            .finish()
    }
}

/// Holds loaded plugins and dispatches events to them, enforcing the
/// capability grant negotiated at load time.
#[derive(Debug, Default)]
pub struct PluginRegistry {
    plugins: HashMap<String, Loaded>,
}

impl PluginRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register and initialize a plugin.
    ///
    /// `granted` must satisfy the plugin's declared
    /// [`required_capabilities`](crate::PluginManifest::required_capabilities);
    /// otherwise registration is denied. This is the single choke point where
    /// the user's permission decision is enforced.
    ///
    /// # Errors
    /// - [`PluginError::AlreadyLoaded`] if the id is already registered.
    /// - [`PluginError::PermissionDenied`] if `granted` is insufficient.
    /// - [`PluginError::LoadFailed`] if the plugin's `on_load` fails.
    pub async fn register(
        &mut self,
        plugin: Arc<dyn Plugin>,
        granted: PluginCapabilities,
    ) -> Result<(), PluginError> {
        let id = plugin.manifest().id.clone();
        if self.plugins.contains_key(&id) {
            return Err(PluginError::AlreadyLoaded(id));
        }

        let required = plugin.manifest().required_capabilities;
        if !granted.satisfies(&required) {
            return Err(PluginError::PermissionDenied {
                plugin: id,
                capability: "required_capabilities",
            });
        }

        let ctx = PluginContext::new(granted);
        plugin.on_load(&ctx).await?;
        self.plugins.insert(id, Loaded { plugin, ctx });
        Ok(())
    }

    /// Dispatch an event to every loaded plugin.
    ///
    /// A failing plugin is reported but does not abort dispatch to the others;
    /// callers typically log returned errors.
    pub async fn dispatch(&self, event: &Event) -> Vec<PluginError> {
        let mut errors = Vec::new();
        for loaded in self.plugins.values() {
            if !can_receive_event(&loaded.ctx.granted, event) {
                continue;
            }
            if let Err(e) = loaded.plugin.on_event(&loaded.ctx, event).await {
                errors.push(e);
            }
        }
        errors
    }

    /// Unload a plugin by id.
    ///
    /// # Errors
    /// Returns [`PluginError::NotLoaded`] if no such plugin exists.
    pub async fn unload(&mut self, id: &str) -> Result<(), PluginError> {
        let loaded = self
            .plugins
            .remove(id)
            .ok_or_else(|| PluginError::NotLoaded(id.into()))?;
        loaded.plugin.on_unload().await;
        Ok(())
    }

    /// Reload a plugin by unloading the existing instance and registering the
    /// replacement under the same permission grant.
    ///
    /// # Errors
    /// Returns [`PluginError`] if the existing plugin is absent, the id changes,
    /// permissions are insufficient, or load fails.
    pub async fn reload(&mut self, plugin: Arc<dyn Plugin>) -> Result<(), PluginError> {
        let id = plugin.manifest().id.clone();
        let granted = self
            .plugins
            .get(&id)
            .map(|loaded| loaded.ctx.granted)
            .ok_or_else(|| PluginError::NotLoaded(id.clone()))?;
        self.unload(&id).await?;
        self.register(plugin, granted).await
    }

    /// Number of loaded plugins.
    #[must_use]
    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    /// Whether no plugins are loaded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }
}

fn can_receive_event(granted: &PluginCapabilities, event: &Event) -> bool {
    match event {
        Event::Inbound { .. } | Event::Outbound { .. } => granted.network_send,
        Event::DeviceDiscovered(_) | Event::DeviceConnected(_) | Event::DeviceDisconnected(_) => {
            granted.device_metadata
        }
        Event::Shutdown => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use coklu_core::DeviceInfo;

    use super::*;
    use crate::{PluginManifest, PluginRuntimeKind};

    #[derive(Debug)]
    struct TestPlugin {
        manifest: PluginManifest,
        loads: Arc<AtomicUsize>,
        events: Arc<AtomicUsize>,
        unloads: Arc<AtomicUsize>,
    }

    impl TestPlugin {
        fn new(required: PluginCapabilities) -> Self {
            Self {
                manifest: PluginManifest {
                    id: "dev.coklu.test".into(),
                    name: "Test".into(),
                    version: "1.0.0".into(),
                    description: String::new(),
                    runtime: PluginRuntimeKind::Native,
                    entrypoint: String::new(),
                    required_capabilities: required,
                },
                loads: Arc::new(AtomicUsize::new(0)),
                events: Arc::new(AtomicUsize::new(0)),
                unloads: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    #[async_trait]
    impl Plugin for TestPlugin {
        fn manifest(&self) -> &PluginManifest {
            &self.manifest
        }

        async fn on_load(&self, _ctx: &PluginContext) -> Result<(), PluginError> {
            self.loads.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn on_event(&self, _ctx: &PluginContext, _event: &Event) -> Result<(), PluginError> {
            self.events.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn on_unload(&self) {
            self.unloads.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn register_denies_insufficient_capabilities() {
        let mut registry = PluginRegistry::new();
        let plugin = Arc::new(TestPlugin::new(PluginCapabilities {
            read_input: true,
            ..PluginCapabilities::none()
        }));
        let err = registry
            .register(plugin, PluginCapabilities::none())
            .await
            .unwrap_err();
        assert!(matches!(err, PluginError::PermissionDenied { .. }));
        assert!(registry.is_empty());
    }

    #[tokio::test]
    async fn dispatch_filters_device_events_without_metadata_capability() {
        let mut registry = PluginRegistry::new();
        let plugin = Arc::new(TestPlugin::new(PluginCapabilities::none()));
        let events = plugin.events.clone();
        registry
            .register(plugin, PluginCapabilities::none())
            .await
            .unwrap();

        registry
            .dispatch(&Event::DeviceDiscovered(DeviceInfo::new(
                "peer",
                coklu_core::OsKind::Linux,
            )))
            .await;
        registry.dispatch(&Event::Shutdown).await;

        assert_eq!(events.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn unload_calls_plugin_teardown() {
        let mut registry = PluginRegistry::new();
        let plugin = Arc::new(TestPlugin::new(PluginCapabilities::none()));
        let unloads = plugin.unloads.clone();
        registry
            .register(plugin, PluginCapabilities::none())
            .await
            .unwrap();
        registry.unload("dev.coklu.test").await.unwrap();
        assert_eq!(unloads.load(Ordering::SeqCst), 1);
        assert!(registry.is_empty());
    }

    #[tokio::test]
    async fn reload_reuses_existing_grant() {
        let mut registry = PluginRegistry::new();
        let first = Arc::new(TestPlugin::new(PluginCapabilities::none()));
        registry
            .register(first, PluginCapabilities::none())
            .await
            .unwrap();
        let second = Arc::new(TestPlugin::new(PluginCapabilities::none()));
        let loads = second.loads.clone();
        registry.reload(second).await.unwrap();
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        assert_eq!(registry.len(), 1);
    }
}
