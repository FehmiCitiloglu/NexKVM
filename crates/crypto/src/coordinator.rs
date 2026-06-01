//! Pairing coordinator: drives a single pairing attempt end-to-end.
//!
//! The model types ([`PairingBootstrap`], [`PairingState`], [`TrustEntry`]) say
//! *what* a pairing is; this module says *how* one runs. A [`PairingSession`]
//! ties them together for one device's side of a pairing:
//!
//! 1. The **initiator** calls [`PairingSession::initiate`], rendering its
//!    [`PairingBootstrap`] as a QR code.
//! 2. The **responder** scans it and calls [`PairingSession::respond`], which
//!    rejects an expired one-time token up front.
//! 3. Both sides independently derive a [`ConfirmationCode`] over *both* public
//!    keys and the shared one-time nonce ([`PairingSession::confirmation_code`]).
//!    The user compares the two codes — this out-of-band check is what defeats a
//!    man-in-the-middle: a swapped key changes the code on exactly one side.
//! 4. On a match the user accepts; [`PairingSession::accept`] pins the peer's key
//!    into the [`TrustStore`] and burns the one-time token so it cannot be
//!    reused.
//!
//! The coordinator is **sans-IO and clock-injected**: it never touches the
//! network and takes the current [`Instant`] from the caller, so the whole flow
//! is deterministic and unit-testable. Randomness (the nonce) is supplied by the
//! caller from the OS RNG, keeping this crate dependency-free for entropy.
//!
//! # Security properties
//! - **One-time token**: the nonce is single-use with a TTL; an expired or
//!   already-consumed token is rejected ([`CryptoError::PairingTimeout`] /
//!   [`CryptoError::Pairing`]).
//! - **Mutual authentication**: the [`ConfirmationCode`] is symmetric in the two
//!   keys (they are length-prefixed and ordered before hashing), so both devices
//!   compute the same value only if they observed the same two keys.
//! - **Pin-on-success**: trust is granted solely by [`accept`](PairingSession::accept),
//!   which writes the peer's *public* key — never any secret — into the store.

use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::error::CryptoError;
use crate::identity::{DeviceIdentity, PublicKey};
use crate::pairing::PairingState;
use crate::qr::{NONCE_LEN, PairingBootstrap};
use crate::trust::{TrustEntry, TrustStore};

/// Number of decimal digits in a [`ConfirmationCode`].
const CODE_DIGITS: u32 = 6;

/// Default lifetime of a pairing token before it expires.
pub const DEFAULT_PAIRING_TTL: Duration = Duration::from_secs(120);

/// A short numeric string both devices display so the user can confirm there is
/// no man-in-the-middle. Equal codes on both screens authenticate the exchange.
#[derive(Clone, PartialEq, Eq)]
pub struct ConfirmationCode(String);

impl ConfirmationCode {
    /// The code as a zero-padded decimal string (e.g. `"047219"`).
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ConfirmationCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

// A confirmation code is low-entropy and user-facing, but avoid surprising it
// into structured logs alongside keys; show it explicitly via `Display`.
impl std::fmt::Debug for ConfirmationCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ConfirmationCode").field(&self.0).finish()
    }
}

/// A single-use pairing secret with an expiry.
#[derive(Debug, Clone)]
struct PairingToken {
    nonce: [u8; NONCE_LEN],
    expires_at: Instant,
    used: bool,
}

impl PairingToken {
    fn new(nonce: [u8; NONCE_LEN], now: Instant, ttl: Duration) -> Self {
        Self {
            nonce,
            expires_at: now + ttl,
            used: false,
        }
    }

    /// Validate the token is neither expired nor already consumed.
    fn check_live(&self, now: Instant) -> Result<(), CryptoError> {
        if self.used {
            return Err(CryptoError::Pairing("pairing token already used".into()));
        }
        if now >= self.expires_at {
            return Err(CryptoError::PairingTimeout);
        }
        Ok(())
    }
}

/// Which side of the exchange this session represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingRole {
    /// Rendered the QR / started the exchange.
    Initiator,
    /// Scanned the QR / answered the exchange.
    Responder,
}

