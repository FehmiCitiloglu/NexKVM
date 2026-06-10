# nexkvm Platform Research

Research notes for the **Platform Research** roadmap phase. Each document
de-risks a later implementation phase and maps findings onto the trait
boundaries already defined in the Foundation phase, so research translates
directly into implementation against existing interfaces.

> Status: research / design input. No production code depends on these notes;
> they inform the `platform-*`, `clipboard`, `streaming`, and `network` crates.

## Documents

| Topic | Document | Informs crate(s) | Owning trait(s) |
|-------|----------|------------------|-----------------|
| Input capture & injection (Windows raw input, macOS Accessibility, Linux evdev/uinput, Wayland) | [input-capture-injection.md](input-capture-injection.md) | `input`, `platform-*` | `InputCapture`, `InputInjector`, `PlatformBackend` |
| Clipboard interoperability | [clipboard-interop.md](clipboard-interop.md) | `clipboard`, `platform-*` | `Clipboard`, `ClipboardContent` |
| Audio routing / PipeWire (follow-mouse audio) | [audio-pipewire.md](audio-pipewire.md) | `streaming`, `platform-*` | (new `AudioBackend` trait) |
| QUIC performance tuning | [networking-quic.md](networking-quic.md) | `network` | `Transport`, `Connection` |

## Cross-cutting conclusions

1. **Kernel/native capture must run off the Tokio runtime.** Every OS capture
   primitive (low-level hooks, `CGEventTap`, evdev reads) needs a dedicated OS
   thread with its own loop. Bridge to async via a bounded
   `tokio::sync::mpsc` channel — never block the runtime, never hold the
   producer lock across `.await`.
2. **Permission/capability gating is non-uniform.** macOS (Accessibility),
   Wayland (portals), and Linux evdev (group/udev) all gate input differently.
   `PlatformCapabilities` + `request_permissions()` already model this; the
   research confirms the per-OS resolution logic each backend needs.
3. **Two QoS classes on the wire.** Real-time input → unreliable, low-latency
   (QUIC datagrams). Clipboard/file/audio bulk → reliable, ordered streams.
   This split is already reflected in the event bus (lossy broadcast) vs the
   `streaming` crate (reliable), and dictates QUIC configuration.
4. **Normalize at the boundary.** Coordinates → normalized `f64` (already in
   `InputEvent`); clipboard formats → MIME (already in `ClipboardContent`);
   keycodes → an OS-neutral keymap (open item, see input doc).
