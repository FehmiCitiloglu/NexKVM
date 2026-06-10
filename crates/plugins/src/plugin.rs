//! The plugin trait and its host-provided context.

use async_trait::async_trait;
use nexkvm_core::Event;

use crate::capability::PluginCapabilities;
use crate::error::PluginError;
use crate::manifest::PluginManifest;

/// Host-provided handle a plugin uses to interact with nexkvm.
///
/// All side effects are mediated here so the host can enforce
/// [`PluginCapabilities`] and, under the WASM backend, marshal calls across the
/// sandbox boundary. Plugins emit events rather than calling subsystems
/// directly, preserving the decoupled bus design.
#[derive(Debug)]
pub struct PluginContext {
    /// Capabilities granted to the plugin (a subset of what it requested).
    pub granted: PluginCapabilities,
    // Future: a capability-gated emit handle into the core EventBus and a
    // brokered host-call table for the WASM runtime.
}

impl PluginContext {
    /// Construct a context with the given granted capabilities.
    #[must_use]
    pub fn new(granted: PluginCapabilities) -> Self {
        Self { granted }
    }
}

/// A nexkvm plugin.
///
/// Lifecycle: [`on_load`](Plugin::on_load) once at registration, then
/// [`on_event`](Plugin::on_event) for each relevant event, then
/// [`on_unload`](Plugin::on_unload) at teardown. Implementations must be
/// `Send + Sync` and must not block the async runtime.
#[async_trait]
pub trait Plugin: Send + Sync {
    /// The plugin's manifest (id, version, required capabilities).
    fn manifest(&self) -> &PluginManifest;

    /// Initialize the plugin. Called once after capabilities are granted.
    ///
    /// # Errors
    /// Return [`PluginError::LoadFailed`] to abort registration.
    async fn on_load(&self, ctx: &PluginContext) -> Result<(), PluginError>;

    /// Handle an observed event. Capability checks are applied by the host
    /// before dispatch, so a plugin only receives events it is permitted to see.
    ///
    /// # Errors
    /// Return [`PluginError::Runtime`] on a recoverable failure; the host logs
    /// and continues dispatching to other plugins.
    async fn on_event(&self, ctx: &PluginContext, event: &Event) -> Result<(), PluginError>;

    /// Tear down the plugin. Called once at shutdown or unload.
    async fn on_unload(&self) {}
}
