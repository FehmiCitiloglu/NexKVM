# NexKVM

NexKVM is an open-source, cross-platform device continuity platform inspired by Barrier, Synergy, Apple Continuity, KDE Connect, Mouse Without Borders, and Universal Control.

The project is currently in a foundation phase: the repository emphasizes portable Rust models, safe trait boundaries, protocol/security contracts, and testable Sans-IO state machines before platform-specific native integrations land.

## Goals

- Share input, clipboard, files, audio, screens, workspace state, and collaboration surfaces across trusted devices.
- Prefer low-latency LAN behavior while supporting TCP fallback and future WebRTC remote mode.
- Keep platform-specific `unsafe` and native API work isolated in `crates/platform/*`.
- Require secure pairing, device trust, encrypted sessions, replay protection, and explicit permissions.
- Keep plugins sandboxed and least-privilege by default.

## Workspace Layout

- `apps/desktop`: desktop daemon and developer CLI.
- `apps/mobile_future`: future mobile companion placeholder.
- `crates/core`: device identity, event bus, platform traits, workspace, collaboration.
- `crates/protocol`: wire envelope, versioning, stream framing.
- `crates/crypto`: pairing, trust, session-security model.
- `crates/network`: QUIC/TCP/WebRTC planning, latency, packets, quality, sessions.
- `crates/input`: input events, topology, batching, prediction, polling.
- `crates/clipboard`: clipboard sync models.
- `crates/streaming`: file transfer, audio, screen streaming models.
- `crates/plugins`: plugin runtime, permissions, marketplace, hot reload.
- `crates/storage`: configuration and trust persistence.
- `crates/platform/*`: OS-specific safe backend boundaries.

## Quick Start

```sh
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features
cargo run -p nexkvm -- doctor
cargo run -p nexkvm -- protocol
cargo run -p nexkvm -- simulate tools/sim/local-workspace.toml
```

Running `cargo run -p nexkvm` with no subcommand starts the desktop daemon and waits for shutdown.

## Documentation

- [Architecture](docs/architecture.md)
- [Protocol](docs/protocol.md)
- [Security](docs/security.md)
- [Plugins](docs/plugins.md)
- [API Documentation](docs/api.md)
- [Tooling](docs/tooling.md)
- [Research Notes](docs/research/README.md)
- [Contributing](CONTRIBUTING.md)

## Current Phase

Many modules are intentionally pure models and trait boundaries. Native implementations for macOS Accessibility/Screen Recording, Wayland portals/PipeWire, Windows raw input/Graphics Capture, hardware encoders, and real plugin engines are phased behind the APIs already present in the workspace.
