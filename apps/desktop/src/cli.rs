//! nexkvm developer CLI: argument parsing and pure output formatting.
//!
//! Kept dependency-free of the runtime (no I/O, no telemetry) so the parsing
//! and formatting logic is unit-testable. [`main`](crate) owns the side effects
//! (loading config, the trust store, and telemetry) and dispatches on the
//! [`Command`] this module produces.

use std::fmt::Write as _;

use nexkvm_core::NativeIntegrationReport;
use nexkvm_crypto::{PairingBootstrap, TrustEntry};

/// A parsed CLI subcommand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Run the desktop daemon (default when no subcommand is given).
    Run,
    /// Print local platform/config diagnostics.
    Doctor,
    /// Prompt/report platform permissions needed by native integrations.
    Permissions,
    /// Run a Linux Wayland portal input smoke diagnostic.
    PortalSmoke,
    /// Print protocol compatibility info.
    Protocol,
    /// Print the resolved config path.
    ConfigPath,
    /// List trusted (paired) devices.
    Devices,
    /// Decode and display a `nexkvm://` pairing bootstrap for confirmation.
    Pair {
        /// The scanned/pasted `nexkvm://pair/v1/...` URI.
        uri: String,
        /// Persist this peer into the local trust store after user confirmation.
        accept: bool,
    },
    /// Generate this device's pairing bootstrap URI.
    PairingUri {
        /// Address peers should dial (`ip:port`).
        addr: String,
    },
    /// Validate a local simulation config.
    Simulate {
        /// Optional path to the simulation TOML.
        path: Option<String>,
        /// Emit only machine-readable JSON output.
        json_only: bool,
    },
    /// Print CLI usage.
    Help,
}

/// A fully parsed CLI invocation: the subcommand plus global flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// The subcommand to run.
    pub command: Command,
    /// Whether `--debug` was passed (raises log verbosity for the daemon).
    pub debug: bool,
}

/// Parse CLI arguments (excluding the program name) into an [`Invocation`].
///
/// The global `--debug` flag may appear in any position. Unknown subcommands
/// produce an error string suitable for printing to stderr.
///
/// # Errors
/// Returns an error message when the subcommand is unknown or a required
/// argument (e.g. the pairing URI) is missing.
pub fn parse<I, S>(args: I) -> Result<Invocation, String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut positional = Vec::new();
    let mut debug = false;
    for arg in args {
        let arg = arg.into();
        if arg == "--debug" {
            debug = true;
        } else {
            positional.push(arg);
        }
    }

    let mut it = positional.into_iter();
    let command = match it.next().as_deref() {
        None => Command::Run,
        Some("doctor") => Command::Doctor,
        Some("permissions") => Command::Permissions,
        Some("portal-smoke") => {
            if it.next().is_some() {
                return Err("portal-smoke accepts no arguments".to_string());
            }
            Command::PortalSmoke
        }
        Some("protocol") => Command::Protocol,
        Some("config-path") => Command::ConfigPath,
        Some("devices") => Command::Devices,
        Some("pair") => parse_pair_args(it)?,
        Some("pairing-uri") => {
            let addr = it.next().ok_or_else(|| {
                "pairing-uri requires an address like 192.168.1.20:47654".to_string()
            })?;
            if it.next().is_some() {
                return Err("pairing-uri accepts one address".to_string());
            }
            Command::PairingUri { addr }
        }
        Some("simulate") => parse_simulate_args(it)?,
        Some("help" | "--help" | "-h") => Command::Help,
        Some(other) => return Err(format!("unknown command `{other}`; run `nexkvm help`")),
    };

    Ok(Invocation { command, debug })
}

fn parse_pair_args<I>(args: I) -> Result<Command, String>
where
    I: IntoIterator<Item = String>,
{
    let mut accept = false;
    let mut uri = None;
    for arg in args {
        if arg == "--accept" {
            accept = true;
        } else if uri.is_none() {
            uri = Some(arg);
        } else {
            return Err("pair accepts one nexkvm:// pairing uri".to_string());
        }
    }
    let uri = uri.ok_or_else(|| "pair requires a nexkvm:// pairing uri".to_string())?;
    Ok(Command::Pair { uri, accept })
}

