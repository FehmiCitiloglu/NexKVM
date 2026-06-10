# Contributing

Thanks for helping build NexKVM. The project is still in a foundation phase, so high-quality models, boundaries, tests, and documentation matter as much as native integrations.

## Local Setup

Install Rust 1.88 or newer. Then run:

```sh
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features
cargo fmt --all -- --check
```

Useful developer commands:

```sh
cargo run -p nexkvm -- doctor
cargo run -p nexkvm -- protocol
cargo run -p nexkvm -- simulate tools/sim/local-workspace.toml
```

## Development Principles

- Keep platform-specific code in `crates/platform/*` behind safe traits.
- Prefer Sans-IO state machines for policy, routing, negotiation, and scheduling.
- Keep async APIs async-first; never block Tokio on native I/O or heavy CPU work.
- Use `bytes::Bytes` for network/protocol payloads when zero-copy fan-out matters.
- Add feature flags for optional heavy integrations.
- Keep permission and trust boundaries explicit.
- Avoid broad refactors unless they are necessary for the task.

## Security Expectations

Do not add insecure shortcuts for convenience. Features that cross devices must respect:

- secure pairing,
- device authentication,
- encrypted sessions,
- replay prevention,
- explicit permissions,
- sandboxed plugins where applicable.

Unknown future events/messages should fail closed at sensitive boundaries.

## Tests

Use focused tests at the crate that owns the behavior:

- Unit tests for pure models and state machines.
- `#[tokio::test]` for async behavior.
- Integration tests under member-crate `tests/` directories for cross-crate behavior.
- Fuzz targets for peer-supplied bytes and protocol boundaries.
- Benchmarks for latency-sensitive paths.

Current integration/fuzz/bench entrypoints:

```sh
cargo test -p nexkvm-network --test protocol_pipeline
cargo check --manifest-path fuzz/Cargo.toml --bins
cargo bench -p nexkvm-network --bench latency_suite
```

## Documentation

When changing behavior or public APIs, update relevant docs:

- `docs/architecture.md`
- `docs/protocol.md`
- `docs/security.md`
- `docs/plugins.md`
- `docs/api.md`
- crate-level `//!` docs and public `///` docs

Build docs locally with:

```sh
cargo doc --workspace --all-features --no-deps
```

## Pull Request Checklist

Before opening a PR, run:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo doc --workspace --all-features --no-deps
```

For protocol/security/network changes, also consider:

```sh
cargo check --manifest-path fuzz/Cargo.toml --bins
cargo bench -p nexkvm-network --bench latency_suite
```

## Platform Integration Notes

- macOS: isolate Accessibility, Screen Recording, CoreAudio, and VideoToolbox APIs in `platform-macos` or backend adapters.
- Linux: Wayland must use portals/PipeWire/libei where applicable; X11 is fallback only.
- Windows: account for UIPI and elevated-window limitations.
- Mobile: model backgrounding, permissions, and secure storage constraints before implementation.
