# NexKVM

NexKVM is a Barrier-inspired LAN keyboard, mouse, clipboard, and file-sharing
application written in Rust. The current supported release-candidate scope is
**two Apple Silicon Macs**. Windows and Linux code remains in the workspace for
development and compatibility work, but those platforms are not claimed as
production-ready releases.

## Apple Silicon feature scope

The macOS application currently provides:

- authenticated pairing and pinned device trust;
- left, right, top, or bottom screen placement in the graphical control panel;
- keyboard and mouse handoff when the pointer crosses the configured edge;
- source, target, and bidirectional input roles, with Escape, inactivity, and
  disconnect release paths;
- multi-format clipboard synchronization plus bounded, encrypted history of
  local and received clipboard selections;
- authenticated file and directory transfer to a bounded receive directory;
- a GUI for topology, pairing, permissions, sharing settings, diagnostics,
  clipboard restore, and drag-and-drop file queueing.

These paths have automated unit and integration coverage. A distributable build
must additionally pass the signed-app, macOS privacy-permission, Gatekeeper, and
physical two-Mac checks in [Release Readiness](docs/release-readiness.md). An
ad-hoc local build is not evidence that notarization or real-device input works.

## Build and run on Apple Silicon

Prerequisites are an Apple Silicon Mac, rustup, and Xcode Command Line Tools.
The repository pins Rust 1.88.0 in `rust-toolchain.toml`. Build the daemon/CLI
and GUI for the supported target:

```sh
rustup target add aarch64-apple-darwin
cargo build --locked -p nexkvm -p nexkvm-gui \
  --release --target aarch64-apple-darwin
./target/aarch64-apple-darwin/release/nexkvm-gui
```

For a local, ad-hoc-signed app bundle:

```sh
NEXKVM_VERSION=0.1.0 ./scripts/package-macos.sh
```

The archive is written to
`target/package/nexkvm-macos-arm64-0.1.0.zip`. Ad-hoc signing is for local
development only; published archives must use Developer ID signing,
notarization, and stapling.

Follow [Two-Mac Apple Silicon Setup](docs/setup-macos-apple-silicon.md) to grant
permissions, pair both devices, choose the active peer, place the target on the
correct edge, and enable clipboard/file sharing.

## Development checks

Run the same core gates used for a release candidate:

```sh
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps
cargo build --workspace --all-features --release
cargo deny --target aarch64-apple-darwin \
  --exclude nexkvm-platform-linux \
  --exclude nexkvm-platform-windows \
  check advisories bans licenses sources
bash scripts/tests/test-package-macos.sh
```

Useful commands:

```sh
cargo run -p nexkvm -- doctor
cargo run -p nexkvm -- permissions
cargo run -p nexkvm -- devices
cargo run -p nexkvm -- pair-auto --peer 192.168.1.27:47654
cargo run -p nexkvm -- protocol
cargo run -p nexkvm -- simulate tools/sim/local-workspace.toml
```

Running `cargo run -p nexkvm` without a subcommand starts the daemon. The GUI
starts the sibling daemon from the app bundle and should be preferred for the
normal macOS workflow.

## Workspace layout

- `apps/desktop`: desktop daemon and CLI.
- `apps/gui`: graphical control panel and app-bundle entry point.
- `crates/core`: device identity, event bus, platform traits, and shared models.
- `crates/protocol`: bounded wire envelopes, framing, and versioning.
- `crates/crypto`: pairing, trust, and session security.
- `crates/network`: authenticated LAN transport and session framing.
- `crates/discovery`: UDP-broadcast discovery and trusted-peer reconnect planning.
- `crates/input`: HID events, topology, edge handoff, and focus safety.
- `crates/clipboard`: clipboard synchronization and history models.
- `crates/streaming`: bounded file-transfer protocol and other streaming models.
- `crates/storage`: protected configuration, identity, trust, and history data.
- `crates/platform/*`: isolated platform-native implementations.

## Documentation

- [Two-Mac Apple Silicon Setup](docs/setup-macos-apple-silicon.md)
- [Release Readiness](docs/release-readiness.md)
- [Apple Silicon Preview Notes](docs/alpha-release-notes.md)
- [Architecture](docs/architecture.md)
- [Protocol](docs/protocol.md)
- [Security Policy](SECURITY.md)
- [Security Design and Limitations](docs/security.md)
- [Feature Tracker](docs/features.md)
- [Tooling](docs/tooling.md)
- [Contributing](CONTRIBUTING.md)

Screen streaming, audio routing, mobile companions, remote/WAN relay, and the
plugin marketplace are outside the supported Apple Silicon KVM release scope.
## License

NexKVM is open-source software licensed under the
[Mozilla Public License 2.0](LICENSE).
