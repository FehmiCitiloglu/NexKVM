# Feature List

This file is the project feature tracker for NexKVM. Keep it updated as work
lands:

- `[ ]` means planned or not production-ready.
- `[x]` means implemented in the repository at the stated level.
- Move an item from "Planned Features" to "Implemented Features" when it
  becomes a working repository feature, not just a design note.
- If a feature is implemented only as a model, trait boundary, or planner, keep
  that scope explicit.

## Current Project Phase

NexKVM is in a foundation phase. Many features are implemented as portable Rust
models, protocol/security contracts, trait boundaries, CLI surfaces, and
Sans-IO state machines. Several user-visible native integrations are still
planned.

## Implemented Features

### Workspace And Project Structure

- [x] Cross-platform Rust workspace with desktop, future mobile, core, protocol,
  crypto, network, discovery, input, clipboard, streaming, plugins, storage,
  telemetry, and platform crates.
- [x] Desktop daemon crate and developer CLI entrypoint.
- [x] Cross-platform native GUI control panel crate for configuration, daemon
  control, diagnostics, permissions, pairing, and input handoff settings.
- [x] GUI-managed daemon start preflight that detects a busy listen port, stops
  a stale local `nexkvm` daemon process, and refuses startup if the port remains
  occupied.
- [x] GUI notification center for daemon lifecycle, configuration, command,
  permission, and pairing status events.
- [x] Future mobile companion placeholder crate for Android and iOS targets.
- [x] Platform-specific crate boundaries for macOS, Linux, and Windows.

### Developer CLI And Daemon Foundation

- [x] `nexkvm doctor` diagnostics command.
- [x] `nexkvm protocol` protocol compatibility command.
- [x] `nexkvm config-path` command.
- [x] `nexkvm devices` trusted-device listing command.
- [x] `nexkvm pair <uri>` pairing bootstrap decode and fingerprint display.
- [x] `nexkvm pair --accept <uri>` trust-store write path after user-confirmed
  pairing bootstrap.
- [x] `nexkvm pairing-uri <addr>` command for generating a testable pairing
  bootstrap URI.
- [x] `nexkvm permissions` command for prompting/reporting required macOS
  input permissions.
- [x] `nexkvm simulate [toml]` basic simulation-file summary.
- [x] `--debug` flag for raising daemon log verbosity.
- [x] Daemon startup wiring for config loading, telemetry, device identity,
  event bus, platform backend resolution, and graceful shutdown.
- [x] UDP LAN discovery startup in daemon mode.
- [x] Trusted-peer rediscovery logging through discovery service.
- [x] Trusted-peer rediscovery to cross-platform TCP connection driver.
- [x] Explicit peer address launch profile for GUI-managed startup when LAN
  discovery is not enough or a specific target should be dialed.
- [x] Input sharing runtime configuration for disabled, source, target, and
  both roles.
- [x] Barrier-inspired linked-screen input controller in the daemon forwarding
  path, reusing the pure topology/boundary state machine for edge handoff.
- [x] Source-side local input suppression while remote focus is active on macOS
  and Windows, with emergency key and timeout release still wired through the
  shared router.

### Protocol

- [x] Protocol version model and version negotiation.
- [x] Network message envelope with monotonic message IDs.
- [x] Stable message kind routing discriminants.
- [x] Length-prefixed stream frame codec.
- [x] Maximum frame-size guard.
- [x] Zero-copy packet decode model.
- [x] Protocol fuzz target for malformed input.
- [x] Cross-crate protocol pipeline integration test.

### Security, Pairing, And Trust

- [x] Device identity and public-key trust model.
- [x] QR/bootstrap pairing URI model.
- [x] Short confirmation/fingerprint display flow.
- [x] In-memory trust store.
- [x] File-backed trust store.
- [x] User-confirmed pairing bootstrap acceptance into the persisted trust
  store.
- [x] File-backed local private device identity seed fallback.
- [x] Session security trait boundary.
- [x] ChaCha20-Poly1305 AEAD session security implementation.
- [x] Monotonic message ID and nonce-based replay-protection model.
- [x] Trusted-peer key announcement handshake for pinned peers.
- [x] Ed25519 private-key proof-of-possession for trusted session handshake.
- [x] Pairing-flow integration test.

### Discovery

- [x] LAN discovery trait.
- [x] UDP broadcast discovery backend.
- [x] Service announcement model.
- [x] TTL-based discovery registry.
- [x] Fingerprint allowlist for advisory trust matching.
- [x] Trusted reconnect planner.
- [x] Proximity and presence scoring model.
- [x] Internet discovery record/candidate model.
- [x] UDP discovery service integration test.
- [x] mDNS feature gate and abstraction.

