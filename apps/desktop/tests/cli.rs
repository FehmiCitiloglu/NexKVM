//! End-to-end CLI smoke tests for the `nexkvm` binary.
//!
//! Exercises the developer CLI surface (help, protocol, pairing decode, unknown
//! command handling) by invoking the built binary, so argument dispatch and
//! exit codes are covered without standing up the daemon.

use std::process::Command;

fn nexkvm() -> Command {
    Command::new(env!("CARGO_BIN_EXE_nexkvm"))
}

#[test]
fn help_lists_the_subcommands() {
    let output = nexkvm().arg("help").output().expect("run nexkvm help");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("nexkvm devices"));
    assert!(stdout.contains("nexkvm pair <uri>"));
    assert!(stdout.contains("--debug"));
}

#[test]
fn protocol_reports_version() {
    let output = nexkvm()
        .arg("protocol")
        .output()
        .expect("run nexkvm protocol");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("protocol:"));
}

#[test]
fn pair_decodes_a_bootstrap_uri() {
    use nexkvm_crypto::{PairingBootstrap, PublicKey};

    let bootstrap = PairingBootstrap::new(
        "studio-mac",
        PublicKey(vec![1, 2, 3, 4, 5]),
        [0u8; nexkvm_crypto::NONCE_LEN],
        "192.168.1.20:47654",
    );
    let uri = bootstrap.to_uri();

    let output = nexkvm()
        .args(["pair", &uri])
        .output()
        .expect("run nexkvm pair");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("studio-mac"));
    assert!(stdout.contains("192.168.1.20:47654"));
    assert!(stdout.contains(&bootstrap.public_key.fingerprint()));
}

#[test]
fn unknown_command_fails() {
    let output = nexkvm()
        .arg("frobnicate")
        .output()
        .expect("run nexkvm frobnicate");
    assert!(!output.status.success());
}

#[test]
fn pair_without_uri_fails() {
    let output = nexkvm().arg("pair").output().expect("run nexkvm pair");
    assert!(!output.status.success());
}
