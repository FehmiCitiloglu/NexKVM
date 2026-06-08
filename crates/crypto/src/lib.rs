//! Pairing & security model for coklu.
//!
//! This crate defines the *model* — the types and trait boundaries — for how
//! devices establish trust and secure a session. The session layer includes a
//! ChaCha20-Poly1305 AEAD implementation keyed from authenticated key-agreement
//! output; concrete platform handshakes still supply and authenticate that
//! shared secret before constructing a session.
//!
//! # Threat model & guarantees (target)
//! - **Confidentiality + integrity** of all traffic via an AEAD session cipher
//!   keyed by an authenticated key exchange. The transport (QUIC/TLS) provides
//!   the outer channel; this layer binds it to *device identity*.
//! - **Mutual device authentication**: each device owns a long-lived identity
//!   keypair. Pairing exchanges and pins the peer's public key.
//! - **Replay protection**: monotonic protocol message ids plus a per-session
//!   nonce window.
//! - **Trust on first use, then pinned**: after pairing, a device only accepts
//!   sessions from keys present in its [`TrustStore`].
//!
//! Pairing UX (QR scan or short numeric code) authenticates the *first* key
//! exchange to defeat man-in-the-middle; subsequent reconnects are silent.

mod coordinator;
mod error;
mod identity;
mod pairing;
mod qr;
mod session;
mod trust;

pub use coordinator::{ConfirmationCode, DEFAULT_PAIRING_TTL, PairingRole, PairingSession};
pub use error::CryptoError;
pub use identity::{DeviceIdentity, PublicKey};
pub use pairing::{PairingMethod, PairingRequest, PairingResponse, PairingState};
pub use qr::{NONCE_LEN, PairingBootstrap};
pub use session::{AeadSessionSecurity, SessionKeys, SessionSecurity};
pub use trust::{InMemoryTrustStore, TrustEntry, TrustStore};
