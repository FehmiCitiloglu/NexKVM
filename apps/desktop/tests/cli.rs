//! End-to-end CLI smoke tests for the `nexkvm` binary.
//!
//! Exercises the developer CLI surface (help, protocol, pairing decode, unknown
//! command handling) by invoking the built binary, so argument dispatch and
//! exit codes are covered without standing up the daemon.

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn nexkvm() -> Command {
    Command::new(env!("CARGO_BIN_EXE_nexkvm"))
}

fn temp_config_home(name: &str) -> std::path::PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("nexkvm-{name}-{unique}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn help_lists_the_subcommands() {
    let output = nexkvm().arg("help").output().expect("run nexkvm help");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("nexkvm devices"));
    assert!(stdout.contains("nexkvm pair [--accept] <uri>"));
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
fn pair_accept_persists_trusted_device() {
    use nexkvm_crypto::{PairingBootstrap, PublicKey};

    let config_home = temp_config_home("pair-accept");
    let bootstrap = PairingBootstrap::new(
        "trusted-mac",
        PublicKey(vec![9, 8, 7, 6, 5]),
        [3u8; nexkvm_crypto::NONCE_LEN],
        "192.168.1.30:47654",
    );
    let uri = bootstrap.to_uri();

    let output = nexkvm()
        .env("XDG_CONFIG_HOME", &config_home)
        .args(["pair", "--accept", &uri])
        .output()
        .expect("run nexkvm pair --accept");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("trusted device accepted"));
    assert!(stdout.contains("trusted-mac"));
    assert!(stdout.contains(&bootstrap.public_key.fingerprint()));

    let devices = nexkvm()
        .env("XDG_CONFIG_HOME", &config_home)
        .arg("devices")
        .output()
        .expect("run nexkvm devices");
    assert!(devices.status.success());
    let devices_out = String::from_utf8_lossy(&devices.stdout);
    assert!(devices_out.contains("trusted-mac"));
    assert!(devices_out.contains(&bootstrap.public_key.fingerprint()));
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
