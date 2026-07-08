//! Local Sans-IO simulation fixture parsing and validation.

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
        let address = device.address.parse().with_context(|| {
            format!(
                "device `{}` has invalid address `{}`",
                device.id, device.address
            )
        })?;
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
            bail!(
                "connection `{} -> {}` points to the same device",
                connection.from,
                connection.to
            );
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
    Ok(SimulationReport {
        devices,
        connections,
    })
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
        assert_eq!(report.devices[0].name, "Desk Linux");
        assert_eq!(report.devices[0].os, "linux");
        assert_eq!(report.devices[0].address, "127.0.0.1:4102".parse().unwrap());
        assert_eq!(report.devices[1].id, "laptop");
        assert_eq!(report.trusted_devices(), 2);
        assert_eq!(report.connections.len(), 1);
        assert_eq!(report.connections[0].from, "laptop");
        assert_eq!(report.connections[0].to, "desktop");
        assert_eq!(report.connections[0].status, ConnectionStatus::DirectLan);
        assert_eq!(report.connections[0].status.as_str(), "direct-lan");
    }

    #[test]
    fn load_report_reads_config_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("simulation.toml");
        std::fs::write(&path, VALID).unwrap();

        let report = load_report(&path).unwrap();

        assert_eq!(report.devices.len(), 2);
        assert_eq!(report.connections.len(), 1);
    }

    #[test]
    fn untrusted_device_blocks_connection() {
        let report = report_from_str(
            r#"
[[device]]
id = "laptop"
name = "Studio Laptop"
os = "macos"
address = "127.0.0.1:4101"
trusted = false

[[device]]
id = "desktop"
name = "Desk Linux"
os = "linux"
address = "127.0.0.1:4102"
trusted = true

[[connection]]
from = "laptop"
to = "desktop"
"#,
        )
        .unwrap();

        assert_eq!(
            report.connections[0].status,
            ConnectionStatus::BlockedByMissingTrust
        );
        assert_eq!(
            report.connections[0].status.as_str(),
            "blocked-missing-trust"
        );
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
