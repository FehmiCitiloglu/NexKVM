# NexKVM Apple Silicon Preview Notes

This preview concentrates on a Barrier-style experience between two trusted
Apple Silicon Macs on the same LAN. The code and automated tests are release-
candidate inputs; publication remains blocked until the signed artifact and
physical two-Mac evidence in [Release Readiness](release-readiness.md) pass.

## Included candidate functionality

- Mutual pairing with persisted public-key trust and authenticated sessions.
- Explicit trusted Active peer selection by device name or fingerprint.
- Source, Target, and Both keyboard/mouse roles.
- GUI screen placement on the left, right, top, or bottom, with live edge
  reconfiguration.
- Physical-input handoff model with local suppression during remote focus and
  Escape, timeout, disconnect, topology-change, and shutdown release paths.
- Multi-format clipboard synchronization with echo suppression, bounds, and
  concealed-item exclusion.
- Bounded encrypted clipboard history with GUI/CLI listing, restore, and clear.
- Authenticated, bounded file and directory transfer with integrity checks,
  durable queueing, checkpointing, and resume support.
- Apple Silicon app-bundle packaging with a GUI main executable and sibling
  daemon, plus fail-closed Developer ID signing/notarization mode.

## Scope and known limitations

- Only Apple Silicon (`arm64`) macOS is in the supported release-candidate
  scope. Intel/Universal macOS, Windows, and Linux are not production claims.
- The application is intended for a trusted, reachable LAN. WAN relay and cloud
  brokering are not included.
- Enabling file transfer permits the selected authenticated peer to send files;
  there is no per-transfer acceptance dialog.
- Clipboard history is encrypted on disk, but restoring an item places it on
  the system clipboard, where applications with clipboard access can read it.
- History archives are not bulk-replicated. Received current selections enter
  local history, and restoring an older entry makes it current for sync.
- Screen streaming, audio routing, mobile companions, remote relay, cloud sync,
  and the plugin marketplace are outside this preview.
- An ad-hoc package can trigger Gatekeeper warnings and is never a public
  release artifact.
- Automated tests do not prove physical event-tap behavior, TCC permission UX,
  a clean-machine Gatekeeper launch, or two-device interruption recovery.

## Publication rule

Do not describe the preview as production-ready or error-free unless the exact
signed/notarized artifact has completed every row in
[macOS KVM Smoke Checks](smoke/macos-kvm-mvp.md), including two physical Macs,
left/right edge handoff, clipboard/history, file transfer/resume, restart,
permission denial, and Gatekeeper validation.