### Network

- [x] Transport and connection traits.
- [x] Transport selector.
- [x] TCP transport backend.
- [x] Desktop daemon TCP listener and inbound connection accept loop.
- [x] Testable input envelope codec and connection routing helpers over peer
  connections.
- [x] QUIC feature-gated backend surface.
- [x] WebRTC feature-gated remote-mode planning surface.
- [x] Wire codec between protocol envelopes and bytes.
- [x] App-layer secure connection wrapper that seals/opens envelope bodies over
  any transport connection.
- [x] Replay/authentication rejection covered in the concrete secure receive
  path.
- [x] Daemon inbound/outbound TCP peer connections are wrapped in app-layer
  session security after trusted-peer signed key handshake.
- [x] Resumable in-process session model.
- [x] Heartbeat and liveness monitor.
- [x] RTT and jitter tracker.
- [x] Network quality estimator.
- [x] Exponential reconnect backoff.
- [x] Adaptive outbound buffering.
- [x] Bandwidth adaptation model.
- [x] Remote-session offer/answer security gate.
- [x] Mesh routing policy model.
- [x] Relay admission and route policy model.
- [x] Browser remote-session ticket planning.
- [x] Latency benchmark suite.

### Input

- [x] Platform-neutral input event model.
- [x] Input capture and injection trait boundaries.
- [x] Mouse sharing controller.
- [x] Keyboard sharing controller.
- [x] Modifier release handling during focus switch.
- [x] Device UX profile store.
- [x] Hotkey and quick-switch model.
- [x] Monitor layout and spatial topology model.
- [x] Cursor boundary detection.
- [x] Cursor handoff and return model.
- [x] Input batching and coalescing.
- [x] Predictive cursor model.
- [x] Adaptive polling model.
- [x] Cursor acceleration model.
- [x] Cursor interpolation and transition model.
- [x] Infinite desktop navigation.
- [x] Gesture switching and momentum transfer models.
- [x] Mobile touchpad translation model.
- [x] Mobile gyro mouse model.
- [x] Platform injection translation helpers.

### Clipboard

- [x] Multi-format clipboard content and snapshot model.
- [x] Clipboard platform access trait.
- [x] Clipboard compression and decompression.
- [x] Clipboard encryption boundary.
- [x] Session-backed clipboard cipher adapter.
- [x] Conflict resolver with last-writer-wins and echo suppression.
- [x] Bounded deduplicated clipboard history.
- [x] Shared clipboard timeline and restore planning.
- [x] Clipboard sync state machine.
- [x] Clipboard engine model.

### Streaming, File Transfer, Audio, And Screen Models

- [x] Reliable ordered streaming error/model boundaries.
- [x] File transfer manifest and entry model.
- [x] Transfer queue and progress snapshots.
- [x] Resume checkpoint model.
- [x] Chunked transfer sender and receiver.
- [x] Transfer reassembly.
- [x] Transfer compression.
- [x] Transfer encryption boundary.
- [x] Hover preview controller.
- [x] Audio device, format, frame, and route models.
- [x] Follow-mouse and shared-headset audio routing model.
- [x] Audio jitter buffer.
- [x] Screen capture and encoder trait boundaries.
- [x] Screen stream capability negotiation.
- [x] Hardware encoder capability model.
- [x] Screen preview and instant app preview planning models.

### Core Workspace, Collaboration, Automation, And Management

- [x] Stable device identity model.
- [x] In-process async event bus.
- [x] Platform capability descriptor.
- [x] Shared workspace model.
- [x] Unified virtual desktop model.
- [x] Window snapping planner.
- [x] Spatial navigation model.
- [x] Flick/throw planner.
- [x] Workspace search and shared memory models.
- [x] Collaboration session model.
- [x] Participant roles and permissions.
- [x] Shared cursor update model.
- [x] Scoped control lease model.
- [x] Automation trigger/action/rule model.
- [x] Quick command and command palette model.
- [x] Cross-device notification model.
- [x] Script engine trait boundary.
- [x] Cloud sync configuration model.
- [x] Enterprise policy model.
- [x] Team collaboration space model.

### Plugins

