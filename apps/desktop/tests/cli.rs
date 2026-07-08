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

fn extract_simulation_report(stdout: &str) -> serde_json::Value {
    if let Some(report_line) = stdout
        .lines()
        .find(|line| line.trim_start().starts_with("simulation_report_json: "))
    {
        let json = report_line
            .trim_start()
            .trim_start_matches("simulation_report_json: ");
        return serde_json::from_str(json).expect("valid simulation_report_json");
    }
    serde_json::from_str(stdout.trim()).expect("valid simulation_report_json")
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
    assert!(stdout.contains("nexkvm pipewire-smoke"));
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

#[cfg(not(target_os = "linux"))]
#[test]
fn pipewire_smoke_reports_unavailable_off_linux() {
    let output = nexkvm()
        .arg("pipewire-smoke")
        .output()
        .expect("run nexkvm pipewire-smoke");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("nexkvm pipewire-smoke"));
    assert!(stdout.contains("status: unavailable"));
    assert!(stdout.contains("Linux PipeWire ScreenCast smoke is only available on Linux targets"));
}

#[cfg(not(target_os = "linux"))]
#[test]
fn audio_smoke_reports_unavailable_off_linux() {
    let output = nexkvm()
        .arg("audio-smoke")
        .output()
        .expect("run nexkvm audio-smoke");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("nexkvm audio-smoke"));
    assert!(stdout.contains("status: unavailable"));
    assert!(stdout.contains("Linux PipeWire audio smoke is only available on Linux targets"));
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
    assert!(stdout.contains("connection planning:"));
    assert!(stdout.contains("Desk Mac: direct-lan (connect directly to 192.168.1.20:47654)"));
    assert!(stdout.contains("Laptop Linux: missing-trust (device is not trusted)"));
    assert!(stdout.contains("simulators:"));
    assert!(stdout.contains("discovery: ranked=2"));
    assert!(stdout.contains("latency: smoothed="));
    assert!(stdout.contains("workspace: snap_right target=Laptop Linux cross_device=true"));
    assert!(stdout.contains("screen: codec="));
    assert!(
        stdout.contains("collaboration: participants=2 pending_requests=0 control_active=true")
    );
    assert!(stdout.contains("simulation_report_json: {"));
    let report = extract_simulation_report(&stdout);
    assert_eq!(report["devices"].as_array().map(Vec::len), Some(2));
    assert_eq!(
        report["connection_planning"][0]["kind"].as_str(),
        Some("direct-lan")
    );
    assert_eq!(
        report["simulators"]["workspace"]["status"].as_str(),
        Some("ok")
    );
    assert_eq!(
        report["simulators"]["collaboration"]["control_active"].as_bool(),
        Some(true)
    );
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
    assert!(stdout.contains("tablet-future: missing-trust (device is not trusted)"));
    assert!(stdout.contains("discovery: ranked=1"));
    assert!(stdout.contains("workspace: snap_right target=tablet-future cross_device=false"));
    assert!(stdout.contains("screen: unavailable (need at least 2 devices)"));
    assert!(stdout.contains("collaboration: unavailable (need at least 2 devices)"));
    let report = extract_simulation_report(&stdout);
    assert_eq!(
        report["simulators"]["screen"]["status"].as_str(),
        Some("unavailable")
    );
    assert_eq!(
        report["simulators"]["workspace"]["cross_device"].as_bool(),
        Some(false)
    );
}

#[test]
fn simulate_connection_planning_reports_reconnect_and_invalid_configuration() {
    let config_home = temp_config_home("simulate-connection-planning");
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
name = "trusted-no-address"
os = "macos"
role = "server"
display_name = "Trusted No Address"
trusted = true
x = 0
y = 0
width = 1728
height = 1117

[[device]]
name = "trusted-invalid-address"
os = "windows"
role = "client"
display_name = "Trusted Invalid Address"
address = "not-a-socket"
trusted = true
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
    assert!(stdout.contains(
        "Trusted No Address: reconnect-candidate (trusted device without address; wait for discovery)"
    ));
    assert!(stdout.contains(
        "Trusted Invalid Address: invalid-configuration (invalid address `not-a-socket` (expected ip:port))"
    ));
}

#[test]
fn simulate_json_only_outputs_machine_readable_report_only() {
    let config_home = temp_config_home("simulate-json-only");
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
trusted = true
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
        .arg("--simulate-json-only")
        .arg(sim_path)
        .output()
        .expect("run nexkvm simulate --simulate-json-only");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("simulation config:"));
    assert!(!stdout.contains("simulation_report_json:"));

    let report = extract_simulation_report(&stdout);
    assert_eq!(
        report["simulators"]["workspace"]["status"].as_str(),
        Some("ok")
    );
    assert_eq!(
        report["simulators"]["screen"]["status"].as_str(),
        Some("ok")
    );
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
