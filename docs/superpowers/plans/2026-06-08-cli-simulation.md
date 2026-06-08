# CLI Simulation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `coklu simulate [toml]` parse a typed local workspace fixture, validate devices/connections, and print a deterministic simulation report.

**Architecture:** Add a focused `apps/desktop/src/simulation.rs` module for TOML parsing, validation, and report data. Keep `apps/desktop/src/cli.rs` as the pure renderer and `apps/desktop/src/main.rs` as the I/O dispatcher. Update the sample fixture and CLI integration tests to lock the user-facing output.

**Tech Stack:** Rust 2024, Cargo workspace, `serde`, `toml`, `anyhow`, existing `coklu` desktop binary tests.

---

## File Structure

- Create `apps/desktop/src/simulation.rs`: typed simulation model, validation, `load_report`, and unit tests.
- Modify `apps/desktop/src/main.rs`: declare `mod simulation;` and call `simulation::load_report`.
- Modify `apps/desktop/src/cli.rs`: add `format_simulation_report(path, report)` renderer and renderer unit tests.
- Modify `apps/desktop/Cargo.toml`: add `serde.workspace = true` and `toml.workspace = true`.
- Modify `apps/desktop/tests/cli.rs`: add end-to-end `simulate` smoke test.
- Modify `tools/sim/local-workspace.toml`: add `id`, `address`, `trusted`, and `[[connection]]` fixtures.

### Task 1: Simulation Config Model And Validation

**Files:**
- Create: `apps/desktop/src/simulation.rs`
- Modify: `apps/desktop/Cargo.toml`

- [ ] **Step 1: Add dependencies**

In `apps/desktop/Cargo.toml`, add these lines inside `[dependencies]`:

```toml
serde.workspace = true
toml.workspace = true
```

- [ ] **Step 2: Write failing validation tests**

Create `apps/desktop/src/simulation.rs` with tests first:

```rust
//! Local Sans-IO simulation fixture parsing and validation.

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
[[device]]
id = "laptop"
name = "Studio Laptop"
os = "macos"
address = "127.0.0.1:4101"
trusted = true

[[device]]
id = "desktop"
name = "Desk Linux"
os = "linux"
address = "127.0.0.1:4102"
trusted = true

[[connection]]
from = "laptop"
to = "desktop"
"#;

    #[test]
    fn valid_config_builds_a_report() {
        let report = report_from_str(VALID).unwrap();

        assert_eq!(report.devices.len(), 2);
        assert_eq!(report.devices[0].id, "desktop");
        assert_eq!(report.devices[1].id, "laptop");
        assert_eq!(report.trusted_devices(), 2);
        assert_eq!(report.connections.len(), 1);
        assert_eq!(report.connections[0].from, "laptop");
        assert_eq!(report.connections[0].to, "desktop");
        assert_eq!(report.connections[0].status, ConnectionStatus::DirectLan);
    }

    #[test]
    fn empty_device_list_is_rejected() {
        let err = report_from_str("").unwrap_err().to_string();
        assert!(err.contains("at least one device"));
    }

    #[test]
    fn duplicate_device_id_is_rejected() {
        let err = report_from_str(
            r#"
[[device]]
id = "laptop"
name = "A"
os = "macos"
address = "127.0.0.1:4101"

[[device]]
id = "laptop"
name = "B"
os = "linux"
address = "127.0.0.1:4102"
"#,
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("duplicate device id `laptop`"));
    }

    #[test]
    fn unknown_connection_target_is_rejected() {
        let err = report_from_str(
            r#"
[[device]]
id = "laptop"
name = "Studio Laptop"
os = "macos"
address = "127.0.0.1:4101"

[[connection]]
from = "laptop"
to = "tablet"
"#,
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("connection `laptop -> tablet` references unknown device `tablet`"));
    }
}
```

- [ ] **Step 3: Run tests and verify RED**

Run:

