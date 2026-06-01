//! End-to-end CLI smoke tests for the `coklu` binary.
//!
//! Exercises the developer CLI surface (help, protocol, pairing decode, unknown
//! command handling) by invoking the built binary, so argument dispatch and
//! exit codes are covered without standing up the daemon.

use std::process::Command;

fn coklu() -> Command {
    Command::new(env!("CARGO_BIN_EXE_coklu"))
}

#[test]
fn help_lists_the_subcommands() {
    let output = coklu().arg("help").output().expect("run coklu help");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("coklu devices"));
    assert!(stdout.contains("coklu pair <uri>"));
    assert!(stdout.contains("--debug"));
}

#[test]
fn protocol_reports_version() {
    let output = coklu()
        .arg("protocol")
        .output()
        .expect("run coklu protocol");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("protocol:"));
}

#[test]
fn pair_decodes_a_bootstrap_uri() {
    use coklu_crypto::{PairingBootstrap, PublicKey};

    let bootstrap = PairingBootstrap::new(
        "studio-mac",
        PublicKey(vec![1, 2, 3, 4, 5]),
        [0u8; coklu_crypto::NONCE_LEN],
        "192.168.1.20:47654",
    );
    let uri = bootstrap.to_uri();

    let output = coklu()
        .args(["pair", &uri])
        .output()
        .expect("run coklu pair");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("studio-mac"));
    assert!(stdout.contains("192.168.1.20:47654"));
    assert!(stdout.contains(&bootstrap.public_key.fingerprint()));
}

#[test]
fn unknown_command_fails() {
    let output = coklu()
        .arg("frobnicate")
        .output()
        .expect("run coklu frobnicate");
    assert!(!output.status.success());
}

#[test]
fn pair_without_uri_fails() {
    let output = coklu().arg("pair").output().expect("run coklu pair");
    assert!(!output.status.success());
}
