//! Plugin API & runtime model.
//!
//! # Architecture
//! Plugins extend coklu (AI clipboard actions, custom sync handlers, automation)
//! without touching core. Two key boundaries:
//!
//! - [`Plugin`] — the lifecycle + event-handling trait every plugin implements.
//!   It is intentionally event-driven: plugins observe and emit
//!   [`coklu_core::Event`]s rather than calling subsystems directly, preserving
//!   the decoupled bus architecture and making permissions enforceable.
//! - [`PluginCapabilities`] — an explicit, least-privilege permission grant. A
//!   plugin can only touch the surfaces it declared in its [`PluginManifest`]
//!   and the user approved. The host checks capabilities before dispatching
//!   sensitive events.
//!
//! # Sandboxing (phased)
//! The foundation defines the *trait + permission model*, runtime descriptors,
//! sandbox policy, marketplace metadata, and hot-reload state. The intended
//! third-party runtime is **WebAssembly (WASM/WASI)**; Lua is supported as a
//! scripting runtime behind the same brokered host-call boundary. Native
//! in-process plugins are reserved for first-party, trusted code.

mod capability;
mod error;
mod hot_reload;
mod manifest;
mod marketplace;
mod plugin;
mod registry;
mod runtime;

pub use capability::PluginCapabilities;
pub use error::PluginError;
pub use hot_reload::{HotReloadTracker, PluginArtifactState, ReloadDecision};
pub use manifest::PluginManifest;
pub use marketplace::{MarketplaceCatalog, MarketplaceListing, MarketplaceTrust, PluginArtifact};
pub use plugin::{Plugin, PluginContext};
pub use registry::PluginRegistry;
pub use runtime::{
    PluginLoadRequest, PluginRuntime, PluginRuntimeKind, PluginSandbox, ResourceLimits,
    RuntimeDescriptor, SandboxLevel, runtime_for_manifest,
};
