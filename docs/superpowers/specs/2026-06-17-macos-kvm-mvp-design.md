# macOS-first KVM MVP Design

## Goal

Build the first real KVM runtime slice for NexKVM by making macOS keyboard and
mouse sharing work across trusted desktop peers, while preparing a signed and
notarized macOS distribution path that avoids Gatekeeper's "unidentified
developer" warning for release builds.

This slice starts with macOS because the immediate replacement need is driven by
Barrier support and distribution friction on macOS. Linux and Windows remain
first-class targets, but their native input backends come after the runtime
shape is proven on macOS.

## Non-goals

- No full GUI pairing flow in this slice.
- No clipboard or file transfer runtime yet; those follow after input sharing.
- No claim that Linux or Windows native input sharing is working until their
  platform backends are actually implemented and smoke-tested.
- No attempt to bypass macOS security prompts. Accessibility and Input
  Monitoring remain explicit user-granted permissions.

## Current Repo Context

The repo already has:

- A desktop daemon and developer CLI in `apps/desktop`.
- TCP transport, inbound accept loop, and trusted rediscovery reconnect driver.
- Protocol envelope kinds for input, clipboard, and file transfer.
- Platform-neutral input models and macOS platform crate boundaries.
- Basic macOS bundle packaging with optional codesign and notarization hooks in
  `scripts/package-macos.sh`.
- `docs/features.md` as the source of truth for implemented versus planned
  feature status.

The missing runtime pieces are:

- A daemon session/router that forwards input envelopes over established peer
  connections.
- macOS native input capture and injection behind the existing platform
  boundary.
- Permission diagnostics that distinguish "not available", "permission
  required", and "ready".
- Release packaging verification for Developer ID signing, hardened runtime,
  entitlements, notarization, and Gatekeeper acceptance.

## Architecture

### macOS Platform Backend

`crates/platform/platform-macos` owns native macOS input integration.

It should expose safe Rust functions through the existing platform boundary for:

- Accessibility permission status and prompt.
- Input Monitoring permission status where observable.
- Keyboard and mouse capture using a CGEvent tap.
- Keyboard and mouse injection using CGEvent posting.
- Translation between NexKVM input events and macOS-native events.

The backend should keep `unsafe` and CoreGraphics calls contained inside the
macOS platform crate. Callers should see typed results and errors, not raw OS
handles.

### Runtime Session Router

`apps/desktop` should add a small peer session layer on top of existing TCP
connections.

For the MVP, the router only needs to handle `MessageKind::Input`:

- A source-side input capture task converts local macOS events into protocol
  envelopes and sends them to the active peer.
- A target-side receive task decodes input envelopes and applies them through
  the platform injection backend.
- Unknown or unsupported envelope kinds are logged and ignored until their
  runtime features land.

This keeps clipboard and file transfer future work on the same transport
surface without forcing those features into the first implementation.

### Control Role

The first slice uses explicit config or CLI state instead of a GUI:

- `source`: capture local input and send it to a trusted target.
- `target`: accept peer input and inject it locally.
- `both`: allow a machine to send and receive, but still require an active peer
  selection before capture starts.

The exact CLI/config surface should be minimal and testable. A later GUI can
replace it without changing the native backend or session router.

### Safety Switches

Input sharing needs escape paths:

- A local emergency hotkey stops capture.
- Modifier keys are released when capture stops or a peer disconnects.
- The daemon logs permission failures and peer disconnects clearly.
- Capture must not start unless macOS permissions are ready.

## macOS Permissions

The implementation must treat permissions as runtime state, not install-time
state.

`nexkvm doctor` should report:

- Accessibility status.
- Input Monitoring status, when detectable.
- Whether the daemon can capture input.
- Whether the daemon can inject input.
- The command or system settings path the user needs next.

Permission prompting should use Apple's supported APIs only. NexKVM should never
ask users to disable Gatekeeper or weaken system policy.

## Signing And Notarization

Release builds should be packaged as a `.app` bundle signed with a Developer ID
Application certificate, hardened runtime enabled, required entitlements
attached, submitted to Apple notarization, and stapled before distribution.

The package script should support:

- `APPLE_CODESIGN_IDENTITY` for Developer ID signing.
- `APPLE_NOTARY_PROFILE` for `xcrun notarytool`.
- A macOS entitlements file checked into `packaging/macos`.
- Post-build validation with `codesign`, `spctl`, and `stapler`.

Validation must fail the release packaging step when:

- The app is unsigned or ad-hoc signed.
- Hardened runtime is missing.
- Notarization is missing when a release archive is requested.
- `spctl` rejects the app bundle.

Local debug builds may remain unsigned or ad-hoc signed, but the documentation
and output must not imply that those builds avoid Gatekeeper distribution
warnings.

## Testing

Unit and integration tests should cover the parts that do not require live
macOS permissions:

- Input event translation between NexKVM and macOS event representations.
- Session/router routing for `MessageKind::Input`.
- Capture disabled when permission status is not ready.
- Injection task behavior for malformed or unsupported payloads.
- Packaging script dry-run or validation command construction where practical.

Manual smoke checks are still required for:

- First launch permission prompts.
- Accessibility permission grant and denial.
- Input Monitoring behavior.
- Real keyboard and mouse capture.
- Real injection into a second macOS session or test target.
- Signed and notarized archive accepted by Gatekeeper.

Smoke evidence should be tracked in docs before any release-readiness claim.

## Rollout Order

1. Add a daemon session/router for input envelopes over existing TCP
   connections.
2. Implement macOS permission diagnostics and wire them into `nexkvm doctor`.
3. Implement macOS event translation with unit tests.
4. Add macOS capture and injection backend tasks behind explicit config/CLI
   role selection.
5. Add emergency stop and modifier cleanup.
6. Harden macOS packaging with entitlements, Developer ID signing,
   notarization, stapling, and Gatekeeper validation.
7. Update `docs/features.md` only for the runtime pieces that are implemented
   and verified.

## Acceptance Criteria

- Two trusted macOS machines can run NexKVM with one configured as source and
  one as target.
- Keyboard and mouse events from the source reach the target over the existing
  transport and are injected by the target backend.
- Capture does not start when required macOS permissions are missing.
- `nexkvm doctor` tells the user exactly which macOS permission or signing
  prerequisite is missing.
- A release packaging command can produce a Developer ID signed, hardened,
  notarized, stapled app bundle when the developer machine has the required
  Apple credentials configured.
- Gatekeeper validation with `spctl` accepts the release app bundle.
- `docs/features.md` distinguishes implemented macOS MVP pieces from still
  planned Linux, Windows, clipboard, and file-transfer work.
