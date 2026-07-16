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
    assert!(stdout.contains("nexkvm pair-auto --peer <host:port>"));
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
        PublicKey(vec![1; 32]),
        [0u8; nexkvm_crypto::NONCE_LEN],
        "192.168.1.20:47654",
    );
    let uri = bootstrap.to_uri().unwrap();

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
    assert_ne!(
        first.nonce, second.nonce,
        "pairing nonces must be freshly random"
    );
}

#[test]
fn pairing_uri_rejects_addresses_the_other_mac_cannot_dial() {
    let config_home = temp_config_home("pairing-uri-unreachable-address");
    for address in [
        "127.0.0.1:47654",
        "[::1]:47654",
        "0.0.0.0:47654",
        "192.168.1.40:0",
        "studio-mac.local:47654",
    ] {
        let output = nexkvm()
            .env("XDG_CONFIG_HOME", &config_home)
            .args(["pairing-uri", address])
            .output()
            .expect("run nexkvm pairing-uri with an unreachable address");

        assert!(
            !output.status.success(),
            "pairing-uri unexpectedly accepted {address}"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("reachable non-loopback unicast IP:port"),
            "unexpected error for {address}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn doctor_reports_the_effective_required_pairing_policy() {
    use nexkvm_storage::Config;

    let config_home = temp_config_home("doctor-pairing-policy");
    let mut config = Config::default();
    config.security.require_pairing = false;
    config.save(config_home.join("nexkvm/config.toml")).unwrap();
    let output = nexkvm()
        .env("XDG_CONFIG_HOME", &config_home)
        .arg("doctor")
        .output()
        .expect("run nexkvm doctor");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("pairing policy: required"));
    assert!(!stdout.contains("require pairing: false"));
    assert!(!stdout.contains("opened settings"));
}

#[test]
fn doctor_reports_effective_tcp_and_warns_about_unsupported_preferences() {
    use nexkvm_storage::Config;

    let config_home = temp_config_home("doctor-effective-transport");
    let mut config = Config::default();
    config.network.transports = vec!["quic".into(), "tcp".into()];
    config.save(config_home.join("nexkvm/config.toml")).unwrap();

    let output = nexkvm()
        .env("XDG_CONFIG_HOME", &config_home)
        .arg("doctor")
        .output()
        .expect("run nexkvm doctor");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("effective transport: tcp"));
    assert!(stdout.contains("unsupported configured transports ignored: quic"));
    assert!(!stdout.contains("transports: quic,tcp"));
}

#[test]
fn pair_accept_persists_trusted_device() {
    use nexkvm_crypto::{PairingBootstrap, PublicKey};

    let config_home = temp_config_home("pair-accept");
    let bootstrap = PairingBootstrap::new(
        "trusted-mac",
        PublicKey(vec![9; 32]),
        [3u8; nexkvm_crypto::NONCE_LEN],
        "192.168.1.30:47654",
    );
    let uri = bootstrap.to_uri().unwrap();

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
fn file_send_queues_valid_sources_and_rejects_invalid_input() {
    use nexkvm_storage::Config;

    let config_home = temp_config_home("file-send");
    let app_dir = config_home.join("nexkvm");
    let config_path = app_dir.join("config.toml");
    let mut config = Config::default();
    config.file_transfer.enabled = true;
    config.file_transfer.max_entries = 8;
    config.file_transfer.max_transfer_bytes = 32;
    config.save(&config_path).unwrap();

    let source = config_home.join("share.txt");
    std::fs::write(&source, b"trusted bytes").unwrap();
    let output = nexkvm()
        .env("XDG_CONFIG_HOME", &config_home)
        .arg("file-send")
        .arg(&source)
        .output()
        .expect("queue file transfer");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("queued file transfer"));
    let queued = std::fs::read_dir(app_dir.join("file-transfer-queue"))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(queued.len(), 1);
    let record = std::fs::read_to_string(queued[0].path()).unwrap();
    assert!(record.contains("relative_path = \"share.txt\""));
    assert!(!record.contains("trusted bytes"));

    let missing = nexkvm()
        .env("XDG_CONFIG_HOME", &config_home)
        .args(["file-send", "does-not-exist"])
        .output()
        .expect("reject missing source");
    assert!(!missing.status.success());

    let empty = nexkvm()
        .env("XDG_CONFIG_HOME", &config_home)
        .arg("file-send")
        .output()
        .expect("reject empty source list");
    assert!(!empty.status.success());
}

#[test]
fn clipboard_history_cli_reads_and_clears_the_encrypted_archive() {
    use nexkvm_clipboard::ClipboardSnapshot;
    use nexkvm_core::DeviceId;
    use nexkvm_storage::{ClipboardHistoryArchive, ClipboardHistoryArchiveConfig, Config};

    let config_home = temp_config_home("clipboard-history");
    let app_dir = config_home.join("nexkvm");
    let config_path = app_dir.join("config.toml");
    let mut config = Config::default();
    config.clipboard.history_enabled = true;
    config.save(&config_path).unwrap();
    let archive_path = app_dir.join("clipboard-history.enc");
    let mut archive = ClipboardHistoryArchive::open(
        &archive_path,
        ClipboardHistoryArchiveConfig {
            capacity: config.clipboard.history_capacity,
            max_entry_bytes: config.clipboard.history_max_entry_bytes,
            max_archive_bytes: 32 * 1024 * 1024,
        },
    )
    .unwrap();
    assert!(archive.record(
        ClipboardSnapshot::from_text("history from a trusted peer"),
        DeviceId::generate(),
        42,
    ));
    archive.persist().unwrap();

    let output = nexkvm()
        .env("XDG_CONFIG_HOME", &config_home)
        .args(["clipboard-history", "--json"])
        .output()
        .expect("list clipboard history");
    assert!(output.status.success());
    let entries: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(entries.as_array().unwrap().len(), 1);
    assert_eq!(
        entries[0]["preview"],
        serde_json::Value::String("history from a trusted peer".into())
    );
    assert!(
        !std::fs::read(&archive_path)
            .unwrap()
            .windows(b"history from a trusted peer".len())
            .any(|window| window == b"history from a trusted peer")
    );

    let clear = nexkvm()
        .env("XDG_CONFIG_HOME", &config_home)
        .arg("clipboard-clear")
        .output()
        .expect("clear clipboard history");
    assert!(clear.status.success());
    let after = nexkvm()
        .env("XDG_CONFIG_HOME", &config_home)
        .args(["clipboard-history", "--json"])
        .output()
        .expect("list cleared history");
    assert!(after.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&after.stdout).unwrap(),
        serde_json::json!([])
    );
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

#[test]
fn checked_in_local_simulation_fixture_is_valid() {
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tools/sim/local-workspace.toml");
    let output = nexkvm()
        .arg("simulate")
        .arg(fixture)
        .arg("--simulate-json-only")
        .output()
        .expect("run the checked-in local simulation fixture");

    assert!(
        output.status.success(),
        "checked-in simulation fixture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
