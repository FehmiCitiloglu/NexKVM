//! Device identity primitives.

use std::fmt;

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::CryptoError;

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

/// A long-lived device identity keypair.
///
/// The private half is intentionally opaque and has non-leaking `Debug` output.
/// Storage backends should keep the seed in an OS-protected secret store and
/// reconstruct this type at runtime.
#[derive(Clone)]
pub struct DeviceKeypair {
    signing_key: SigningKey,
}

impl DeviceKeypair {
    /// Construct an Ed25519 device keypair from a 32-byte seed.
    #[must_use]
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self {
            signing_key: SigningKey::from_bytes(&seed),
        }
    }

    /// Return the public identity key peers pin in the trust store.
    #[must_use]
    pub fn public_key(&self) -> PublicKey {
        PublicKey(self.signing_key.verifying_key().to_bytes().to_vec())
    }

    /// Sign an authenticated-session challenge or transcript.
    #[must_use]
    pub fn sign_identity_challenge(&self, challenge: &[u8]) -> IdentitySignature {
        IdentitySignature(self.signing_key.sign(challenge).to_bytes().to_vec())
    }
}

impl fmt::Debug for DeviceKeypair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceKeypair")
            .field("public_key", &self.public_key())
            .finish_non_exhaustive()
    }
}

/// Ed25519 signature proving possession of a device identity private key.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentitySignature(pub Vec<u8>);

impl IdentitySignature {
    /// Borrow raw signature bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for IdentitySignature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("IdentitySignature")
            .field(&format_args!("{} bytes", self.0.len()))
            .finish()
    }
}

/// Verify that `signature` was produced by the private key matching `public_key`.
///
/// # Errors
/// Returns [`CryptoError::BadSignature`] if the public key or signature is
/// malformed or verification fails.
pub fn verify_identity_signature(
    public_key: &PublicKey,
    challenge: &[u8],
    signature: &IdentitySignature,
) -> Result<(), CryptoError> {
    let key_bytes: [u8; 32] = public_key
        .as_bytes()
        .try_into()
        .map_err(|_| CryptoError::BadSignature)?;
    let verifying_key =
        VerifyingKey::from_bytes(&key_bytes).map_err(|_| CryptoError::BadSignature)?;
    let signature =
        Signature::from_slice(signature.as_bytes()).map_err(|_| CryptoError::BadSignature)?;
    verifying_key
        .verify(challenge, &signature)
        .map_err(|_| CryptoError::BadSignature)
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