- [x] Plugin lifecycle trait.
- [x] Plugin context and event hook model.
- [x] Plugin manifest model.
- [x] Least-privilege plugin capability model.
- [x] Plugin registry with capability-filtered event dispatch.
- [x] Plugin runtime trait and descriptor model.
- [x] Sandbox level and resource-limit model.
- [x] Host broker and host-call permission model.
- [x] Marketplace listing and installability policy model.
- [x] Hot reload tracker.
- [x] WASM and Lua runtime feature gates.

### Storage, Telemetry, Tooling, And Packaging

- [x] TOML user configuration schema.
- [x] Platform-aware config path resolution in desktop app.
- [x] JSON trust-store persistence.
- [x] Tracing-based telemetry initialization.
- [x] Optional JSON telemetry output feature.
- [x] Cargo-native test, clippy, doc, fuzz, bench, and deny tooling docs.
- [x] CI workflow definitions.
- [x] Scheduled/manual fuzz workflow definition.
- [x] Release workflow definition.
- [x] Linux desktop file and package metadata.
- [x] macOS bundle metadata.
- [x] macOS release packaging validation scaffolding for Developer ID signing,
  hardened runtime, notarization, stapling, and Gatekeeper checks.
- [x] Windows NSIS installer script.
- [x] Package helper scripts.

### Platform Capability Foundations

- [x] macOS platform backend skeleton.
- [x] macOS Accessibility permission prompt and capability refresh.
- [x] macOS keyboard/mouse permission diagnostics in `doctor`.
- [x] macOS first-run permission guidance with System Settings path and restart
  instruction.
- [x] macOS input capture and injection permission-gated runtime boundaries.
- [x] macOS CGEventTap input capture loop for pointer, buttons, scroll, and
  MVP keyboard keys.
- [x] Edge-based extended-screen input handoff that keeps input local until the
  configured edge is crossed.
- [x] macOS source-side input suppression while remote focus is active.
- [x] macOS native input injection posting for absolute pointer, buttons,
  scroll, and MVP keyboard keys.
- [x] Windows platform backend skeleton.
- [x] Linux platform backend with session, desktop, portal, PipeWire, X11, and
  handheld capability analysis.
- [x] Runtime native integration reporting for available, permission-required,
  and unsupported platform capabilities.
- [x] macOS injection translation helper model.
- [x] Linux injection translation helper model.
- [x] Windows injection translation helper model.
- [x] Windows low-level hook input capture loop for pointer, buttons, scroll,
  and MVP keyboard keys.
- [x] Windows native input injection via `SendInput` for pointer, buttons,
  scroll, and MVP keyboard keys.

## Planned Features

### Native Platform Integrations

- [x] macOS clipboard backend (MVP text read/write via pbpaste/pbcopy; daemon runtime integration complete; encode/decode transport layer complete; remote updates applied via Clipboard::write(); multi-format NSPasteboard FFI support for text, HTML, RTF, and images implemented).
- [x] Windows clipboard backend (MVP text read/write via native Clipboard API; UTF-8 encoding; daemon runtime integration complete; supports CF_UNICODETEXT, CF_DIB, CF_HDROP format mapping).
- [x] Linux clipboard backend (MVP text read/write via arboard; unified X11/Wayland support; daemon runtime integration complete).
- [x] macOS screen capture using CoreGraphics CGDisplayCreateImage and window capture paths (MVP synchronous frame capture for display/window/application sources; display enumeration via NSScreen FFI; Window/Application source enumeration via CGWindowListCopyWindowInfo; ScreenCaptureBackend trait impl with ScreenCaptureKit availability gating, screen-recording permission request integration, source listing, and monotonic frame sequence numbering; BGRA8 pixel format with System memory backend; spawn_blocking async integration; 2 unit tests passing).
- [x] macOS media encoding through VideoToolbox (`MacosVideoToolboxEncoder`
  wraps BGRA/RGBA system-memory frames in CoreVideo pixel buffers, encodes
  H.264/H.265 through `VTCompressionSession`, and returns stream-ready encoded
  payloads).
- [~] Linux Wayland portal-mediated input capture and injection (daemon-facing
  portal session boundary, grant gating, capture/injection traits, and concrete
  zbus xdg-desktop-portal RemoteDesktop/InputCapture transport are implemented;
  InputCapture pointer-barrier lifecycle is modeled through `GetZones`,
  `SetPointerBarriers`, and `Enable`; `ConnectToEIS` fd is retained for an EIS
  decoder backend; Request response parsing is wired for zone sets and rejected
  barriers; `ReisPortalEisEventDecoder` opens the portal fd as a receiver
  context and maps pointer, scroll, button, and keyboard EIS events to NexKVM
  input events; `nexkvm portal-smoke` exercises the grant, first-zone right-edge
  barrier, and EIS event path on real Linux Wayland sessions).