```sh
cargo test -p coklu simulation::tests --all-features
```

Expected: compile failure because `report_from_str`, `ConnectionStatus`, and report types are not defined.

- [ ] **Step 4: Implement the minimal parser and validator**

Add this implementation above the test module in `apps/desktop/src/simulation.rs`:

```rust
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::Path;

use anyhow::{Context, bail};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct SimulationConfig {
    #[serde(default)]
    device: Vec<SimulatedDeviceConfig>,
    #[serde(default)]
    connection: Vec<SimulatedConnectionConfig>,
}

#[derive(Debug, Deserialize)]
struct SimulatedDeviceConfig {
    id: String,
    name: String,
    os: String,
    address: String,
    #[serde(default)]
    trusted: bool,
}

#[derive(Debug, Deserialize)]
struct SimulatedConnectionConfig {
    from: String,
    to: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimulationReport {
    pub devices: Vec<SimulatedDevice>,
    pub connections: Vec<ConnectionPlan>,
}

impl SimulationReport {
    #[must_use]
    pub fn trusted_devices(&self) -> usize {
        self.devices.iter().filter(|device| device.trusted).count()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimulatedDevice {
    pub id: String,
    pub name: String,
    pub os: String,
    pub address: SocketAddr,
    pub trusted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionPlan {
    pub from: String,
    pub to: String,
    pub status: ConnectionStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionStatus {
    DirectLan,
    BlockedByMissingTrust,
}

impl ConnectionStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DirectLan => "direct-lan",
            Self::BlockedByMissingTrust => "blocked-missing-trust",
        }
    }
}

pub fn load_report(path: impl AsRef<Path>) -> anyhow::Result<SimulationReport> {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading simulation config from {}", path.display()))?;
    report_from_str(&text)
}

pub fn report_from_str(text: &str) -> anyhow::Result<SimulationReport> {
    let config: SimulationConfig = toml::from_str(text).context("parsing simulation TOML")?;
    build_report(config)
}

fn build_report(config: SimulationConfig) -> anyhow::Result<SimulationReport> {
    if config.device.is_empty() {
        bail!("simulation config must define at least one device");
    }

    let mut seen = HashSet::new();
    let mut devices = Vec::with_capacity(config.device.len());

    for device in config.device {
        if device.id.trim().is_empty() {
            bail!("device id must not be empty");
        }
        if !seen.insert(device.id.clone()) {
            bail!("duplicate device id `{}`", device.id);
        }
        let address = device
            .address
            .parse()
            .with_context(|| format!("device `{}` has invalid address `{}`", device.id, device.address))?;
        devices.push(SimulatedDevice {
            id: device.id,
            name: device.name,
            os: device.os,
            address,
            trusted: device.trusted,
        });
    }

    devices.sort_by(|a, b| a.id.cmp(&b.id));
    let by_id: HashMap<_, _> = devices
        .iter()
        .map(|device| (device.id.as_str(), device.trusted))
        .collect();

    let mut connections = Vec::with_capacity(config.connection.len());
    for connection in config.connection {
        validate_endpoint(&by_id, &connection.from, &connection.from, &connection.to)?;
        validate_endpoint(&by_id, &connection.to, &connection.from, &connection.to)?;
        if connection.from == connection.to {
            bail!("connection `{} -> {}` points to the same device", connection.from, connection.to);
        }

        let from_trusted = by_id[connection.from.as_str()];
        let to_trusted = by_id[connection.to.as_str()];
        let status = if from_trusted && to_trusted {
            ConnectionStatus::DirectLan
        } else {
            ConnectionStatus::BlockedByMissingTrust
        };

        connections.push(ConnectionPlan {
            from: connection.from,
            to: connection.to,
            status,
        });
    }

    connections.sort_by(|a, b| a.from.cmp(&b.from).then_with(|| a.to.cmp(&b.to)));
    Ok(SimulationReport { devices, connections })
}

fn validate_endpoint(
    by_id: &HashMap<&str, bool>,
    endpoint: &str,
    from: &str,
    to: &str,
) -> anyhow::Result<()> {
    if by_id.contains_key(endpoint) {
        Ok(())
    } else {
        bail!("connection `{from} -> {to}` references unknown device `{endpoint}`");
    }
}
```

