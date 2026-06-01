# Architecture

coklu is organized as a Rust workspace with small crates that own specific parts of the continuity platform. The current implementation favors Sans-IO state machines and trait boundaries so behavior can be tested before native integrations are added.

## Crate Ownership

| Area | Crate | Owns |
| --- | --- | --- |
| Desktop entrypoint | `apps/desktop` | Daemon startup, config loading, telemetry initialization, developer CLI. |
| Mobile placeholder | `apps/mobile_future` | Future mobile companion surface. |
| Core domain | `crates/core` | Device identity, event bus, platform trait boundary, shared workspace, collaboration sessions, notifications, quick commands, automation planning, cloud sync policy, enterprise policy, team management. |
| Protocol | `crates/protocol` | Versioning, message envelope, stream frame codec. |
| Crypto/trust | `crates/crypto` | Device identity model, pairing, trust store trait, session security model. |
| Network | `crates/network` | Transport traits, QUIC/TCP/WebRTC planning, heartbeat, RTT, quality, zero-copy packets, decentralized mesh routing, relay policy, browser session planning. |
| Discovery | `crates/discovery` | LAN discovery, announcements, registry/reconnect state, proximity and presence scoring. |
| Input | `crates/input` | Input event model, topology, coalescing, batching, prediction, adaptive polling, cursor throw, infinite navigation, gesture switching, momentum transfer. |
| Clipboard | `crates/clipboard` | Clipboard item/history/timeline/sync model, conflict handling, compression/encryption boundaries. |
| Streaming | `crates/streaming` | File transfer, audio routing, screen streaming model, ordered media/bulk lanes. |
| Plugins | `crates/plugins` | Plugin manifest, permissions, runtime descriptors, sandbox policy, marketplace metadata, hot reload. |
| Storage | `crates/storage` | TOML config, trust persistence. |
| Telemetry | `crates/telemetry` | Tracing configuration. |
| Platform | `crates/platform/*` | OS-specific implementations and compatibility reports behind safe traits, including Linux handheld/Steam Deck capability hints. |

## Data Flow

1. Discovery finds peers on the LAN or through future internet discovery.
2. Crypto pairs and authenticates devices, then network establishes an encrypted session.
3. Protocol envelopes route opaque payloads by `MessageKind`.
4. Domain crates encode/decode their own payload bodies.
5. `core::EventBus` handles lossy in-process fan-out for real-time control and UI events.
6. `streaming` lanes handle reliable ordered file/media payloads that must not use the lossy event bus.

## Async Boundaries

- Public backend traits that may touch I/O are async: transport, platform permissions, capture/injection, plugin runtimes, screen/audio backends, workspace/collaboration backends.
- Pure policy engines remain synchronous and deterministic: topology, input batching, RTT/quality estimation, bandwidth adaptation, hot reload decisions, workspace snapping, collaboration leases.
- Blocking native calls must stay inside platform crates and be moved off the Tokio runtime with `spawn_blocking` or a dedicated native thread/queue.

## Platform Boundaries

Platform crates are the only place OS-specific FFI should land.

- macOS: Accessibility for input capture/injection; Screen Recording for display capture; CoreAudio/VideoToolbox for media.
- Linux Wayland: compositor portals for input/capture, PipeWire for screen/audio, X11 as legacy fallback.
- Windows: Raw Input/SendInput, UIPI-aware injection behavior, Graphics Capture/Desktop Duplication, WASAPI.
- Android/iOS: future companion APIs with mobile OS permission and backgrounding limits.
- Linux handhelds: Steam Deck/gamescope mode should prefer gamepad navigation, virtual keyboard fallback, and touch-friendly flows while respecting the same Wayland portal constraints.

## Performance Model

- Prefer `bytes::Bytes` for network/protocol payload fan-out.
- Keep input and cursor paths latency-first: coalesce, batch only briefly, and avoid queue buildup.
- Use dedicated stream lanes for file/audio/screen data instead of the event bus.
- Model zero-copy GPU paths for screen streaming, but keep CPU fallback available when policy allows.
- Keep cursor throw, gesture switching, and presence-aware switching as pure planners fed by sampled platform data so hot paths avoid blocking native calls.

## Security Model

Security is layered rather than delegated to one crate:

- Network transports provide QUIC/TLS or TCP+TLS/WebRTC channels.
- Crypto binds sessions to paired device identity and provides app-layer encryption/replay prevention.
- Plugins, workspace, collaboration, clipboard, and streaming expose explicit permission/policy surfaces.
- Presence/proximity signals are UX hints only; they never establish trust or replace secure pairing/authentication.
- Mesh relays and self-hosted relay servers are not trust roots; payloads remain end-to-end encrypted and replay-protected.
- Cloud sync is optional and must require HTTPS plus end-to-end encrypted payloads before uploading user data.
- Unknown future event/message variants should fail closed at sensitive boundaries.