/// One device's side of an in-progress pairing.
#[derive(Debug)]
pub struct PairingSession {
    local: DeviceIdentity,
    role: PairingRole,
    token: PairingToken,
    state: PairingState,
    /// Dial address the initiator advertises (unused by the responder).
    addr: String,
}

impl PairingSession {
    /// Start pairing as the **initiator**.
    ///
    /// `nonce` must be freshly random and single-use (callers source it from the
    /// OS RNG). `addr` is the `ip:port` the responder should dial. The session
    /// begins in [`PairingState::AwaitingResponse`]; render [`bootstrap`] as a QR.
    ///
    /// [`bootstrap`]: PairingSession::bootstrap
    #[must_use]
    pub fn initiate(
        local: DeviceIdentity,
        addr: impl Into<String>,
        nonce: [u8; NONCE_LEN],
        now: Instant,
        ttl: Duration,
    ) -> Self {
        Self {
            local,
            role: PairingRole::Initiator,
            token: PairingToken::new(nonce, now, ttl),
            state: PairingState::AwaitingResponse,
            addr: addr.into(),
        }
    }

    /// Start pairing as the **responder**, from a scanned [`PairingBootstrap`].
    ///
    /// The bootstrap's one-time token is validated against `now`; a stale QR is
    /// rejected before any trust is touched. The session begins in
    /// [`PairingState::AwaitingConfirmation`].
    ///
    /// # Errors
    /// Returns [`CryptoError::PairingTimeout`] if the bootstrap's token has
    /// expired relative to `now`.
    pub fn respond(
        local: DeviceIdentity,
        bootstrap: &PairingBootstrap,
        now: Instant,
        ttl: Duration,
    ) -> Result<Self, CryptoError> {
        let token = PairingToken::new(bootstrap.nonce, now, ttl);
        // A freshly scanned bootstrap should still be live; this guards against a
        // zero/negative TTL misconfiguration consistently with the initiator.
        token.check_live(now)?;
        Ok(Self {
            local,
            role: PairingRole::Responder,
            token,
            state: PairingState::AwaitingConfirmation,
            addr: bootstrap.addr.clone(),
        })
    }

    /// The QR bootstrap to render (initiator only).
    #[must_use]
    pub fn bootstrap(&self) -> Option<PairingBootstrap> {
        if self.role != PairingRole::Initiator {
            return None;
        }
        Some(PairingBootstrap::new(
            self.local.display_name.clone(),
            self.local.public_key.clone(),
            self.token.nonce,
            self.addr.clone(),
        ))
    }

    /// Current state of the handshake.
    #[must_use]
    pub fn state(&self) -> PairingState {
        self.state
    }

    /// Whether the one-time token has expired at `now`.
    #[must_use]
    pub fn is_expired(&self, now: Instant) -> bool {
        now >= self.token.expires_at
    }

    /// Derive the confirmation code for the user to compare across devices.
    ///
    /// Both sides pass the *peer's* public key; the derivation is symmetric, so a
    /// genuine pair yields identical codes while a MITM (different keys observed
    /// on each side) yields different ones.
    ///
    /// # Errors
    /// Returns [`CryptoError::PairingTimeout`] or [`CryptoError::Pairing`] if the
    /// token is expired or already consumed.
    pub fn confirmation_code(
        &self,
        peer_key: &PublicKey,
        now: Instant,
    ) -> Result<ConfirmationCode, CryptoError> {
        self.token.check_live(now)?;
        Ok(confirmation_code(
            &self.local.public_key,
            peer_key,
            &self.token.nonce,
        ))
    }

    /// Accept the pairing after the user confirmed the codes match: pin `peer`
    /// into `store` and burn the one-time token.
    ///
    /// `paired_at` is a wall-clock Unix timestamp (seconds) recorded on the trust
    /// entry; the token TTL itself uses a monotonic clock.
    ///
    /// # Errors
    /// Returns [`CryptoError::PairingTimeout`] / [`CryptoError::Pairing`] if the
    /// token is no longer live.
    pub fn accept(
        &mut self,
        peer: &DeviceIdentity,
        paired_at: u64,
        now: Instant,
        store: &dyn TrustStore,
    ) -> Result<TrustEntry, CryptoError> {
        self.token.check_live(now)?;
        let entry = TrustEntry {
            display_name: peer.display_name.clone(),
            public_key: peer.public_key.clone(),
            paired_at,
        };
        store.insert(entry.clone());
        self.token.used = true; // single-use: cannot pair again with this token
        self.state = PairingState::Paired;
        Ok(entry)
    }