- [ ] **Step 5: Run tests and verify GREEN**

Run:

```sh
cargo test -p coklu simulation::tests --all-features
```

Expected: all `simulation::tests` pass.

- [ ] **Step 6: Commit Task 1**

Run:

```sh
git add apps/desktop/Cargo.toml apps/desktop/src/simulation.rs
git commit -m "feat: add simulation config validation"
```

### Task 2: Stable CLI Renderer

**Files:**
- Modify: `apps/desktop/src/cli.rs`

- [ ] **Step 1: Write failing renderer test**

In `apps/desktop/src/cli.rs`, add this import near the existing imports:

```rust
use crate::simulation::{ConnectionPlan, ConnectionStatus, SimulatedDevice, SimulationReport};
```

Then add this test inside `#[cfg(test)] mod tests`:

```rust
    #[test]
    fn simulation_report_is_rendered_without_trailing_blank_line() {
        let report = SimulationReport {
            devices: vec![
                SimulatedDevice {
                    id: "desktop".into(),
                    name: "Desk Linux".into(),
                    os: "linux".into(),
                    address: "127.0.0.1:4102".parse().unwrap(),
                    trusted: true,
                },
                SimulatedDevice {
                    id: "laptop".into(),
                    name: "Studio Laptop".into(),
                    os: "macos".into(),
                    address: "127.0.0.1:4101".parse().unwrap(),
                    trusted: false,
                },
            ],
            connections: vec![ConnectionPlan {
                from: "laptop".into(),
                to: "desktop".into(),
                status: ConnectionStatus::BlockedByMissingTrust,
            }],
        };

        let rendered = format_simulation_report("tools/sim/local-workspace.toml", &report);

        assert!(rendered.contains("coklu simulation"));
        assert!(rendered.contains("config: tools/sim/local-workspace.toml"));
        assert!(rendered.contains("  - desktop (linux) trusted address=127.0.0.1:4102"));
        assert!(rendered.contains("  - laptop (macos) untrusted address=127.0.0.1:4101"));
        assert!(rendered.contains("  - laptop -> desktop: blocked-missing-trust"));
        assert!(rendered.contains("summary: 2 devices, 1 trusted, 1 planned connection"));
        assert!(!rendered.ends_with('\n'));
    }
```

- [ ] **Step 2: Run test and verify RED**

Run:

```sh
cargo test -p coklu cli::tests::simulation_report_is_rendered_without_trailing_blank_line --all-features
```

Expected: compile failure because `format_simulation_report` is not defined.

- [ ] **Step 3: Implement the renderer**

In `apps/desktop/src/cli.rs`, add this function after `format_pairing`:

```rust
#[must_use]
pub fn format_simulation_report(path: &str, report: &SimulationReport) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "coklu simulation");
    let _ = writeln!(out, "config: {path}");
    let _ = writeln!(out);
    let _ = writeln!(out, "devices:");
    for device in &report.devices {
        let trust = if device.trusted { "trusted" } else { "untrusted" };
        let _ = writeln!(
            out,
            "  - {} ({}) {} address={}",
            device.id, device.os, trust, device.address
        );
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "connections:");
    if report.connections.is_empty() {
        let _ = writeln!(out, "  - none");
    } else {
        for connection in &report.connections {
            let _ = writeln!(
                out,
                "  - {} -> {}: {}",
                connection.from,
                connection.to,
                connection.status.as_str()
            );
        }
    }
    let connection_label = if report.connections.len() == 1 {
        "planned connection"
    } else {
        "planned connections"
    };
    let _ = writeln!(
        out,
        "\nsummary: {} devices, {} trusted, {} {}",
        report.devices.len(),
        report.trusted_devices(),
        report.connections.len(),
        connection_label
    );
    out.truncate(out.trim_end().len());
    out
}
```

