use nexkvm_crypto::{EphemeralKeyAgreement, EphemeralPublicKey};

#[test]
fn ephemeral_x25519_endpoints_derive_the_same_secret() {
    let alice = EphemeralKeyAgreement::from_secret([0x11; 32]);
    let bob = EphemeralKeyAgreement::from_secret([0x22; 32]);

    let alice_secret = alice.agree(bob.public_key()).unwrap();
    let bob_secret = bob.agree(alice.public_key()).unwrap();

    assert_eq!(alice_secret.as_bytes(), bob_secret.as_bytes());
}

#[test]
fn distinct_ephemeral_keys_do_not_reuse_session_material() {
    let peer = EphemeralKeyAgreement::from_secret([0x33; 32]);
    let first = EphemeralKeyAgreement::from_secret([0x44; 32]);
    let second = EphemeralKeyAgreement::from_secret([0x55; 32]);

    assert_ne!(first.public_key(), second.public_key());
    assert_ne!(
        first.agree(peer.public_key()).unwrap().as_bytes(),
        second.agree(peer.public_key()).unwrap().as_bytes()
    );
}

#[test]
fn rejects_low_order_peer_public_key() {
    let local = EphemeralKeyAgreement::from_secret([0x66; 32]);
    let low_order = EphemeralPublicKey::from_bytes([0; 32]);

    assert!(local.agree(low_order).is_err());
}