fn parse_simulate_args<I>(args: I) -> Result<Command, String>
where
    I: IntoIterator<Item = String>,
{
    let mut json_only = false;
    let mut path = None;
    for arg in args {
        if arg == "--simulate-json-only" || arg == "--json-only" {
            json_only = true;
        } else if path.is_none() {
            path = Some(arg);
        } else {
            return Err(
                "simulate accepts at most one path plus optional --simulate-json-only".to_string(),
            );
        }
    }
    Ok(Command::Simulate { path, json_only })
}

/// Render the CLI usage text.
#[must_use]
pub fn help_text() -> String {
    let mut out = String::new();
    out.push_str("nexkvm developer CLI\n\n");
    out.push_str("USAGE:\n");
    out.push_str("  nexkvm [--debug]            Run the desktop daemon\n");
    out.push_str("  nexkvm devices             List trusted (paired) devices\n");
    out.push_str("  nexkvm pair [--accept] <uri> Decode or accept a pairing bootstrap\n");
    out.push_str("  nexkvm pairing-uri <addr>  Print this device's pairing bootstrap URI\n");
    out.push_str("  nexkvm permissions         Request/report required macOS permissions\n");
    out.push_str("  nexkvm portal-smoke       Test Linux Wayland portal grant/barrier/EIS flow\n");
    out.push_str("  nexkvm doctor              Print local platform/config diagnostics\n");
    out.push_str("  nexkvm protocol            Print protocol compatibility info\n");
    out.push_str("  nexkvm config-path         Print the resolved config path\n");
    out.push_str(
        "  nexkvm simulate [--simulate-json-only] [toml] Validate a local simulation config\n",
    );
    out.push_str("\nFLAGS:\n");
    out.push_str("  --debug                   Raise log verbosity to debug\n");
    out
}

/// Render the trusted-device list for `nexkvm devices`.
#[must_use]
pub fn format_device_list(entries: &[TrustEntry]) -> String {
    if entries.is_empty() {
        return "no trusted devices paired".to_string();
    }
    let mut out = format!("{} trusted device(s):\n", entries.len());
    for entry in entries {
        let _ = writeln!(
            out,
            "  {}  {}  (paired_at={})",
            entry.public_key.fingerprint(),
            entry.display_name,
            entry.paired_at,
        );
    }
    out.truncate(out.trim_end().len());
    out
}

/// Render a decoded pairing bootstrap for out-of-band fingerprint confirmation.
///
/// Decoding the URI is *not* trust: the user must compare the printed
/// fingerprint against the initiating device before accepting.
#[must_use]
pub fn format_pairing(bootstrap: &PairingBootstrap) -> String {
    format!(
        "pairing bootstrap\n  name: {}\n  addr: {}\n  fingerprint: {}\n\nConfirm this fingerprint matches the other device before accepting.",
        bootstrap.display_name,
        bootstrap.addr,
        bootstrap.public_key.fingerprint(),
    )
}

/// Render a persisted pairing acceptance.
#[must_use]
pub fn format_pairing_accepted(entry: &TrustEntry) -> String {
    format!(
        "trusted device accepted\n  name: {}\n  fingerprint: {}\n  paired_at: {}",
        entry.display_name,
        entry.public_key.fingerprint(),
        entry.paired_at,
    )
}

/// Render native integration availability for `nexkvm doctor`.
#[must_use]
pub fn format_native_integrations(report: &NativeIntegrationReport) -> String {
    let mut out = format!("native integrations: {:?}\n", report.os);
    for entry in &report.integrations {
        let _ = writeln!(
            out,
            "  {}: {}",
            entry.integration.label(),
            entry.status.label()
        );
    }
    out.truncate(out.trim_end().len());
    out
}

