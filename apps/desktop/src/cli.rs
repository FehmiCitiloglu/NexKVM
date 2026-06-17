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
    },
    /// Validate a local simulation config.
    Simulate {
        /// Optional path to the simulation TOML.
        path: Option<String>,
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
        Some("protocol") => Command::Protocol,
        Some("config-path") => Command::ConfigPath,
        Some("devices") => Command::Devices,
        Some("pair") => {
            let uri = it
                .next()
                .ok_or_else(|| "pair requires a nexkvm:// pairing uri".to_string())?;
            Command::Pair { uri }
        }
        Some("simulate") => Command::Simulate { path: it.next() },
        Some("help" | "--help" | "-h") => Command::Help,
        Some(other) => return Err(format!("unknown command `{other}`; run `nexkvm help`")),
    };

    Ok(Invocation { command, debug })
}

/// Render the CLI usage text.
#[must_use]
pub fn help_text() -> String {
    let mut out = String::new();
    out.push_str("nexkvm developer CLI\n\n");
    out.push_str("USAGE:\n");
    out.push_str("  nexkvm [--debug]            Run the desktop daemon\n");
    out.push_str("  nexkvm devices             List trusted (paired) devices\n");
    out.push_str("  nexkvm pair <uri>          Decode a nexkvm:// pairing bootstrap\n");
    out.push_str("  nexkvm doctor              Print local platform/config diagnostics\n");
    out.push_str("  nexkvm protocol            Print protocol compatibility info\n");
    out.push_str("  nexkvm config-path         Print the resolved config path\n");
    out.push_str("  nexkvm simulate [toml]     Validate a local simulation config\n");
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
    if let Some(next_step) = next_step {
        let _ = writeln!(out, "  next step: {next_step}");
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
                uri: "nexkvm://pair/v1/00".into()
            }
        );
    }

    #[test]
    fn unknown_command_is_rejected() {
        assert!(parse(["frobnicate"]).is_err());
    }

    #[test]
    fn simulate_takes_optional_path() {
        assert_eq!(
            parse(["simulate"]).unwrap().command,
            Command::Simulate { path: None }
        );
        assert_eq!(
            parse(["simulate", "a.toml"]).unwrap().command,
            Command::Simulate {
                path: Some("a.toml".into())
            }
        );
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
    }
}
