# Release Readiness

This document is the release decision contract for NexKVM. Passing the Rust
test suite means the code is a release *candidate*; it is not sufficient to
claim that a macOS input-sharing app is production-ready. A release also needs
a signed artifact and recorded tests on two physical Macs.

## Supported release scope

The current supported scope is:

- Apple Silicon (`arm64`) Macs running macOS 12 or newer;
- two trusted Macs on the same reachable LAN;
- mutual authenticated pairing and pinned peer identity;
- GUI-selected left, right, top, or bottom input handoff;
- keyboard and mouse capture/injection with explicit safety-release paths;
- bounded clipboard synchronization and encrypted clipboard history;
- bounded, authenticated file/directory transfer to a dedicated receive root.

This candidate speaks wire protocol `2.0`. Its signed ephemeral-key handshake
is intentionally incompatible with protocol `1.x`; upgrade both paired Macs
together before reconnecting them.

Intel-only or Universal macOS binaries, Windows, and Linux are not part of this
production-ready claim. The workspace and release workflow may compile or
package compatibility/experimental artifacts for those platforms, but artifact
existence is not a support statement. Screen streaming, audio routing, mobile
companions, WAN/relay mode, cloud sync, and the plugin marketplace are also out
of scope.

## Automated quality gates

Every candidate must pass from a clean checkout with the Rust 1.88.0 toolchain
pinned by `rust-toolchain.toml`:

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
```

Protocol fuzzing is a tagged-release gate, not an optional follow-up:

```sh
cargo +nightly-2026-07-15 fuzz run protocol_decode -- -max_total_time=30
```

Run the macOS packaging contract test on an Apple Silicon macOS runner:

```sh
bash scripts/tests/test-package-macos.sh
```

UDP discovery and TCP integration tests open local sockets. A restricted
sandbox can return `Operation not permitted`; classify that as an environment
limitation only after the same target passes outside the sandbox.

## Security and data-safety gates

A release candidate must fail closed on its supported path:

- Pairing pins the peer public key only after the user verifies and accepts the
  bootstrap fingerprint.
- Every runtime lane accepts only authenticated, trusted peers; an explicitly
  configured Active peer must never silently fall back to another peer.
- Session traffic uses authenticated encryption and replay protection.
- Clipboard and file payloads travel only inside the authenticated session.
- Pairing nonces and persisted encryption keys come from the operating system's
  cryptographically secure random source.
- Identity, trust, configuration, transfer queue, and clipboard-history files
  use bounded parsing, protected permissions, and safe non-symlink paths.
- Concealed pasteboard selections are neither synchronized nor retained.
- File manifests and chunks are bounded; traversal, symlinks, special files,
  source mutation, hash mismatch, and unsafe overwrite fail closed.
- Logs and diagnostics do not expose private/session keys, pairing secrets,
  plaintext clipboard contents, or file payload bytes.
- Disconnect, timeout, topology change, and shutdown release held keys/buttons
  and return focus without leaving local input suppressed.

The release review must include a targeted scan of production `unwrap`,
`expect`, `panic!`, `unsafe`, unbounded allocation, blocking async work, and
sensitive logging sites. Each remaining occurrence needs a documented reason or
a fix; test-only assertions are not production paths.

## What automation cannot prove

The following are manual gates and require two physical Apple Silicon Macs:

| Gate | Required evidence |
| --- | --- |
| TCC permissions | Accessibility and any presented Input Monitoring/Local Network prompts on both installed app copies; denial and post-grant restart behavior |
| Real input | Physical mouse edge crossing, pointer/click/drag/scroll, keyboard/modifiers, return to local, Escape, timeout, disconnect, and daemon shutdown |
| Topology | Right, Left, Top, and Bottom layouts, matching the four directions in the supported release scope |
| Clipboard | Bidirectional text and rich/image format copy, encrypted history restore, concealed-item exclusion, size-limit failure |
| File transfer | File and directory transfer, hash comparison, interrupted resume, limit rejection, and no unsafe overwrite/path escape |
| Persistence | Pairing, selected peer, topology, and trusted reconnect after both apps restart |
| Installed artifact | Gatekeeper launch on a clean second Mac using the exact downloaded archive, not `cargo run` or a developer-terminal binary |
| Lifecycle | Replace/upgrade the app bundle, preserve intended user state, then remove the app and document retained user-data behavior |

Synthetic UI automation is not acceptable evidence for the input rows because
macOS-generated accessibility events can bypass the hardware event-tap path.
Record results in [macOS KVM Smoke Checks](smoke/macos-kvm-mvp.md). Any required
row without a pass and reproducible evidence blocks release publication.

## Apple Silicon artifact gates

The supported release artifact is
`nexkvm-macos-arm64-<version>.zip`. It must contain exactly one top-level
`nexkvm.app` with:

- `nexkvm-gui` as `CFBundleExecutable`;
- the sibling `nexkvm` daemon/CLI;
- only `arm64` Mach-O executables;
- valid bundle version, icon, Local Network usage text, and Bonjour service;
- Developer ID Application signatures, hardened runtime, and trusted timestamp;
- an accepted Apple notarization result and stapled ticket.

Build the distributable artifact with explicit release mode:

```sh
: "${APPLE_CODESIGN_IDENTITY:?set Developer ID Application identity}"
: "${APPLE_NOTARY_PROFILE:?set validated notarytool keychain profile}"
NEXKVM_VERSION=0.1.0 NEXKVM_RELEASE=1 ./scripts/package-macos.sh
```

Then independently inspect the exact archive/app that will be published:

```sh
bash scripts/validate-macos-package.sh \
  target/package/nexkvm-macos-arm64-0.1.0.zip 0.1.0 arm64
codesign --verify --deep --strict --verbose=2 target/package/nexkvm.app
xcrun stapler validate target/package/nexkvm.app
spctl --assess --type execute --verbose=2 target/package/nexkvm.app
```

Running `./scripts/package-macos.sh` without `NEXKVM_RELEASE=1` creates an
ad-hoc-signed local-development bundle. That output may be useful for testing,
but it must never be published or counted as signing/notarization evidence.

A tagged release must also publish and verify:

- SHA-256 checksums for every published asset;
- an SPDX JSON SBOM or equivalent dependency report;
- release notes with supported scope and known limitations;
- the completed two-Mac smoke record tied to the exact version and SHA-256;
- CI logs for all automated gates.

## Signing credentials

The macOS release workflow expects these GitHub Actions secrets and fails
closed when any is missing:

- `APPLE_CERTIFICATE_BASE64` — base64-encoded Developer ID Application `.p12`;
- `APPLE_CERTIFICATE_PASSWORD` — password protecting that `.p12`;
- `APPLE_CODESIGN_IDENTITY` — the imported Developer ID Application identity;
- `APPLE_NOTARY_KEY_BASE64` — base64-encoded App Store Connect API `.p8` key;
- `APPLE_NOTARY_KEY_ID` and `APPLE_NOTARY_ISSUER_ID` — API key identifiers.

The workflow imports credentials into an ephemeral keychain, validates the
notary profile, and deletes the keychain after packaging. CI credential setup
does not replace the independent Gatekeeper test on a clean Mac.

## Release decision

Call the Apple Silicon build release-ready only when all automated, security,
artifact, and manual gates above pass for the same commit and artifact digest.
If physical two-device, TCC, Gatekeeper, or notarization evidence is unavailable,
label the output a development/preview candidate and state the missing gate.
