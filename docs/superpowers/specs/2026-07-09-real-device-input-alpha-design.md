# Real-Device Input Alpha Design

## Goal

Prepare NexKVM for a credible public alpha by proving and tightening the core
product promise: keyboard and mouse sharing between real trusted desktop
devices. This is the release spine because the repository already has most of
the daemon, pairing, secure TCP, platform input, GUI configuration, and edge
handoff foundations, while `docs/features.md` still marks the end-to-end
product feature as not production-ready.

The outcome is not a full commercial release. The outcome is an alpha that can
be published with clear limitations, real-device smoke evidence, and no known
blocker in the input-sharing path.

## Release Scope

The first publishable alpha targets desktop keyboard and mouse sharing over a
trusted LAN peer connection. macOS real-device input sharing is the required
smoke target because the existing macOS MVP work is the most complete and
includes permission prompts, capture, injection, source-side suppression, and
signing/notarization scaffolding. Windows smoke evidence is optional and does
not block the alpha; include it only if it passes the same path without
expanding scope. Linux input is reported as capability-limited unless the
already-started portal path passes a real Wayland session smoke.

The alpha path covers:

- Pairing two desktop devices and persisting trust.
- Running one device as `source` and one as `target`, plus `both` when both
  sides pass the same checks.
- Establishing the secure peer connection through explicit address or trusted
  rediscovery.
- Crossing the configured handoff edge to forward pointer, button, scroll, and
  MVP keyboard events.
- Suppressing local source-side input while remote focus is active.
- Releasing remote focus through the emergency key, timeout, disconnect, and
  daemon shutdown.
- Recording smoke evidence and known limitations before feature tracker updates.

## Non-Goals

- No screen streaming, hover previews, audio routing, mobile companion, cloud
  sync, plugin marketplace, WebRTC remote mode, relay mode, or file transfer.
- No broad UI redesign. The existing GUI may receive small status or wording
  fixes only when they unblock the input alpha.
- No claim that clipboard sync is release-ready. The current daemon has a
  clipboard handler path, but the alpha remains input-first unless clipboard
  work is explicitly scheduled later.
- No full commercial release claim. `docs/release-readiness.md` remains the
  stricter production gate for signed installers, SBOM, checksums, and every
  supported OS smoke record.

## Current Repo Context

The useful foundations already present are:

- `apps/desktop` starts the daemon, loads config, resolves platform
  capabilities, binds TCP, accepts inbound connections, dials explicit peers,
  and starts trusted rediscovery.
- `apps/desktop/src/input_session.rs` encodes input envelopes, routes edge
  handoff through the shared topology controller, forwards captured events,
  injects received envelopes, and handles emergency stop and timeout release.
- `crates/storage` has `[input]` config for role, active peer, handoff edge,
  emergency key, and remote focus timeout.
- `crates/platform/platform-macos` exposes macOS permission reporting, native
  capture, native injection, local suppression, clipboard, screen capture, and
  encoder surfaces.
- `crates/platform/platform-windows` exposes native input capture and injection
  support for the same shared input session layer.
- `docs/smoke/macos-kvm-mvp.md` covers permission and release signing smoke,
  but not the full two-device input-sharing record.

The main gap is evidence and hardening around the complete real-device flow.
Any implementation changes should be driven by failures observed in that path,
not by adding unrelated roadmap features.

## Architecture

This task uses the current runtime architecture rather than introducing a new
subsystem.

The source device owns native capture and the edge handoff state machine. Before
handoff, input remains local. At the configured edge, the source enters remote
focus, suppresses local input where supported, converts routed input events into
`MessageKind::Input` envelopes, and sends them over the established secure peer
connection.

The target device owns native injection. It receives envelopes from the same
connection, ignores non-input messages for this alpha path, decodes input
events, and injects them through the platform backend. The target must fail
closed on malformed input payloads, missing permissions, closed connections, or
unsupported platform events.

The connection path stays unchanged unless smoke testing exposes a blocker:
pairing writes trusted public keys, explicit address or trusted rediscovery
establishes TCP, and the app-layer secure session wraps the peer connection. If
rediscovery is flaky, the alpha may document explicit address as the reliable
first-run path while leaving rediscovery as best effort.

## Data Flow

1. The user pairs both devices with `nexkvm pairing-uri` and `nexkvm pair
   --accept`, or uses the existing GUI controls that call the same CLI paths.
2. The user configures `control_role`, `connect_addr` when needed,
   `handoff_edge`, `active_peer`, emergency key, and focus timeout.
3. `nexkvm doctor` and `nexkvm permissions` confirm capture and injection
   readiness before daemon start.
4. The source daemon captures local input and keeps events local until the
   configured edge is crossed.
5. The source forwards routed input envelopes over the secure connection while
   remote focus is active.
6. The target daemon decodes and injects the input events.
7. Emergency key, timeout, disconnect, or shutdown releases source-side
   suppression and returns focus to local control.

## Error Handling

Permission failures must be visible in `doctor`, daemon logs, and GUI command
output where the GUI is used. Capture must not start unless the platform reports
capture readiness; injection must not start unless injection readiness is true.

Connection failures must leave the source in local control. If a peer
disconnects during remote focus, the source releases suppression and logs the
reason. If an input payload is malformed or has the wrong message kind, the
target drops or rejects it without injecting anything.

Emergency stop is the most important safety behavior. Pressing the configured
key must stop remote forwarding without sending that key to the target, release
suppression, and make the daemon state obvious in logs or UI output.

## Testing And Evidence

Automated tests should cover any code changed while hardening the flow:

- Input session routing, envelope codec, emergency stop, timeout release, and
  local suppression callbacks.
- Config parsing and GUI/CLI command formatting when touched.
- Connection handler behavior if multiple peer handlers need to coexist.
- Platform translation helpers for any changed native event mappings.

Manual smoke records are required before calling the alpha input feature ready:

- First launch and permission prompt on the primary target platform.
- Successful pairing and trust persistence.
- Explicit peer address connection.
- Trusted rediscovery reconnect when available.
- Source-to-target keyboard, pointer, button, and scroll forwarding.
- Edge crossing and return-to-local behavior.
- Emergency key release.
- Focus timeout release.
- Daemon restart after pairing and after permission changes.
- Denied-permission behavior.

If a real two-device smoke cannot be completed in the current environment, the
implementation must not mark the product feature complete. In that case the
deliverable is a hardened smoke guide plus any automated fixes that are
verifiable locally.

## Documentation And Feature Tracker

The task adds or expands smoke documentation under `docs/smoke/`, then updates
`docs/features.md` only to the level proven by evidence:

- Move `End-to-end keyboard and mouse sharing between real devices` to
  implemented only after a recorded real-device smoke passes.
- Move `Real cursor edge crossing between machines` to implemented only after
  the edge-crossing smoke passes.
- Mark release-readiness smoke records as done only when the corresponding
  records exist.
- Keep clipboard, file transfer, screen streaming, audio, mobile, cloud, and
  plugin features planned.

Release notes call this a public alpha if the production gates in
`docs/release-readiness.md` are not fully met.

## Acceptance Criteria

- A documented two-device smoke path exists for the primary alpha platform.
- The source can hand off control at the configured edge and the target receives
  real keyboard and mouse input.
- Local source input is suppressed during remote focus on platforms that support
  suppression.
- Emergency key, timeout, disconnect, and daemon shutdown release remote focus.
- Missing or denied permissions prevent capture or injection and explain the
  next user action.
- Pairing and trust persistence survive daemon restart.
- Automated tests pass for every code path changed by hardening work.
- `docs/features.md` and smoke records reflect only what was actually proven.
