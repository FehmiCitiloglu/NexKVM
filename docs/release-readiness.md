# Release Readiness

This document defines the production gates for coklu release candidates. It is
intentionally stricter than the current foundation-phase implementation: a
release candidate must satisfy these gates before it can be called production
ready.

## Supported Release Scope

The first commercial desktop release targets:

- macOS desktop daemon and app bundle.
- Windows desktop daemon and installer.
- Linux desktop daemon packages for X11, with Wayland support reported through
  explicit capability state when compositor portals are incomplete.

The first release does not require mobile companion apps, WebRTC remote mode,
screen streaming, audio routing, cloud sync, or plugin marketplace support.

## Required Quality Gates

Every release candidate must pass:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps
cargo build --workspace --release --all-features
cargo deny check advisories bans licenses sources
```

Protocol fuzz smoke must also run before a tagged release:

```sh
cargo fuzz run protocol_decode -- -max_total_time=30
```

UDP discovery tests open local sockets. They should run on normal developer
machines and CI runners, but can fail inside restricted sandboxes with
`Operation not permitted`. Treat that as an environment failure only after a
non-sandboxed run passes the same test target.

## Security Gates

A release candidate must not ship with development-only security behavior on a
production path:

- Pairing pins the peer public key into the trust store only after user
  confirmation.
- Session traffic uses authenticated encryption with replay protection above
  transport TLS.
- Clipboard and file transfer lanes use session-backed ciphers; plaintext
  ciphers are limited to tests or explicit development features.
- Private keys are not stored in TOML config files.
- Logs and diagnostics do not expose private keys, session keys, plaintext
  clipboard contents, or file payload bytes.
- Untrusted peers, downgraded security policies, malformed frames, and replayed
  messages fail closed.

## Platform Gates

Each supported platform must have a manual smoke record for:

- First launch and permission prompts.
- Input capture and injection.
- Clipboard read/write and sync.
- Pairing, restart, and trusted reconnect.
- Denied permission behavior.
- Installer upgrade and uninstall.

Platform limitations must be reflected in `doctor` output and release notes.

## Release Artifact Gates

Tagged releases must publish:

- macOS signed and notarized archive.
- Windows signed installer.
- Linux `.deb`, `.rpm`, and AppImage artifacts.
- Checksums for every artifact.
- Changelog and known limitations.
- SBOM or dependency report.
- Smoke-test evidence for every supported OS.
