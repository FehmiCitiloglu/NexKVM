# NexKVM Public Alpha Notes

This alpha focuses on real-device keyboard and mouse sharing between trusted
desktop peers over a LAN connection.

## Included In The Alpha

- Mac-to-Windows real-device keyboard and mouse sharing when the smoke record
  passes, with macOS as the input source and Windows as the target.
- Pairing through `nexkvm pairing-uri` and `nexkvm pair --accept`.
- Explicit TCP peer connection through `network.connect_addr`.
- Edge-based pointer handoff.
- Source-side input suppression while remote focus is active on supported
  platforms.
- Emergency key, timeout, disconnect, and daemon shutdown release paths.
- GUI-assisted configuration, daemon start/stop, pairing, diagnostics, and
  notification output.

## Known Limitations

- This is a public alpha, not the full commercial release described in
  `docs/release-readiness.md`.
- Clipboard sync is disabled by default and is not part of the input alpha.
- Screen streaming, hover previews, audio routing, file transfer, mobile
  companion apps, WebRTC remote mode, relay mode, cloud sync, and plugin
  marketplace support are outside this alpha.
- The reverse Windows-to-Mac input direction is not covered by the initial
  alpha smoke record.
- Linux input is capability-limited unless a real Wayland portal smoke passes.
- Signed installers, SBOM, checksums, and every-OS smoke evidence remain
  production release gates.

## Publishing Rule

Publish only with the current `docs/smoke/real-device-input-alpha.md` evidence
record and keep every unsupported feature listed above in the known limitations.
