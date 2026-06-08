# Tooling

This repository keeps developer tooling lightweight and Cargo-native.

## Integration Tests

Cross-crate integration tests live under member crates. The current protocol/network/input/streaming pipeline test is:

```sh
cargo test -p coklu-network --test protocol_pipeline
```

## Protocol Fuzzing

Protocol fuzz targets use `cargo-fuzz` in `fuzz/`.

```sh
cargo install cargo-fuzz --locked
cargo fuzz run protocol_decode
```

The target feeds arbitrary bytes through stream framing and envelope decoding. It must never panic on malformed peer input.

## Benchmarks

The latency benchmark suite is a harness-free Cargo bench target:

```sh
cargo bench -p coklu-network --bench latency_suite
```

or:

```sh
sh scripts/bench.sh
```

## Developer CLI

The desktop binary includes dependency-light developer commands:

```sh
cargo run -p coklu -- doctor
cargo run -p coklu -- protocol
cargo run -p coklu -- config-path
cargo run -p coklu -- simulate tools/sim/local-workspace.toml
```

Running `cargo run -p coklu` with no subcommand starts the daemon.

## Local Simulation

`tools/sim/local-workspace.toml` describes a local sans-IO multi-device environment. Today the CLI validates and summarizes it; later phases can feed it into discovery, latency, workspace, screen, and collaboration simulators.

## CI And Release

- `.github/workflows/ci.yml` runs format, clippy, tests, release builds, and benchmark smoke checks across Linux, macOS, and Windows.
- The CI dependency-audit job runs `cargo deny check advisories bans licenses sources` using `deny.toml`.
- `.github/workflows/fuzz.yml` runs scheduled/manual protocol fuzz smoke tests.
- `.github/workflows/release.yml` builds release artifacts on version tags and publishes them to GitHub Releases.

UDP discovery tests open local sockets. In restricted sandboxes they can fail
with `Operation not permitted`; rerun the same target on a normal developer
machine or CI runner before treating the failure as a product regression.

## API Docs

Generate local Rust API documentation with:

```sh
cargo doc --workspace --all-features --no-deps
```

For a stricter pass that catches broken intra-doc links:

```sh
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps
```
