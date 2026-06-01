# Audio Routing & PipeWire (Follow-Mouse Audio)

Informs: `crates/streaming` and the `platform-*` backends. Introduces a proposed
`AudioBackend` trait (not yet in code) so audio capture/playback stays
platform-abstracted like input and clipboard.

**Feature**: "follow-mouse audio" — system audio follows the active device, so
when the user moves control to another machine, sound routes there too.

This is a **future-phase differentiator**; this document is research only and
should not pull audio code into the current phase. It exists to ensure the
`streaming` crate's interfaces don't preclude it.

---

## Pipeline (device A → device B)

```
capture (A) -> resample/encode (Opus) -> streaming transport -> decode -> playback (B)
                                          (reliable, ordered)
```

- **Codec**: Opus — low-latency, excellent quality/bitrate, designed for
  real-time. Crate: `opus` (libopus bindings) or `audiopus`.
- **Transport QoS**: audio is continuous but loss-sensitive for glitches; use a
  **reliable ordered stream** (or a jitter-buffered datagram flow). It is *not*
  event-bus traffic. Owned by `streaming`, not the lossy broadcast bus.
- **Clocking/jitter**: a playback-side jitter buffer absorbs network variance;
  resample to the sink's rate. Target end-to-end latency budget < ~50 ms on LAN.

---

## Per-platform capture/playback backends

The `AudioBackend` trait (proposed) abstracts these:

```text
trait AudioBackend {
    async fn start_capture(&self) -> Result<AudioStream, AudioError>;   // PCM frames out
    async fn start_playback(&self) -> Result<AudioSink, AudioError>;    // PCM frames in
    fn capabilities(&self) -> AudioCapabilities;
}
```

| OS | Backend | Capture approach |
|----|---------|------------------|
| Linux | **PipeWire** | create a capture node, or a **null sink** and capture its monitor to grab system output |
| macOS | CoreAudio | aggregate/loopback device (e.g. a virtual device) for system-audio capture |
| Windows | WASAPI | **loopback capture** (`AUDCLNT_STREAMFLAGS_LOOPBACK`) — native system-output capture |

### PipeWire specifics (Linux)
- PipeWire is the modern Linux audio/video graph server (supersedes PulseAudio +
  JACK; exposes PulseAudio/JACK compat layers).
- **Capture system output**: create a **null sink**, set it as the default sink,
  and capture its **monitor** port — clean way to intercept all app audio. Or
  link directly to existing nodes via the graph registry.
- **Playback**: create a stream node linked to the real output sink.
- Crate: **`pipewire`** (pipewire-rs). The PipeWire main loop is its own loop →
  run on a dedicated thread, bridge PCM frames via bounded `mpsc` (same pattern
  as input/clipboard).
- **Latency tuning**: PipeWire `quantum` / buffer size controls the period;
  smaller = lower latency, higher CPU/xrun risk. Negotiate per stream.
- **Permissions**: PipeWire access is generally available in a user session;
  under sandboxing (Flatpak) it goes through a portal.

---

## Open design items (defer to audio phase)

- Routing policy: "follow-mouse" needs a hook into the active-device state owned
  by `core` (which device currently has control) to decide the audio sink.
- Per-device volume / mute and format negotiation (rate, channels) handshake.
- Echo/duplication when both devices are physically near each other.
- Bidirectional (mic) is out of scope for v1 of the feature.

## Conclusion for current phase
No code changes now. The only constraint this places on the **current**
`streaming` crate design: keep its transfer abstraction generic over a
**continuous, ordered byte/frame stream** (not just discrete file blobs) so an
`AudioStream` can ride it later without redesign.
