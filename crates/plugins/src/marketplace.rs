//! Plugin marketplace architecture.
//!
//! The marketplace is a signed metadata catalog, not an execution path. Records
//! describe where to fetch artifacts, which runtime they require, and what
//! permissions they request. Trust still comes from signature verification and
//! explicit user approval before the registry grants capabilities.

use serde::{Deserialize, Serialize};

use crate::{PluginCapabilities, PluginManifest, PluginRuntimeKind};

/// Trust state for a marketplace listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MarketplaceTrust {
    /// Signed by an approved publisher key.
    VerifiedPublisher,
    /// Community listing without publisher verification.
    Community,
    /// Known bad or policy-blocked listing.
    Blocked,
}

/// One downloadable plugin artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginArtifact {
    /// Runtime this artifact targets.
    pub runtime: PluginRuntimeKind,
    /// Download URL or registry object key.
    pub url: String,
    /// Hex/base64 digest string of artifact bytes.
    pub sha256: String,
    /// Optional detached signature URL/key.
    pub signature: Option<String>,
}

/// Marketplace listing metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketplaceListing {
    /// Manifest shown for install approval.
    pub manifest: PluginManifest,
    /// Publisher id/name.
    pub publisher: String,
    /// Marketplace trust level.
    pub trust: MarketplaceTrust,
    /// Available artifacts.
    pub artifacts: Vec<PluginArtifact>,
    /// Capabilities approved by marketplace policy; user approval may grant less.
    pub policy_cap: PluginCapabilities,
}

impl MarketplaceListing {
    /// Whether this listing may be installed from the marketplace.
    #[must_use]
    pub fn installable(&self) -> bool {
        self.trust != MarketplaceTrust::Blocked
            && self
                .policy_cap
                .satisfies(&self.manifest.required_capabilities)
            && self
                .artifacts
                .iter()
                .any(|artifact| artifact.runtime == self.manifest.runtime)
    }

    /// Best artifact for the manifest runtime.
    #[must_use]
    pub fn artifact_for_runtime(&self) -> Option<&PluginArtifact> {
        self.artifacts
            .iter()
            .find(|artifact| artifact.runtime == self.manifest.runtime)
    }
}

/// Marketplace catalog snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketplaceCatalog {
    listings: Vec<MarketplaceListing>,
}

impl MarketplaceCatalog {
    /// Construct an empty catalog.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add or replace a listing by plugin id.
    pub fn upsert(&mut self, listing: MarketplaceListing) {
        if let Some(existing) = self
            .listings
            .iter_mut()
            .find(|item| item.manifest.id == listing.manifest.id)
        {
            *existing = listing;
        } else {
            self.listings.push(listing);
        }
    }

    /// Find a listing by plugin id.
    #[must_use]
    pub fn get(&self, plugin_id: &str) -> Option<&MarketplaceListing> {
        self.listings
            .iter()
            .find(|listing| listing.manifest.id == plugin_id)
    }

    /// All installable listings.
    #[must_use]
    pub fn installable(&self) -> Vec<&MarketplaceListing> {
        self.listings
            .iter()
            .filter(|listing| listing.installable())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PluginRuntimeKind;

    fn listing() -> MarketplaceListing {
        MarketplaceListing {
            manifest: PluginManifest {
                id: "dev.nexkvm.test".into(),
                name: "Test".into(),
                version: "1.0.0".into(),
                description: String::new(),
                runtime: PluginRuntimeKind::Wasm,
                entrypoint: "plugin.wasm".into(),
                required_capabilities: PluginCapabilities::none(),
            },
            publisher: "nexkvm".into(),
            trust: MarketplaceTrust::VerifiedPublisher,
            artifacts: vec![PluginArtifact {
                runtime: PluginRuntimeKind::Wasm,
                url: "https://plugins.example/test.wasm".into(),
                sha256: "abc".into(),
                signature: Some("sig".into()),
            }],
            policy_cap: PluginCapabilities::none(),
        }
    }

    #[test]
    fn installable_requires_runtime_artifact_and_policy() {
        assert!(listing().installable());
        let mut blocked = listing();
        blocked.trust = MarketplaceTrust::Blocked;
        assert!(!blocked.installable());
    }

    #[test]
    fn catalog_upserts_and_filters() {
        let mut catalog = MarketplaceCatalog::new();
        catalog.upsert(listing());
        assert!(catalog.get("dev.nexkvm.test").is_some());
        assert_eq!(catalog.installable().len(), 1);
    }
}