- [ ] Linux PipeWire screen capture.
- [ ] Linux PipeWire audio routing backend.
- [ ] Linux X11 input and clipboard fallback implementation.
- [ ] Windows screen capture via Graphics Capture or Desktop Duplication.
- [ ] Windows audio routing via WASAPI.

### Networking And Sessions

- [ ] Private-key-backed device identity in OS keychain.
- [ ] Fully authenticated reconnect path for trusted devices.
- [ ] TCP transport hardened with TLS.
- [ ] QUIC transport fully wired as preferred LAN path.
- [ ] WebRTC NAT traversal for remote mode.
- [ ] STUN/TURN configuration and remote signaling flow.
- [ ] Self-hosted relay server integration.
- [ ] Managed relay integration.
- [ ] Browser remote-session runtime flow.

### Product Features

- [ ] End-to-end keyboard and mouse sharing between real devices.
- [ ] Real cursor edge crossing between machines.
- [ ] Real shared clipboard read/write/sync between machines.
- [ ] Drag-and-drop file transfer between machines.
- [ ] Background file/folder transfer UI or daemon flow.
- [ ] Follow-mouse audio routing.
- [ ] Shared headset mode.
- [ ] Screen streaming.
- [ ] Window hover preview backed by live capture.
- [ ] Instant app preview backed by live capture.
- [ ] Unified virtual desktop UI.
- [ ] Cross-device window snapping.
- [ ] Global workspace search.
- [ ] Cross-device app launch.
- [ ] Shared workspace memory.
- [ ] Shared cursor collaboration.
- [ ] Pair programming collaborative control flow.
- [ ] Remote teaching/control leases with revocation UI.
- [~] Cross-device notifications surfaced in UI (GUI notification center exists
  for local runtime events; trusted-peer notification ingestion remains
  planned).
- [ ] Quick command palette UI.
- [ ] Automation scripting runtime.

### Mobile Companion

- [ ] Android mobile companion app.
- [ ] iOS mobile companion app.
- [ ] Mobile pairing flow.
- [ ] Mobile touchpad-to-desktop runtime path.
- [ ] Mobile gyro-to-desktop runtime path.
- [ ] Mobile clipboard integration.
- [ ] Mobile backgrounding and permission model.
- [ ] Secure mobile key storage.

### Plugins And Marketplace

- [ ] Real WASM/WASI plugin runtime.
- [ ] Real Lua plugin runtime.
- [ ] Stable host-call ABI for sandboxed plugins.
- [ ] Signed plugin artifact verification.
- [ ] Persistent plugin install state.
- [ ] Plugin capability review UI.
- [ ] Marketplace trust UI.
- [ ] Plugin install, update, disable, and uninstall flows.

### Cloud, Enterprise, And Teams

- [ ] Opt-in cloud sync runtime.
- [ ] End-to-end encrypted cloud payload upload.
- [ ] Cloud sync provider integration.
- [ ] Enterprise policy enforcement across runtime paths.
- [ ] Team collaboration runtime flow.
- [ ] Team membership management UI or CLI.
- [ ] Managed-device deployment policy.

### Simulation And Developer Experience

- [x] Typed TOML parsing for `nexkvm simulate`.
- [x] Simulation validation for empty, duplicated, malformed, or unknown devices.
- [x] Simulation output with device ID, display name, OS, address, and trust
  state.
- [x] Simulation connection planning for direct LAN, reconnect candidate,
  missing trust, and invalid configuration.
- [x] Stable integration test for simulation report output.
- [x] Feed simulation data into discovery, latency, workspace, screen, and
  collaboration simulators.

### Release Readiness

- [ ] First-launch platform smoke records.
- [ ] Permission prompt smoke records.
- [ ] Input capture and injection smoke records.
- [ ] Clipboard sync smoke records.
- [ ] Pairing, restart, and trusted reconnect smoke records.
- [ ] Denied-permission behavior smoke records.
- [ ] Installer upgrade and uninstall smoke records.
- [ ] macOS signed and notarized archive.
- [ ] Windows signed installer.
- [ ] Linux `.deb` artifact.
- [ ] Linux `.rpm` artifact.
- [ ] Linux AppImage artifact.
- [ ] Checksums for release artifacts.
- [ ] Changelog and known limitations.
- [ ] SBOM or dependency report.
- [ ] Smoke-test evidence for every supported OS.
