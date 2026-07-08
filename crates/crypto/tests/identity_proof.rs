use nexkvm_crypto::{DeviceKeypair, verify_identity_signature};

#[test]
fn device_keypair_signs_and_verifies_identity_challenges() {
    let alice = DeviceKeypair::from_seed([1u8; 32]);
    let bob = DeviceKeypair::from_seed([2u8; 32]);
    let challenge = b"nexkvm signed session transcript";

    let signature = alice.sign_identity_challenge(challenge);

    verify_identity_signature(&alice.public_key(), challenge, &signature).expect("valid signature");
    assert!(verify_identity_signature(&bob.public_key(), challenge, &signature).is_err());
}
