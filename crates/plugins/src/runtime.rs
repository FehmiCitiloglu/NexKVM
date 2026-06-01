//! Plugin runtime and sandbox model.
//!
//! Runtime engines (WASM/WASI, Lua, trusted native) all satisfy the same host
//! contract. This module is intentionally sans-engine: it describes how plugins
//! are isolated, what resource limits apply, and which backend kind should load
//! a manifest. Actual `wasmtime`/Lua integration can land behind feature flags
//! without changing host policy.

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{PluginError, PluginManifest};

/// Supported plugin runtime families.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PluginRuntimeKind {
    /// Trusted, in-process Rust plugin. First-party only.
    Native,
    /// WebAssembly/WASI sandbox.
    Wasm,
    /// Lua script sandbox.
    Lua,
}

/// Filesystem/network isolation profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SandboxLevel {
    /// No ambient OS access. Host calls only.
    Strict,
    /// Host calls plus explicitly granted virtual resources.
    Brokered,
    /// Trusted in-process access. Reserved for first-party plugins.
    TrustedNative,
}

/// Resource limits enforced by runtime backends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceLimits {
    /// Maximum linear memory/heap bytes.
    pub max_memory_bytes: u64,
    /// Maximum CPU time per event callback.
    pub max_callback_time: Duration,
    /// Maximum queued host-call responses.
    pub max_pending_host_calls: usize,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_memory_bytes: 64 * 1024 * 1024,
            max_callback_time: Duration::from_millis(50),
            max_pending_host_calls: 64,
        }
    }
}

/// Sandbox configuration derived from manifest + user policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginSandbox {
    /// Isolation level.
    pub level: SandboxLevel,
    /// Runtime resource limits.
    pub limits: ResourceLimits,
    /// Whether outbound network access is brokered through host policy.
    pub broker_network: bool,
    /// Whether filesystem access is fully denied.
    pub deny_filesystem: bool,
}

impl PluginSandbox {
    /// Strict default for untrusted third-party plugins.
    #[must_use]
    pub fn strict() -> Self {
        Self {
            level: SandboxLevel::Strict,
            limits: ResourceLimits::default(),
            broker_network: true,
            deny_filesystem: true,
        }
    }

    /// Trusted native profile for first-party plugins only.
    #[must_use]
    pub fn trusted_native() -> Self {
        Self {
            level: SandboxLevel::TrustedNative,
            limits: ResourceLimits {
                max_memory_bytes: 512 * 1024 * 1024,
                max_callback_time: Duration::from_millis(250),
                max_pending_host_calls: 1024,
            },
            broker_network: false,
            deny_filesystem: false,
        }
    }

    /// Whether this sandbox is suitable for untrusted marketplace code.
    #[must_use]
    pub const fn is_marketplace_safe(&self) -> bool {
        matches!(self.level, SandboxLevel::Strict | SandboxLevel::Brokered)
            && self.broker_network
            && self.deny_filesystem
    }
}

/// Load request passed to a runtime backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginLoadRequest {
    /// Manifest to instantiate.
    pub manifest: PluginManifest,
    /// Sandbox policy to enforce.
    pub sandbox: PluginSandbox,
    /// Runtime-specific artifact bytes/path identifier.
    pub artifact: String,
}

/// Runtime backend metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeDescriptor {
    /// Backend kind.
    pub kind: PluginRuntimeKind,
    /// Whether this runtime is compiled into this build.
    pub available: bool,
    /// Default sandbox for this backend.
    pub default_sandbox: SandboxLevel,
}

impl RuntimeDescriptor {
    /// Descriptor for WASM support.
    #[must_use]
    pub const fn wasm() -> Self {
        Self {
            kind: PluginRuntimeKind::Wasm,
            available: cfg!(feature = "runtime-wasm"),
            default_sandbox: SandboxLevel::Strict,
        }
    }

    /// Descriptor for Lua support.
    #[must_use]
    pub const fn lua() -> Self {
        Self {
            kind: PluginRuntimeKind::Lua,
            available: cfg!(feature = "runtime-lua"),
            default_sandbox: SandboxLevel::Strict,
        }
    }

    /// Descriptor for native support.
    #[must_use]
    pub const fn native() -> Self {
        Self {
            kind: PluginRuntimeKind::Native,
            available: true,
            default_sandbox: SandboxLevel::TrustedNative,
        }
    }
}

/// Plugin runtime backend boundary.
#[async_trait]
pub trait PluginRuntime: Send + Sync {
    /// Runtime kind.
    fn kind(&self) -> PluginRuntimeKind;

    /// Instantiate a plugin artifact.
    ///
    /// # Errors
    /// Returns [`PluginError::RuntimeUnavailable`] if the backend is not
    /// compiled/available, or [`PluginError::SandboxViolation`] if policy is
    /// impossible to enforce.
    async fn load(&self, request: PluginLoadRequest) -> Result<(), PluginError>;

    /// Unload a plugin by id.
    async fn unload(&self, plugin_id: &str) -> Result<(), PluginError>;
}

/// Select the expected runtime from a manifest.
#[must_use]
pub fn runtime_for_manifest(manifest: &PluginManifest) -> PluginRuntimeKind {
    manifest.runtime
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_sandbox_is_marketplace_safe() {
        assert!(PluginSandbox::strict().is_marketplace_safe());
        assert!(!PluginSandbox::trusted_native().is_marketplace_safe());
    }

    #[test]
    fn runtime_descriptors_reflect_feature_flags() {
        assert_eq!(RuntimeDescriptor::wasm().kind, PluginRuntimeKind::Wasm);
        assert_eq!(RuntimeDescriptor::lua().kind, PluginRuntimeKind::Lua);
        assert!(RuntimeDescriptor::native().available);
    }
}
