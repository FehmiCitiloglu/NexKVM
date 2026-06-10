//! End-to-end secure pairing: two independent [`PairingSession`]s drive a full
//! QR-bootstrap handshake through the public API, confirm via matching codes,
//! and both pin each other into their own trust stores.

use std::time::Instant;

use nexkvm_crypto::{
    DEFAULT_PAIRING_TTL, DeviceIdentity, InMemoryTrustStore, PairingBootstrap, PairingSession,
    PairingState, PublicKey, TrustStore,
};

fn identity(name: &str, key: &[u8]) -> DeviceIdentity {
    DeviceIdentity {
        display_name: name.into(),
        public_key: PublicKey(key.to_vec()),
    }
}

#[test]
fn two_devices_pair_via_qr_and_pin_each_other() {
    let now = Instant::now();

    // Each device owns a long-lived identity and a local trust store.
    let mac = identity("Alien's MacBook", &[0x11, 0x22, 0x33, 0x44]);
    let phone = identity("Alien's Phone", &[0xAA, 0xBB, 0xCC, 0xDD]);
    let mac_store = InMemoryTrustStore::new();
    let phone_store = InMemoryTrustStore::new();

    // 1. Initiator (Mac) renders a QR bootstrap.
    let nonce = [0x5Au8; 32];
    let mut mac_session = PairingSession::initiate(
        mac.clone(),
        "192.168.1.42:47654",
        nonce,
        now,
        DEFAULT_PAIRING_TTL,
    );
    let uri = mac_session.bootstrap().unwrap().to_uri();

    // 2. Responder (Phone) scans the QR (parses the URI) and starts its side.
    let scanned = PairingBootstrap::from_uri(&uri).unwrap();
    assert_eq!(scanned.public_key, mac.public_key);
    let mut phone_session =
        PairingSession::respond(phone.clone(), &scanned, now, DEFAULT_PAIRING_TTL).unwrap();

    // 3. Both derive a confirmation code over the peer key; they must match.
    let mac_code = mac_session
        .confirmation_code(&phone.public_key, now)
        .unwrap();
    let phone_code = phone_session
        .confirmation_code(&mac.public_key, now)
        .unwrap();
    assert_eq!(
        mac_code, phone_code,
        "honest pairing yields identical codes"
    );

    // 4. The user confirms the codes match; each side pins the peer.
    mac_session
        .accept(&phone, 1_700_000_000, now, &mac_store)
        .unwrap();
    phone_session
        .verify_and_accept(mac_code.as_str(), &mac, 1_700_000_000, now, &phone_store)
        .unwrap();

    // Both sessions completed and trust is mutual.
    assert_eq!(mac_session.state(), PairingState::Paired);
    assert_eq!(phone_session.state(), PairingState::Paired);
    assert!(mac_store.is_trusted(&phone.public_key));
    assert!(phone_store.is_trusted(&mac.public_key));
}
