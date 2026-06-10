//! Placeholder for the future nexkvm mobile companion (Android/iOS).
//!
//! Reserved in the workspace now so crate boundaries and the shared `core`
//! dependency are established early. The mobile app will reuse `core`,
//! `protocol`, `crypto`, and `network` while providing platform backends via
//! Android (`InputManager`/`ClipboardManager`) and iOS APIs in a later phase.

/// Returns the OS families the mobile companion will target.
#[must_use]
pub fn planned_targets() -> [nexkvm_core::OsKind; 2] {
    [nexkvm_core::OsKind::Android, nexkvm_core::OsKind::Ios]
}
