# Security

nexkvm treats continuity features as sensitive: they can move input, clipboard content, files, audio, screen data, app launches, and collaborative control across devices. Security is mandatory, layered, and fail-closed.

## Threat Model

Primary threats:

- Unpaired devices attempting to join a mesh.
- Man-in-the-middle attacks during first pairing.
- Replay of old messages or control commands.
- Malicious or over-permissioned plugins.
- Accidental data exposure through clipboard, screen, workspace search, or collaboration.
- Platform permission confusion, especially on Wayland/macOS/Windows elevated windows.

## Device Trust

- Every device has a stable device identity used by core/UI routing.
- Crypto identity binds trust to a long-lived public key.
- Pairing pins peer identity into a trust store.
- Reconnects are accepted only for trusted devices, subject to policy.

## Pairing

The foundation supports QR/bootstrap and short-code style pairing models. Pairing must authenticate the first key exchange to prevent MITM. Future concrete cryptographic backends should use authenticated key agreement and signature verification behind the existing `crypto` interfaces.

The network layer can exchange pairing request/response messages over an
established transport and return a local confirmation prompt. That prompt is
not trust by itself: the user must compare/approve the short code before a later
runtime step writes the peer into the trust store. The file-backed trust path
verifies the approved code, pins the peer public key, and flushes the JSON trust
store before reporting success.

Trusted reconnects exchange device identities after a transport connection is
established and reject peers whose public key is not already pinned in the local
trust store. A later signing/challenge-response step must prove private-key
ownership before this becomes a complete cryptographic authentication story.

## Transport Security

Preferred transport order:

1. QUIC for direct LAN.
2. TCP fallback when QUIC/UDP is blocked.
3. WebRTC for future remote mode.

Transport TLS is not enough by itself. nexkvm also requires app-layer session security bound to paired device identity.

## Replay Protection

Replay prevention uses monotonic `MessageId` plus session nonces/windows in the crypto/session layer. Receivers must reject duplicates and stale IDs once concrete encrypted transport is wired.

## Permissions

Sensitive surfaces are explicit:

- Platform capabilities report what the current OS/session can do.
- Clipboard sync has conflict/history/encryption boundaries.
- Streaming requires encrypted media/file lanes.
- Workspace remote launch/search/memory features are policy-gated.
- Collaboration control uses explicit, scoped, revocable leases.
- Plugins declare and receive least-privilege `PluginCapabilities`.
- Presence/proximity signals only rank already-trusted devices for UX decisions; they never prove identity or grant permissions.
- Mesh routing and relay fallback must keep relays outside the trust boundary; paired device sessions stay end-to-end encrypted and replay-protected.
- Cloud sync is opt-in and must upload only end-to-end encrypted user data over HTTPS.
- Enterprise policy can deny remote sessions, cloud sync, mesh routing, clipboard timelines, team collaboration, and marketplace installs before execution reaches platform backends.

Unknown future event or protocol variants should be denied by default at sensitive dispatch boundaries.

## Plugin Sandboxing

Third-party plugins are expected to run in sandboxed WASM/WASI or Lua runtimes behind brokered host calls. Native in-process plugins are reserved for first-party trusted code. Marketplace installability checks require policy-compatible capabilities and matching runtime artifacts.

## Platform Notes

- macOS: Accessibility is required for input; Screen Recording for display capture.
- Linux Wayland: global input/display access must be portal-mediated; X11 is legacy fallback.
- Windows: UIPI can block injection into elevated windows; behavior must be surfaced as capability/policy state.
- Mobile: backgrounding, permissions, and secure storage constraints must be modeled before native implementation.

## Reporting Security Issues

Until a dedicated security policy file is added, do not disclose suspected vulnerabilities publicly. Open a minimal private report through the repository maintainers or use the project’s configured private advisory channel when available.