- [ ] **Step 4: Run test and verify GREEN**

Run:

```sh
cargo test -p coklu cli::tests::simulation_report_is_rendered_without_trailing_blank_line --all-features
```

Expected: the renderer test passes.

- [ ] **Step 5: Commit Task 2**

Run:

```sh
git add apps/desktop/src/cli.rs
git commit -m "feat: render simulation reports"
```

### Task 3: Wire The CLI Command

**Files:**
- Modify: `apps/desktop/src/main.rs`

- [ ] **Step 1: Write failing integration test**

In `apps/desktop/tests/cli.rs`, add this test:

```rust
#[test]
fn simulate_reports_devices_and_connections() {
    let output = coklu()
        .args(["simulate", "tools/sim/local-workspace.toml"])
        .output()
        .expect("run coklu simulate");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("coklu simulation"));
    assert!(stdout.contains("devices:"));
    assert!(stdout.contains("connections:"));
    assert!(stdout.contains("summary:"));
}
```

- [ ] **Step 2: Run test and verify RED**

Run:

```sh
cargo test -p coklu --test cli simulate_reports_devices_and_connections --all-features
```

Expected: failure because the current fixture lacks required fields and/or the command still prints the old scaffold output.

- [ ] **Step 3: Wire simulation module into main**

In `apps/desktop/src/main.rs`, add:

```rust
mod simulation;
```

Replace the current `simulate` function with:

```rust
fn simulate(path: Option<String>) -> anyhow::Result<()> {
    let path = path.unwrap_or_else(|| "tools/sim/local-workspace.toml".into());
    let report = simulation::load_report(&path)
        .with_context(|| format!("loading simulation report from {path}"))?;
    println!("{}", cli::format_simulation_report(&path, &report));
    Ok(())
}
```

- [ ] **Step 4: Run test and confirm fixture failure**

Run:

```sh
cargo test -p coklu --test cli simulate_reports_devices_and_connections --all-features
```

Expected: still fails with a clear validation error for missing required fixture fields.

- [ ] **Step 5: Keep Task 3 changes uncommitted until the fixture is upgraded**

Do not commit after this step. The CLI is wired, but the checked-in fixture is
not valid for the new schema yet, so the focused integration test is still red.
Carry these files into Task 4:

```text
apps/desktop/src/main.rs
apps/desktop/tests/cli.rs
```

### Task 4: Upgrade The Local Fixture

**Files:**
- Modify: `tools/sim/local-workspace.toml`

- [ ] **Step 1: Update fixture with ids, addresses, trust, and connections**

Replace `tools/sim/local-workspace.toml` with:

```toml
# Local sans-IO simulation environment for coklu developer workflows.
# The desktop CLI validates and summarizes this file today; later phases can
# feed it into simulated discovery, latency, workspace, and collaboration flows.

[network]
profile = "lan"
rtt_ms = 8
jitter_ms = 1
loss = 0.0
throughput_bps = 100000000

[[device]]
id = "desk-macos"
name = "desk-macos"
os = "macos"
role = "server"
address = "127.0.0.1:4101"
trusted = true
x = 0
y = 0
width = 1728
height = 1117

[[device]]
id = "laptop-linux"
name = "laptop-linux"
os = "linux-wayland"
role = "client"
address = "127.0.0.1:4102"
trusted = true
x = 1728
y = 0
width = 1920
height = 1080

[[device]]
id = "tablet-future"
name = "tablet-future"
os = "android"
role = "client"
address = "127.0.0.1:4103"
trusted = false
x = -1200
y = 100
width = 1200
height = 800

[[connection]]
from = "desk-macos"
to = "laptop-linux"

[[connection]]
from = "laptop-linux"
to = "desk-macos"

[[connection]]
from = "tablet-future"
to = "desk-macos"

[features]
clipboard = true
file_transfer = true
screen_preview = true
shared_cursor = true
plugins = false
```