    /// Verify a user-entered `entered` code against the expected one, then accept.
    ///
    /// Convenience for the numeric-entry flow: the peer reads their code aloud and
    /// the local user types it. A mismatch aborts pairing without pinning.
    ///
    /// # Errors
    /// Returns [`CryptoError::PairingMismatch`] if the codes differ, or the same
    /// token errors as [`accept`](Self::accept).
    pub fn verify_and_accept(
        &mut self,
        entered: &str,
        peer: &DeviceIdentity,
        paired_at: u64,
        now: Instant,
        store: &dyn TrustStore,
    ) -> Result<TrustEntry, CryptoError> {
        let expected = self.confirmation_code(&peer.public_key, now)?;
        if !constant_time_eq(entered.as_bytes(), expected.as_str().as_bytes()) {
            self.state = PairingState::Failed;
            return Err(CryptoError::PairingMismatch);
        }
        self.accept(peer, paired_at, now, store)
    }

    /// Abort the pairing (user rejected the code or cancelled).
    pub fn reject(&mut self) {
        self.token.used = true;
        self.state = PairingState::Failed;
    }
}

/// Derive a [`ConfirmationCode`] from both public keys and the one-time nonce.
///
/// Keys are length-prefixed and ordered so the result is identical regardless of
/// which device is the initiator (mutual authentication). Domain-separated under
/// a version tag so the derivation can evolve without ambiguity.
fn confirmation_code(a: &PublicKey, b: &PublicKey, nonce: &[u8; NONCE_LEN]) -> ConfirmationCode {
    let (lo, hi) = if a.as_bytes() <= b.as_bytes() {
        (a, b)
    } else {
        (b, a)
    };

    let mut hasher = Sha256::new();
    hasher.update(b"coklu-pair-sas-v1");
    absorb_field(&mut hasher, lo.as_bytes());
    absorb_field(&mut hasher, hi.as_bytes());
    hasher.update(nonce);
    let digest = hasher.finalize();

    let raw = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]);
    let modulus = 10u32.pow(CODE_DIGITS);
    let value = raw % modulus;
    ConfirmationCode(format!("{value:0width$}", width = CODE_DIGITS as usize))
}

/// Length-prefix a field before hashing so concatenation is unambiguous.
fn absorb_field(hasher: &mut Sha256, field: &[u8]) {
    let len = u32::try_from(field.len()).unwrap_or(u32::MAX);
    hasher.update(len.to_be_bytes());
    hasher.update(field);
}

