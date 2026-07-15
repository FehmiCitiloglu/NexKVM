//! Fresh X25519 key agreement for authenticated peer sessions.

use curve25519_dalek::montgomery::MontgomeryPoint;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::CryptoError;

/// The public half of a fresh X25519 key agreement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EphemeralPublicKey([u8; 32]);

impl EphemeralPublicKey {
    /// Construct from the 32-byte X25519 wire representation.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Return the wire representation.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// A single-use X25519 private key.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct EphemeralKeyAgreement {
    secret: [u8; 32],
    #[zeroize(skip)]
    public: EphemeralPublicKey,
}

impl EphemeralKeyAgreement {
    /// Generate a fresh key using the operating system CSPRNG.
    ///
    /// # Errors
    /// Returns [`CryptoError::KeyExchange`] when secure randomness is unavailable.
    pub fn generate() -> Result<Self, CryptoError> {
        let mut secret = [0u8; 32];
        getrandom::fill(&mut secret).map_err(|error| {
            CryptoError::KeyExchange(format!("random generation failed: {error}"))
        })?;
        Ok(Self::from_secret(secret))
    }

    /// Deterministically construct a key for protocol tests.
    #[doc(hidden)]
    #[must_use]
    pub fn from_secret(secret: [u8; 32]) -> Self {
        let public = EphemeralPublicKey(*MontgomeryPoint::mul_base_clamped(secret).as_bytes());
        Self { secret, public }
    }

    /// Public key to include in the authenticated handshake transcript.
    #[must_use]
    pub const fn public_key(&self) -> EphemeralPublicKey {
        self.public
    }

    /// Derive a shared secret with a peer public key.
    ///
    /// # Errors
    /// Rejects low-order public keys that produce an all-zero shared secret.
    pub fn agree(&self, peer: EphemeralPublicKey) -> Result<SharedSecret, CryptoError> {
        let shared = *MontgomeryPoint(*peer.as_bytes())
            .mul_clamped(self.secret)
            .as_bytes();
        if shared == [0; 32] {
            return Err(CryptoError::KeyExchange(
                "peer supplied a low-order ephemeral key".into(),
            ));
        }
        Ok(SharedSecret(shared))
    }
}

impl std::fmt::Debug for EphemeralKeyAgreement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EphemeralKeyAgreement")
            .field("public", &self.public)
            .finish_non_exhaustive()
    }
}

/// X25519 output that is cleared when dropped.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct SharedSecret([u8; 32]);

impl SharedSecret {
    /// Borrow the secret for HKDF input.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl std::fmt::Debug for SharedSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedSecret").finish_non_exhaustive()
    }
}
