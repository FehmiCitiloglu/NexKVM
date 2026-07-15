# macOS Apple Silicon KVM Smoke Checks

This record is the manual release gate for NexKVM's supported two-Mac scope.
Fill it with evidence from the exact signed archive proposed for publication.
Commands run from `cargo` or synthetic UI input are useful diagnostics, but do
not replace installed-app and physical-hardware results.

## Candidate identity

Record before testing:

| Field | Value |
| --- | --- |
| Version/tag | _not recorded_ |
| Commit | _not recorded_ |
| Archive filename | _not recorded_ |
| Archive SHA-256 | _not recorded_ |
| Mac A model / macOS | _not recorded_ |
| Mac B model / macOS | _not recorded_ |
| Tester / date | _not recorded_ |

Do not publish while any required field or test row remains unrecorded.

## Automated preflight

Attach CI links or local logs for:

```sh
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps
cargo build --workspace --all-features --release
cargo deny check advisories bans licenses sources
cargo fuzz run protocol_decode -- -max_total_time=30
bash scripts/tests/test-package-macos.sh
```

Record: _not recorded_.

## Permission and denial smoke

Install the exact candidate app on both Macs. Before Accessibility approval,
run **Permissions** and **Doctor** from the GUI. Expected:

- macOS presents or links to the relevant privacy setting;
- `macOS input accessibility: permission-required`;
- capture/injection are not reported ready for a role that needs them;
- the daemon does not silently continue with input sharing.

Grant Accessibility to the exact installed NexKVM app. Approve Local Network
and Input Monitoring if macOS presents them. Quit the entire app and reopen it,
then run **Doctor** again. Expected for a Source/Target pair:

- Mac A Source reports capture ready;
- Mac B Target reports injection ready;
- both report the intended active trusted peer and connection settings.

| Check | Mac A | Mac B | Evidence |
| --- | --- | --- | --- |
| Denied Accessibility fails closed | not run | not run | _not recorded_ |
| Grant/restart path succeeds | not run | not run | _not recorded_ |
| Local Network prompt/connection works | not run | not run | _not recorded_ |
| Rebuilt/replaced app has understandable permission behavior | not run | not run | _not recorded_ |

## Pairing and connection smoke

Follow [Two-Mac Apple Silicon Setup](../setup-macos-apple-silicon.md). Exchange
pairing URIs over a trusted channel, compare fingerprints independently, and
accept each Mac on the other. Do not place full pairing URIs or sensitive paths
in this public record.

| Check | Status | Evidence |
| --- | --- | --- |
| Mutual fingerprint verification and acceptance | not run | _not recorded_ |
| `devices` lists only the intended peer on both Macs | not run | _not recorded_ |
| Explicit Active peer accepts that peer and rejects a different trusted peer | not run | _not recorded_ |
| Restart preserves identity/trust and reconnects | not run | _not recorded_ |
| Firewall/unreachable address produces actionable failure | not run | _not recorded_ |

## Physical input and topology smoke

Use a physical mouse and keyboard on the Source. Synthetic accessibility events
may bypass the hardware event tap and are not release evidence.

| Check | Status | Evidence |
| --- | --- | --- |
| Target on Right: source right-edge crossing hands off focus | not run | _not recorded_ |
| Target on Left: source left-edge crossing hands off focus | not run | _not recorded_ |
| Target on Top: source top-edge crossing hands off focus | not run | _not recorded_ |
| Target on Bottom: source bottom-edge crossing hands off focus | not run | _not recorded_ |
| Pointer motion, left/right click, drag, and scroll reach target | not run | _not recorded_ |
| Letters, arrows, function keys, and left/right modifiers reach target | not run | _not recorded_ |
| Source input is suppressed only while remote focus is active | not run | _not recorded_ |
| Movement back across linked boundary returns local focus | not run | _not recorded_ |
| Escape returns local focus without forwarding Escape | not run | _not recorded_ |
| Inactivity timeout returns local focus and releases held state | not run | _not recorded_ |
| Disconnect and daemon shutdown release held keys/buttons | not run | _not recorded_ |
| Live edge change releases old focus and uses the new edge | not run | _not recorded_ |
| Retina/scaled and multi-display coordinates behave correctly | not run | _not recorded_ |