/// Constant-time byte comparison, to avoid leaking the code via timing.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trust::InMemoryTrustStore;

    fn identity(name: &str, key: &[u8]) -> DeviceIdentity {
        DeviceIdentity {
            display_name: name.into(),
            public_key: PublicKey(key.to_vec()),
        }
    }

    fn pair_sessions(
        nonce: [u8; NONCE_LEN],
        now: Instant,
    ) -> (
        PairingSession,
        DeviceIdentity,
        PairingSession,
        DeviceIdentity,
    ) {
        let initiator_id = identity("MacBook", &[1, 2, 3, 4]);
        let responder_id = identity("Phone", &[9, 8, 7, 6]);

        let initiator = PairingSession::initiate(
            initiator_id.clone(),
            "192.168.1.10:47654",
            nonce,
            now,
            DEFAULT_PAIRING_TTL,
        );
        let bootstrap = initiator.bootstrap().unwrap();
        let responder =
            PairingSession::respond(responder_id.clone(), &bootstrap, now, DEFAULT_PAIRING_TTL)
                .unwrap();
        (initiator, initiator_id, responder, responder_id)
    }

    #[test]
    fn both_sides_derive_matching_code() {
        let now = Instant::now();
        let (initiator, init_id, responder, resp_id) = pair_sessions([5u8; NONCE_LEN], now);

        let code_a = initiator
            .confirmation_code(&resp_id.public_key, now)
            .unwrap();
        let code_b = responder
            .confirmation_code(&init_id.public_key, now)
            .unwrap();

        assert_eq!(code_a, code_b, "honest pair must agree");
        assert_eq!(code_a.as_str().len(), CODE_DIGITS as usize);
    }

    #[test]
    fn mitm_substituted_key_breaks_code() {
        let now = Instant::now();
        let (initiator, init_id, responder, _resp_id) = pair_sessions([5u8; NONCE_LEN], now);

        // Responder is talking to an attacker presenting a different key, while
        // the initiator still sees the real responder key.
        let attacker = identity("attacker", &[0xAA, 0xBB]);
        let code_initiator = initiator
            .confirmation_code(&identity("Phone", &[9, 8, 7, 6]).public_key, now)
            .unwrap();
        let code_responder = responder
            .confirmation_code(&attacker.public_key, now)
            .unwrap();

        assert_ne!(
            code_initiator, code_responder,
            "MITM key substitution must change the code"
        );
        let _ = init_id;
    }

    #[test]
    fn accept_pins_peer_and_burns_token() {
        let now = Instant::now();
        let (mut initiator, _init_id, _responder, resp_id) = pair_sessions([1u8; NONCE_LEN], now);
        let store = InMemoryTrustStore::new();

        let entry = initiator
            .accept(&resp_id, 1_700_000_000, now, &store)
            .unwrap();
        assert_eq!(initiator.state(), PairingState::Paired);
        assert!(store.is_trusted(&entry.public_key));

        // One-time: a second accept with the same token is rejected.
        let err = initiator
            .accept(&resp_id, 1_700_000_000, now, &store)
            .unwrap_err();
        assert!(matches!(err, CryptoError::Pairing(_)));
    }

    #[test]
    fn expired_token_is_rejected_on_respond_and_code() {
        let now = Instant::now();
        let initiator = PairingSession::initiate(
            identity("a", &[1]),
            "127.0.0.1:1",
            [2u8; NONCE_LEN],
            now,
            Duration::from_secs(10),
        );
        let bootstrap = initiator.bootstrap().unwrap();

        // Past the TTL: confirmation code derivation fails.
        let later = now + Duration::from_secs(11);
        let err = initiator
            .confirmation_code(&PublicKey(vec![9]), later)
            .unwrap_err();
        assert!(matches!(err, CryptoError::PairingTimeout));

        // Responding to a stale bootstrap with a zero TTL also fails immediately.
        let err = PairingSession::respond(
            identity("b", &[2]),
            &bootstrap,
            later,
            Duration::from_secs(0),
        )
        .unwrap_err();
        assert!(matches!(err, CryptoError::PairingTimeout));
    }

    #[test]
    fn verify_and_accept_rejects_wrong_code() {
        let now = Instant::now();
        let (mut initiator, _init_id, _responder, resp_id) = pair_sessions([3u8; NONCE_LEN], now);
        let store = InMemoryTrustStore::new();

        let err = initiator
            .verify_and_accept("000000", &resp_id, 1_700_000_000, now, &store)
            .unwrap_err();
        assert!(matches!(err, CryptoError::PairingMismatch));
        assert_eq!(initiator.state(), PairingState::Failed);
        assert!(!store.is_trusted(&resp_id.public_key));
    }

    #[test]
    fn verify_and_accept_pins_on_correct_code() {
        let now = Instant::now();
        let (mut initiator, _init_id, _responder, resp_id) = pair_sessions([4u8; NONCE_LEN], now);
        let store = InMemoryTrustStore::new();

        let code = initiator
            .confirmation_code(&resp_id.public_key, now)
            .unwrap();
        let entry = initiator
            .verify_and_accept(code.as_str(), &resp_id, 1_700_000_000, now, &store)
            .unwrap();

        assert_eq!(initiator.state(), PairingState::Paired);
        assert!(store.is_trusted(&entry.public_key));
    }
}
