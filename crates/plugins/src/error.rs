//! Plugin runtime errors.

use thiserror::Error;

/// Errors from loading or running a plugin.
#[derive(Debug, Error)]
pub enum PluginError {
    /// A plugin attempted an action it was not granted permission for.
    #[error("plugin '{plugin}' lacks capability: {capability}")]
    PermissionDenied {
        /// Offending plugin id.
        plugin: String,
        /// The capability that was required.
        capability: &'static str,
    },

    /// A plugin with the same id is already registered.
    #[error("plugin '{0}' is already loaded")]
    AlreadyLoaded(String),

    /// The plugin's `on_load` initialization failed.
    #[error("plugin '{plugin}' failed to load: {reason}")]
    LoadFailed {
        /// Plugin id.
        plugin: String,
        /// Failure detail.
        reason: String,
    },

    /// A plugin returned an error while handling an event.
    #[error("plugin '{plugin}' runtime error: {reason}")]
    Runtime {
        /// Plugin id.
        plugin: String,
        /// Failure detail.
        reason: String,
    },

    /// The requested runtime backend is not available in this build.
    #[error("plugin runtime unavailable: {0}")]
    RuntimeUnavailable(&'static str),

    /// A plugin or runtime attempted to escape its sandbox policy.
    #[error("plugin '{plugin}' sandbox violation: {reason}")]
    SandboxViolation {
        /// Plugin id.
        plugin: String,
        /// Failure detail.
        reason: String,
    },

    /// Marketplace listing failed policy validation.
    #[error("plugin '{plugin}' marketplace policy violation: {reason}")]
    MarketplacePolicy {
        /// Plugin id.
        plugin: String,
        /// Failure detail.
        reason: String,
    },

    /// Plugin id is not currently loaded.
    #[error("plugin '{0}' is not loaded")]
    NotLoaded(String),
}