- [ ] **Step 2: Run integration test and verify GREEN**

Run:

```sh
cargo test -p coklu --test cli simulate_reports_devices_and_connections --all-features
```

Expected: test passes.

- [ ] **Step 3: Strengthen integration assertions**

Update the integration test to assert concrete output:

```rust
#[test]
fn simulate_reports_devices_and_connections() {
    let output = coklu()
        .args(["simulate", "tools/sim/local-workspace.toml"])
        .output()
        .expect("run coklu simulate");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("coklu simulation"));
    assert!(stdout.contains("  - desk-macos (macos) trusted address=127.0.0.1:4101"));
    assert!(stdout.contains("  - laptop-linux (linux-wayland) trusted address=127.0.0.1:4102"));
    assert!(stdout.contains("  - tablet-future (android) untrusted address=127.0.0.1:4103"));
    assert!(stdout.contains("  - desk-macos -> laptop-linux: direct-lan"));
    assert!(stdout.contains("  - laptop-linux -> desk-macos: direct-lan"));
    assert!(stdout.contains("  - tablet-future -> desk-macos: blocked-missing-trust"));
    assert!(stdout.contains("summary: 3 devices, 2 trusted, 3 planned connections"));
}
```

- [ ] **Step 4: Run test and verify GREEN**

Run:

```sh
cargo test -p coklu --test cli simulate_reports_devices_and_connections --all-features
```

Expected: test passes with concrete fixture output.

- [ ] **Step 5: Commit Task 4**

Run:

```sh
git add apps/desktop/src/main.rs apps/desktop/tests/cli.rs tools/sim/local-workspace.toml
git commit -m "feat: wire local simulation command"
```

### Task 5: Focused And Workspace Verification

**Files:**
- No code edits unless verification finds a real issue.

- [ ] **Step 1: Run focused desktop tests**

Run:

```sh
cargo test -p coklu --all-features
```

Expected: all desktop package tests pass.

- [ ] **Step 2: Run formatting check**

Run:

```sh
cargo fmt --all -- --check
```

Expected: no formatting diff.

If formatting fails only due to new files, run:

```sh
cargo fmt --all
cargo fmt --all -- --check
```

- [ ] **Step 3: Run broader workspace tests**

Run:

```sh
cargo test --workspace --all-features
```

Expected: all tests pass. If UDP discovery tests fail with `Operation not permitted`, record it as a sandbox limitation and run focused non-network tests before finalizing.

- [ ] **Step 4: Commit formatting-only changes if any**

Run this only if `cargo fmt --all` changed files:

```sh
git add apps/desktop/src/cli.rs apps/desktop/src/main.rs apps/desktop/src/simulation.rs apps/desktop/tests/cli.rs tools/sim/local-workspace.toml
git commit -m "style: format simulation changes"
```

### Task 6: Final Review

**Files:**
- Inspect changed files only.

- [ ] **Step 1: Check changed file scope**

Run:

```sh
git status --short
git diff --stat HEAD~3..HEAD
```

Expected: only simulation-related files changed by these tasks, plus any unrelated pre-existing worktree changes remain unstaged.

- [ ] **Step 2: Review CLI output manually**

Run:

```sh
cargo run -p coklu -- simulate tools/sim/local-workspace.toml
```

Expected output contains:

```text
coklu simulation
config: tools/sim/local-workspace.toml
devices:
connections:
summary: 3 devices, 2 trusted, 3 planned connections
```

- [ ] **Step 3: Final response**

Report:

- Commits created.
- Tests run and whether they passed.
- Any sandbox limitations.
- The exact command the user can run to see the feature.