/// Render macOS input permission details for `nexkvm doctor`.
#[cfg(any(target_os = "macos", test))]
#[must_use]
pub fn format_macos_input_report(
    accessibility: &str,
    can_capture_input: bool,
    can_inject_input: bool,
    next_step: Option<&str>,
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "macOS input accessibility: {accessibility}");
    let _ = writeln!(out, "  capture ready: {can_capture_input}");
    let _ = writeln!(out, "  inject ready: {can_inject_input}");
    let _ = writeln!(
        out,
        "  settings: System Settings > Privacy & Security > Accessibility"
    );
    if let Some(next_step) = next_step {
        let _ = writeln!(out, "  next step: {next_step}");
    }
    if !can_capture_input || !can_inject_input {
        let _ = writeln!(
            out,
            "  after granting permission: restart nexkvm after granting permission"
        );
    }
    out.truncate(out.trim_end().len());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexkvm_crypto::PublicKey;

    fn entry(name: &str, key: &[u8], paired_at: u64) -> TrustEntry {
        TrustEntry {
            display_name: name.into(),
            public_key: PublicKey(key.to_vec()),
            paired_at,
        }
    }

    #[test]
    fn no_args_runs_the_daemon() {
        let parsed = parse(Vec::<String>::new()).unwrap();
        assert_eq!(parsed.command, Command::Run);
        assert!(!parsed.debug);
    }

    #[test]
    fn debug_flag_is_position_independent() {
        let before = parse(["--debug", "devices"]).unwrap();
        let after = parse(["devices", "--debug"]).unwrap();
        assert_eq!(before.command, Command::Devices);
        assert_eq!(after.command, Command::Devices);
        assert!(before.debug && after.debug);
    }

    #[test]
    fn pair_requires_a_uri() {
        assert!(parse(["pair"]).is_err());
        let parsed = parse(["pair", "nexkvm://pair/v1/00"]).unwrap();
        assert_eq!(
            parsed.command,
            Command::Pair {
                uri: "nexkvm://pair/v1/00".into(),
                accept: false,
            }
        );
    }

    #[test]
    fn pair_accept_flag_is_position_independent() {
        let before = parse(["pair", "--accept", "nexkvm://pair/v1/00"]).unwrap();
        let after = parse(["pair", "nexkvm://pair/v1/00", "--accept"]).unwrap();
        assert_eq!(
            before.command,
            Command::Pair {
                uri: "nexkvm://pair/v1/00".into(),
                accept: true,
            }
        );
        assert_eq!(before, after);
    }

    #[test]
    fn pairing_uri_requires_one_addr() {
        assert!(parse(["pairing-uri"]).is_err());
        assert!(parse(["pairing-uri", "a", "b"]).is_err());
        assert_eq!(
            parse(["pairing-uri", "192.168.1.40:47654"])
                .unwrap()
                .command,
            Command::PairingUri {
                addr: "192.168.1.40:47654".into()
            }
        );
    }

    #[test]
    fn unknown_command_is_rejected() {
        assert!(parse(["frobnicate"]).is_err());
    }

    #[test]
    fn permissions_command_is_parsed() {
        assert_eq!(
            parse(["permissions"]).unwrap().command,
            Command::Permissions
        );
        assert!(help_text().contains("nexkvm permissions"));
    }

    #[test]
    fn portal_smoke_command_is_parsed() {
        assert_eq!(
            parse(["portal-smoke"]).unwrap().command,
            Command::PortalSmoke
        );
        assert!(help_text().contains("nexkvm portal-smoke"));
    }

    #[test]
    fn simulate_takes_optional_path() {
        assert_eq!(
            parse(["simulate"]).unwrap().command,
            Command::Simulate {
                path: None,
                json_only: false,
            }
        );
        assert_eq!(
            parse(["simulate", "a.toml"]).unwrap().command,
            Command::Simulate {
                path: Some("a.toml".into()),
                json_only: false,
            }
        );
    }

    #[test]
    fn simulate_json_only_flag_is_supported() {
        assert_eq!(
            parse(["simulate", "--simulate-json-only"]).unwrap().command,
            Command::Simulate {
                path: None,
                json_only: true,
            }
        );
        assert_eq!(
            parse(["simulate", "--json-only", "a.toml"])
                .unwrap()
                .command,
            Command::Simulate {
                path: Some("a.toml".into()),
                json_only: true,
            }
        );
        assert!(parse(["simulate", "a.toml", "b.toml"]).is_err());
    }

    #[test]
    fn empty_device_list_is_reported() {
        assert_eq!(format_device_list(&[]), "no trusted devices paired");
    }

    #[test]
    fn device_list_includes_fingerprint_and_name() {
        let entries = vec![
            entry("laptop", &[1, 2, 3], 1_700_000_000),
            entry("phone", &[9, 9], 1_700_000_500),
        ];
        let rendered = format_device_list(&entries);
        assert!(rendered.starts_with("2 trusted device(s):"));
        assert!(rendered.contains("laptop"));
        assert!(rendered.contains("phone"));
        assert!(rendered.contains(&entries[0].public_key.fingerprint()));
        // No trailing blank line.
        assert!(!rendered.ends_with('\n'));
    }

    #[test]
    fn pairing_summary_shows_fingerprint() {
        let bootstrap = PairingBootstrap::new(
            "studio-mac",
            PublicKey(vec![7, 7, 7, 7]),
            [0u8; nexkvm_crypto::NONCE_LEN],
            "192.168.1.5:47654",
        );
        let rendered = format_pairing(&bootstrap);
        assert!(rendered.contains("studio-mac"));
        assert!(rendered.contains("192.168.1.5:47654"));
        assert!(rendered.contains(&bootstrap.public_key.fingerprint()));
    }

    #[test]
    fn pairing_acceptance_summary_shows_persisted_entry() {
        let entry = entry("studio-mac", &[7, 7, 7, 7], 1_700_000_000);
        let rendered = format_pairing_accepted(&entry);
        assert!(rendered.contains("trusted device accepted"));
        assert!(rendered.contains("studio-mac"));
        assert!(rendered.contains(&entry.public_key.fingerprint()));
        assert!(
            rendered.contains("paired_at=1700000000") || rendered.contains("paired_at: 1700000000")
        );
    }

    #[test]
    fn native_integration_report_is_formatted_for_doctor() {
        use nexkvm_core::{
            NativeIntegration, NativeIntegrationAvailability, NativeIntegrationReport,
            NativeIntegrationStatus, OsKind,
        };

        let rendered = format_native_integrations(&NativeIntegrationReport {
            os: OsKind::MacOs,
            integrations: vec![
                NativeIntegrationAvailability {
                    integration: NativeIntegration::InputCapture,
                    status: NativeIntegrationStatus::PermissionRequired,
                },
                NativeIntegrationAvailability {
                    integration: NativeIntegration::Clipboard,
                    status: NativeIntegrationStatus::Unsupported,
                },
            ],
        });

        assert!(rendered.contains("native integrations: MacOs"));
        assert!(rendered.contains("input-capture: permission-required"));
        assert!(rendered.contains("clipboard: unsupported"));
    }

    #[test]
    fn macos_input_report_includes_next_step_when_permission_missing() {
        let rendered = format_macos_input_report(
            "permission-required",
            false,
            false,
            Some(
                "Grant Accessibility permission in System Settings > Privacy & Security > Accessibility",
            ),
        );

        assert!(rendered.contains("macOS input accessibility: permission-required"));
        assert!(rendered.contains("capture ready: false"));
        assert!(rendered.contains("inject ready: false"));
        assert!(rendered.contains("Grant Accessibility permission"));
        assert!(rendered.contains("System Settings"));
        assert!(rendered.contains("restart nexkvm after granting permission"));
    }
}