## Clipboard and history smoke

Enable clipboard sync/history on both Macs and restart both daemons.

| Check | Status | Evidence |
| --- | --- | --- |
| Plain text copies A→B and B→A without echo loops | not run | _not recorded_ |
| HTML/RTF and an image paste with usable format/content | not run | _not recorded_ |
| Bounded encrypted history survives restart | not run | _not recorded_ |
| GUI refresh/restore/clear and CLI equivalents work | not run | _not recorded_ |
| Restored item becomes current clipboard content | not run | _not recorded_ |
| Concealed pasteboard item is not sent or retained | not run | _not recorded_ |
| Oversized/malformed/tampered content fails closed | not run | _not recorded_ |

Do not paste secrets merely to test concealed behavior; use synthetic test data
from an application that marks its pasteboard item concealed.

## File-transfer smoke

Enable file transfer on both Macs, select an explicit Active peer, and restart.
Use non-sensitive fixtures and compare SHA-256 hashes at source/destination.

| Check | Status | Evidence |
| --- | --- | --- |
| Single regular file arrives with matching name, size, and hash | not run | _not recorded_ |
| Nested directory and empty file/directory arrive correctly | not run | _not recorded_ |
| Destination is isolated under `NexKVM/<transfer-id>/` | not run | _not recorded_ |
| Disconnect mid-file resumes and completes with matching hash | not run | _not recorded_ |
| Existing/unsafe destination is not overwritten | not run | _not recorded_ |
| Symlink, special file, traversal path, and limit violations are rejected | not run | _not recorded_ |
| Different trusted but unselected peer cannot send | not run | _not recorded_ |

## Release signing and Gatekeeper smoke

Build only after the signing certificate and a validated `notarytool` keychain
profile are available:

```sh
: "${APPLE_CODESIGN_IDENTITY:?set Developer ID Application identity}"
: "${APPLE_NOTARY_PROFILE:?set notarytool keychain profile}"
version="$(cargo pkgid -p nexkvm)"
NEXKVM_VERSION="${version##*@}" NEXKVM_RELEASE=1 ./scripts/package-macos.sh
```

Inspect the generated app and archive:

```sh
codesign --verify --deep --strict --verbose=2 target/package/nexkvm.app
codesign -dvvv --entitlements :- target/package/nexkvm.app
xcrun stapler validate target/package/nexkvm.app
spctl --assess --type execute --verbose=2 target/package/nexkvm.app
bash scripts/validate-macos-package.sh \
  "target/package/nexkvm-macos-arm64-${version##*@}.zip" "${version##*@}" arm64
```

Expected:

- both embedded executables are `arm64` and the app signature verifies;
- `codesign` shows Developer ID signing, hardened runtime, and timestamp;
- `stapler validate` succeeds and `spctl` reports the app accepted;
- the archive contains `nexkvm-gui` as the bundle executable and sibling
  `nexkvm` daemon;
- a fresh second Mac launches the downloaded archive without bypassing
  Gatekeeper and can complete the permission/pairing smoke above.

| Check | Status | Evidence |
| --- | --- | --- |
| Developer ID signature/hardened runtime/timestamp | not run | _not recorded_ |
| Apple notarization accepted and ticket stapled | not run | _not recorded_ |
| Package validator passes exact archive | not run | _not recorded_ |
| Clean-Mac Gatekeeper launch succeeds | not run | _not recorded_ |
| Published SHA-256 matches tested archive | not run | _not recorded_ |

## Result

Overall status: **NOT VERIFIED** until every required row above is a recorded
pass for the same archive digest. A failing or missing physical, TCC,
notarization, or Gatekeeper row blocks production publication even when all
automated tests pass.
