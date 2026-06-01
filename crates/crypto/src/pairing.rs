//! Pairing handshake model.
//!
//! Pairing authenticates the *first* connection between two devices so an
//! attacker cannot silently substitute their own key (MITM). Two methods are
//! supported at the model layer:
//!
//! - [`PairingMethod::QrCode`] — the initiator renders a QR encoding its public
//!   key + a one-time secret; the responder scans it. Best UX, phone-friendly.
//! - [`PairingMethod::NumericCode`] — both sides display a short code derived
//!   from the exchanged keys; the user confirms they match (SAS-style).
//!
//! Both bind the human confirmation to the exchanged key material, so a
//! successful pairing pins the peer key into the [`TrustStore`](crate::TrustStore).

use serde::{Deserialize, Serialize};

use crate::identity::DeviceIdentity;

/// How the user authenticates the first key exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PairingMethod {
    /// Scan a QR code rendered by the initiator.
    QrCode,
    /// Compare/enter a short numeric code shown on both devices.
    NumericCode,
}

/// Sent by the initiator to begin pairing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingRequest {
    /// Identity (incl. public key) of the requesting device.
    pub identity: DeviceIdentity,
    /// Chosen confirmation method.
    pub method: PairingMethod,
    /// Ephemeral one-time value bound into the confirmation code/QR.
    pub nonce: [u8; 32],
}

/// The responder's reply, completing the exchange once the user confirms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingResponse {
    /// Identity of the accepting device.
    pub identity: DeviceIdentity,
    /// `true` once the user confirmed the code/QR on this side.
    pub confirmed: bool,
}

/// State machine for an in-progress pairing attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PairingState {
    /// Waiting for the peer to respond to a [`PairingRequest`].
    AwaitingResponse,
    /// Waiting for local user confirmation of the code/QR.
    AwaitingConfirmation,
    /// Pairing succeeded; the peer key has been pinned.
    Paired,
    /// Pairing failed or was rejected.
    Failed,
}
