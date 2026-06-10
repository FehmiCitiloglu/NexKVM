//! Device identity within a nexkvm mesh.
//!
//! Distinct from cryptographic identity ([`nexkvm_crypto::DeviceIdentity`]): a
//! [`DeviceId`] is a stable, opaque handle used for routing and UI, while the
//! crypto key proves *authenticity*. The two are bound together at pairing.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Stable, opaque identifier for a device. Generated once and persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeviceId(pub Uuid);

impl DeviceId {
    /// Generate a fresh random device id.
    #[must_use]
    pub fn generate() -> Self {
        Self(Uuid::new_v4())
    }
}

impl std::fmt::Display for DeviceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Operating-system family of a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum OsKind {
    /// Windows.
    Windows,
    /// macOS.
    MacOs,
    /// Linux (X11 or Wayland).
    Linux,
    /// Android (future mobile companion).
    Android,
    /// iOS (future mobile companion).
    Ios,
    /// Unknown / unreported.
    Unknown,
}

/// The role a device plays in a given session.
///
/// A device can switch roles between sessions (a laptop may be a server for a
/// tablet but a client of a desktop). Roles are negotiated per link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeviceRole {
    /// Owns the shared input/clipboard surface; other devices connect to it.
    Server,
    /// Connects to a server and contributes/consumes events.
    Client,
}

/// Advertised metadata for a device on the mesh.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    /// Stable identifier.
    pub id: DeviceId,
    /// Human-readable name.
    pub name: String,
    /// OS family.
    pub os: OsKind,
}

impl DeviceInfo {
    /// Construct device info with a fresh id.
    #[must_use]
    pub fn new(name: impl Into<String>, os: OsKind) -> Self {
        Self {
            id: DeviceId::generate(),
            name: name.into(),
            os,
        }
    }
}
