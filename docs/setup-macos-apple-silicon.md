# Two-Mac Apple Silicon Setup

This guide configures one Apple Silicon Mac to control another over a trusted
LAN. It covers the supported keyboard/mouse, clipboard history, and file
transfer scope. It does not turn an ad-hoc development build into a published
release; see [Release Readiness](release-readiness.md) for that distinction.

## 1. Prepare both Macs

Use two Apple Silicon Macs running macOS 12 or newer. Both devices must be on a
trusted LAN and able to reach TCP port `47654` on the receiving Mac. Automatic
discovery uses IPv4 UDP broadcast on port `47655`; the bundle also declares the
reserved `_nexkvm._udp` Bonjour service type for the mDNS backend. VPN
isolation, guest Wi-Fi client isolation, or a firewall can prevent discovery or
connection.

Install the same NexKVM build on both devices. For repository development, run:

```sh
rustup target add aarch64-apple-darwin
NEXKVM_VERSION=0.1.0 ./scripts/package-macos.sh
```

Unpack `target/package/nexkvm-macos-arm64-0.1.0.zip` and move `nexkvm.app` to
`/Applications` on each Mac. This default command creates an ad-hoc-signed local
build; do not redistribute it. A public build must be Developer ID signed,
notarized, stapled, and accepted by Gatekeeper.

Give each Mac a unique device name in **Settings > Device**, then save. Avoid
using the same display name for both peers because the active-peer selector can
resolve by name.

## 2. Grant macOS permissions

On each Mac, open NexKVM and choose **Permissions** from the Overview page. In
**System Settings > Privacy & Security > Accessibility**, allow the exact
NexKVM build you are running. Also approve Local Network and Input Monitoring if
macOS presents those prompts. Quit the whole app and reopen it after changing a
privacy permission; an already-running process may retain the old state.

Run **Doctor** in the GUI, or use the bundled CLI:

```sh
/Applications/nexkvm.app/Contents/MacOS/nexkvm permissions
/Applications/nexkvm.app/Contents/MacOS/nexkvm doctor
```

Do not continue with input sharing until `doctor` reports macOS input
accessibility, capture, and injection as ready for the configured role. Privacy
approval is tied to the app's code identity, so replacing or rebuilding an
ad-hoc app can require approval again.

## 3. Pair in both directions

Pairing is mutual: each Mac must pin the other Mac's public key. On Mac A, open
**Pairing & Output**, enter Mac A's reachable LAN address such as
`192.168.1.20:47654`, and generate a pairing URI. Transfer that URI to Mac B over
a channel you trust. Repeat on Mac B with its own reachable address.

Before accepting a URI, decode it and compare the displayed fingerprint over a
separate trusted channel:

```sh
NEXKVM=/Applications/nexkvm.app/Contents/MacOS/nexkvm
"$NEXKVM" pair 'nexkvm://pair/v1/...'
"$NEXKVM" pair --accept 'nexkvm://pair/v1/...'
"$NEXKVM" devices
```

Accept Mac A's URI on Mac B and Mac B's URI on Mac A. The GUI's **Accept
pairing** action performs the second command; only use it after verifying the
URI and fingerprint. Restart the daemons after changing trust.

## 4. Select the connection and screen edge

For a simple source/target arrangement:

1. On the Mac with the keyboard and mouse, set **Role** to **Source**.
2. On the other Mac, set **Role** to **Target**.
3. Set **Active peer** on each Mac to the other device's exact trusted display
   name or fingerprint.
4. On one Mac, set **Connect address** to the other Mac's `IP:47654`. The other
   Mac may listen with an empty Connect address. Discovery can find a trusted
   peer, but an explicit address is easier to diagnose for the first setup.
5. On the source Mac, drag the target-screen preview to its physical side or
   select **Left**, **Right**, **Top**, or **Bottom** under **Handoff edge**.
6. Save, start the target daemon first, then start the source daemon.

