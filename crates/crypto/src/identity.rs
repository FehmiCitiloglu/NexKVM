//! Device identity primitives.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A device's long-lived public key.
///
/// Stored as opaque bytes at the model layer; the concrete algorithm
/// (Ed25519 expected) is fixed by the crypto backend feature. The byte length
/// is not enforced here so the model stays backend-agnostic.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PublicKey(pub Vec<u8>);

impl PublicKey {
    /// Borrow the raw key bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// A short, stable hex fingerprint for display / pairing confirmation.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        self.0
            .iter()
            .take(8)
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(":")
    }
}

// Avoid leaking full key material into logs.
impl fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("PublicKey")
            .field(&self.fingerprint())
            .finish()
    }
}

/// A device's cryptographic identity (public half).
///
/// The private half is held only by the owning device and never serialized
/// here; key storage lives in the `storage` crate behind OS keychains.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceIdentity {
    /// Human-readable device name (e.g. "Alien's MacBook").
    pub display_name: String,
    /// The device's long-lived public key.
    pub public_key: PublicKey,
}
