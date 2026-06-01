//! Telemetry: structured logging & tracing.
//!
//! coklu standardizes on the [`tracing`] ecosystem so spans flow across async
//! tasks (connection handling, input pipelines) with correlatable context.
//! [`init`] wires up a subscriber once at process start; library crates emit
//! events with the `tracing` macros and never configure a subscriber
//! themselves.
//!
//! # Privacy
//! Telemetry is **local-only by default** — logs go to stderr/files, never the
//! network. Crypto material is excluded from logs by construction (see the
//! redacted `Debug` impls in `coklu-crypto`).

use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

/// Errors initializing telemetry.
#[derive(Debug, Error)]
pub enum TelemetryError {
    /// A subscriber was already installed (double-init).
    #[error("telemetry already initialized")]
    AlreadyInitialized,

    /// The configured filter directive was invalid.
    #[error("invalid log filter: {0}")]
    InvalidFilter(String),
}

/// Verbosity floor when no `RUST_LOG`/`COKLU_LOG` override is present.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    /// Errors only.
    Error,
    /// Warnings and above.
    Warn,
    /// Informational (default).
    #[default]
    Info,
    /// Debug and above.
    Debug,
    /// Everything.
    Trace,
}

impl LogLevel {
    fn as_directive(self) -> &'static str {
        match self {
            LogLevel::Error => "error",
            LogLevel::Warn => "warn",
            LogLevel::Info => "info",
            LogLevel::Debug => "debug",
            LogLevel::Trace => "trace",
        }
    }
}

/// Telemetry configuration (mirrors the `[telemetry]` config section).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryConfig {
    /// Default verbosity when no env override is set.
    #[serde(default)]
    pub level: LogLevel,
    /// Include source file/line in log output.
    #[serde(default)]
    pub with_location: bool,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            level: LogLevel::Info,
            with_location: false,
        }
    }
}

/// Initialize the global tracing subscriber.
///
/// Filter precedence: `COKLU_LOG` env var, then `RUST_LOG`, then the configured
/// [`LogLevel`]. With the `json` feature the output is structured JSON;
/// otherwise it is human-readable.
///
/// # Errors
/// Returns [`TelemetryError`] if a subscriber is already installed or the env
/// filter is malformed.
pub fn init(config: &TelemetryConfig) -> Result<(), TelemetryError> {
    let filter = build_filter(config)?;

    let fmt_layer = fmt::layer()
        .with_target(true)
        .with_file(config.with_location)
        .with_line_number(config.with_location);

    #[cfg(feature = "json")]
    let fmt_layer = fmt_layer.json();

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .try_init()
        .map_err(|_| TelemetryError::AlreadyInitialized)
}

fn build_filter(config: &TelemetryConfig) -> Result<EnvFilter, TelemetryError> {
    if let Ok(directive) = std::env::var("COKLU_LOG").or_else(|_| std::env::var("RUST_LOG")) {
        return EnvFilter::from_str(&directive)
            .map_err(|e| TelemetryError::InvalidFilter(e.to_string()));
    }
    EnvFilter::from_str(config.level.as_directive())
        .map_err(|e| TelemetryError::InvalidFilter(e.to_string()))
}