If the target is physically to the right, choose **Right** on the source; moving
the pointer through the source's right edge then hands keyboard and mouse focus
to the target. Choose **Left** for a target on the left. Edge changes are applied
to a running daemon; role, active-peer, network, pairing, and sharing changes
should be followed by a daemon restart.

Use **Both** on both Macs only when each Mac's local keyboard/mouse should be
able to control the other. Configure opposite edges for a symmetric layout—for
example, Right on Mac A and Left on Mac B.

Escape is the default emergency release key. Remote focus also releases on the
configured inactivity timeout, disconnect, daemon shutdown, or movement back
across the linked boundary. Validate these safety paths before relying on the
setup.

## 5. Enable clipboard history

On both Macs, open **Sharing** and enable:

- **Sync clipboard with trusted peers**;
- **Keep encrypted clipboard history**.

Choose a bounded history capacity and maximum entry size, save the sharing
settings, and restart both daemons. Text, HTML, RTF, and supported image/custom
pasteboard formats can be synchronized. Selections marked concealed by macOS
are excluded from synchronization and history. History is encrypted on local
disk, but restored content becomes the active system clipboard and can be read
by applications with clipboard access.

History archives are local rather than bulk-replicated databases. A selection
received through clipboard sync is added to the receiving Mac's history; when
an older entry is restored, it becomes current and can then synchronize to the
peer.

Use **Sharing > Clipboard history > Refresh** to list entries and **Restore** to
make one current. CLI equivalents are:

```sh
"$NEXKVM" clipboard-history
"$NEXKVM" clipboard-restore <hex-fingerprint>
"$NEXKVM" clipboard-clear
```

## 6. Enable and send files

On both Macs, enable **Sharing > Allow files from trusted peers**, set a maximum
transfer size, optionally select a download directory, save, and restart both
daemons. Enabling this option is the receiving consent: there is no per-file
acceptance dialog, so keep an explicit Active peer selected.

Drop local files or directories onto **Sharing > Send files**, or queue them
with:

```sh
"$NEXKVM" file-send "/path/to/file" "/path/to/directory"
```

The sender validates and durably queues regular files/directories. Symlinks,
special files, unsafe relative paths, limit violations, and changed source
content are rejected. Received items are written under
`~/Downloads/NexKVM/<transfer-id>/` by default, or under the configured download
root's `NexKVM/<transfer-id>/` directory. Transfers use authenticated peer
sessions, bounded chunks, integrity hashes, checkpointing, and resume support;
the physical interruption/resume case remains a required release smoke test.

## 7. Verify the real setup

On both Macs, confirm **Doctor** is ready and **Devices** lists the intended
fingerprint. Then test, with physical hardware rather than UI automation:

- cross the configured left/right edge and type on the target;
- move the pointer back, press Escape, wait for the timeout, and disconnect;
- copy and restore text plus at least one rich/image clipboard item;
- send a file and a directory, compare hashes, then interrupt and resume a file;
- restart both apps and verify pairing, active-peer selection, and reconnect.

Record the results in [macOS KVM Smoke Checks](smoke/macos-kvm-mvp.md). Automated
tests cannot grant TCC permissions, reproduce hardware event-tap behavior, or
prove Apple notarization on a clean second Mac.

## Troubleshooting

- **No edge handoff:** verify the source role, active peer, selected edge, and
  Accessibility status. Test with a physical mouse; synthetic accessibility
  events can bypass the event-tap capture path.
- **Peer not connected:** verify both trust stores, the exact Active peer value,
  TCP `47654`, UDP broadcast `47655`, Local Network permission, and the explicit
  Connect address. An explicit address does not depend on discovery.
- **Clipboard/file settings appear ignored:** restart the daemon after saving
  sharing settings.
- **Permission remains pending:** remove the stale NexKVM entry from macOS
  privacy settings, add/approve the exact current app, then quit and relaunch.
- **Published app is blocked:** do not bypass Gatekeeper for a release. Verify
  Developer ID signing, notarization, stapling, and the downloaded checksum.
