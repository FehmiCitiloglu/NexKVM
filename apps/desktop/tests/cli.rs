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
    assert!(stdout.contains("nexkvm permissions"));
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
fn pairing_uri_outputs_decodable_bootstrap() {
    use nexkvm_crypto::PairingBootstrap;

    let config_home = temp_config_home("pairing-uri");
    let output = nexkvm()
        .env("XDG_CONFIG_HOME", &config_home)
        .args(["pairing-uri", "192.168.1.40:47654"])
        .output()
        .expect("run nexkvm pairing-uri");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let uri = stdout.trim();
    let bootstrap = PairingBootstrap::from_uri(uri).expect("decodable pairing uri");
    assert_eq!(bootstrap.addr, "192.168.1.40:47654");
    assert!(!bootstrap.display_name.is_empty());
    assert_eq!(bootstrap.public_key.as_bytes().len(), 32);
}

#[test]
fn pairing_uri_reuses_persisted_identity_key() {
    use nexkvm_crypto::PairingBootstrap;

    let config_home = temp_config_home("pairing-uri-identity");
    let first = nexkvm()
        .env("XDG_CONFIG_HOME", &config_home)
        .args(["pairing-uri", "192.168.1.40:47654"])
        .output()
        .expect("run first nexkvm pairing-uri");
    let second = nexkvm()
        .env("XDG_CONFIG_HOME", &config_home)
        .args(["pairing-uri", "192.168.1.40:47654"])
        .output()
        .expect("run second nexkvm pairing-uri");

    assert!(first.status.success());
    assert!(second.status.success());
    let first = PairingBootstrap::from_uri(String::from_utf8_lossy(&first.stdout).trim())
        .expect("first uri");
    let second = PairingBootstrap::from_uri(String::from_utf8_lossy(&second.stdout).trim())
        .expect("second uri");

    assert_eq!(first.public_key, second.public_key);
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

#[test]
fn simulate_reports_typed_summary() {
    let config_home = temp_config_home("simulate-ok");
    let sim_path = config_home.join("sim.toml");
    std::fs::write(
        &sim_path,
        r#"
[network]
profile = "lan"
rtt_ms = 8
jitter_ms = 1
loss = 0.0
throughput_bps = 100000000

[[device]]
name = "desk-macos"
os = "macos"
role = "server"
id = "sim-desk"
display_name = "Desk Mac"
address = "192.168.1.20:47654"
trusted = true
x = 0
y = 0
width = 1728
height = 1117

[[device]]
name = "laptop-linux"
os = "linux-wayland"
role = "client"
display_name = "Laptop Linux"
address = "192.168.1.25:47654"
trusted = false
x = 1728
y = 0
width = 1920
height = 1080

[features]
clipboard = true
file_transfer = true
screen_preview = true
shared_cursor = true
plugins = false
"#,
    )
    .expect("write simulation config");

    let output = nexkvm()
        .arg("simulate")
        .arg(sim_path)
        .output()
        .expect("run nexkvm simulate");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("devices: 2"));
    assert!(stdout.contains(
        "id=sim-desk display_name=Desk Mac os=macos address=192.168.1.20:47654 trust=trusted"
    ));
    assert!(stdout.contains(
        "display_name=Laptop Linux os=linux-wayland address=192.168.1.25:47654 trust=untrusted"
    ));
    assert!(stdout.contains("status: typed TOML parsed and validated"));
}

#[test]
fn simulate_device_identity_fields_fallback_when_omitted() {
    let config_home = temp_config_home("simulate-fallback");
    let sim_path = config_home.join("sim.toml");
    std::fs::write(
        &sim_path,
        r#"
[network]
profile = "lan"
rtt_ms = 8
jitter_ms = 1
loss = 0.0
throughput_bps = 100000000

[[device]]
name = "tablet-future"
os = "android"
role = "client"
x = -1200
y = 100
width = 1200
height = 800

[features]
clipboard = true
file_transfer = true
screen_preview = true
shared_cursor = true
plugins = false
"#,
    )
    .expect("write simulation config");

    let output = nexkvm()
        .arg("simulate")
        .arg(sim_path)
        .output()
        .expect("run nexkvm simulate");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("display_name=tablet-future os=android address=unassigned trust=untrusted")
    );
    assert!(stdout.contains("id=sim-"));
}

#[test]
fn simulate_rejects_duplicate_device_names() {
    let config_home = temp_config_home("simulate-duplicate");
    let sim_path = config_home.join("sim.toml");
    std::fs::write(
        &sim_path,
        r#"
[network]
profile = "lan"
rtt_ms = 8
jitter_ms = 1
loss = 0.0
throughput_bps = 100000000

[[device]]
name = "duplicate"
os = "macos"
role = "server"
x = 0
y = 0
width = 1728
height = 1117

[[device]]
name = "duplicate"
os = "windows"
role = "client"
x = 1728
y = 0
width = 1920
height = 1080

[features]
clipboard = true
file_transfer = true
screen_preview = true
shared_cursor = true
plugins = false
"#,
    )
    .expect("write simulation config");

    let output = nexkvm()
        .arg("simulate")
        .arg(sim_path)
        .output()
        .expect("run nexkvm simulate");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("duplicate device name `duplicate`"));
}

#[test]
fn simulate_rejects_unknown_device_os() {
    let config_home = temp_config_home("simulate-unknown-os");
    let sim_path = config_home.join("sim.toml");
    std::fs::write(
        &sim_path,
        r#"
[network]
profile = "lan"
rtt_ms = 8
jitter_ms = 1
loss = 0.0
throughput_bps = 100000000

[[device]]
name = "strange-box"
os = "beos"
role = "server"
x = 0
y = 0
width = 1728
height = 1117

[features]
clipboard = true
file_transfer = true
screen_preview = true
shared_cursor = true
plugins = false
"#,
    )
    .expect("write simulation config");

    let output = nexkvm()
        .arg("simulate")
        .arg(sim_path)
        .output()
        .expect("run nexkvm simulate");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown device os `beos`"));
}
