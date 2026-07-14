# Real-Device Input Alpha Smoke

Automated verification before manual smoke: `cargo fmt`, focused tests,
workspace tests, and strict Clippy passed on 2026-07-14.

This smoke record is the gate for marking the Mac-to-Windows public-alpha
keyboard and mouse sharing path ready in `docs/features.md`. The Mac is the
input source so macOS capture, edge handoff, and source-side suppression are
covered; Windows is the target so native `SendInput` injection is covered.

Result values are `pass`, `fail`, or `skipped`. A feature tracker item can move
to `[x]` only when every required row for that item is `pass`.

## Devices

| Role | Device | OS | NexKVM build | Address |
| --- | --- | --- | --- | --- |
| Source | source-mac | macOS | local release build | SOURCE_IP:47654 |
| Target | target-windows | Windows | local release build | TARGET_IP:47654 |

## Build

Run on the Mac from the repository root:

```sh
cargo build -p nexkvm --release
```

Run in PowerShell on Windows from the repository root:

```powershell
cargo build -p nexkvm --release
```

Expected result: `target/release/nexkvm` exists on macOS and
`target\release\nexkvm.exe` exists on Windows.

## Pairing

On Windows:

```powershell
.\target\release\nexkvm.exe pairing-uri TARGET_IP:47654
```

On the Mac:

```sh
target/release/nexkvm pair --accept '<target-uri>'
```

On the Mac:

```sh
target/release/nexkvm pairing-uri SOURCE_IP:47654
```

On Windows, use double quotes around the URI:

```powershell
.\target\release\nexkvm.exe pair --accept "<source-uri>"
```

Verify trust on the Mac:

```sh
target/release/nexkvm devices
```

Verify trust on Windows:

```powershell
.\target\release\nexkvm.exe devices
```

Expected result: each device lists the other device fingerprint.

## Source Config

Set the source config to:

`~/Library/Application Support/nexkvm/config.toml`

```toml
[network]
listen_port = 47654
connect_addr = "TARGET_IP:47654"
enable_discovery = true
transports = ["tcp"]

[input]
control_role = "source"
active_peer = "target-windows"
handoff_edge = "right"
emergency_stop_keycode = 41
remote_focus_timeout_millis = 3000

[clipboard]
sync_enabled = false
```

## Target Config

Set the target config to:

`$env:APPDATA\nexkvm\config.toml`

```toml
[network]
listen_port = 47654
enable_discovery = true
transports = ["tcp"]

[input]
control_role = "target"
active_peer = "source-mac"
handoff_edge = "left"
emergency_stop_keycode = 41
remote_focus_timeout_millis = 3000

[clipboard]
sync_enabled = false
```

## Permission Checks

Run on the Mac:

```sh
target/release/nexkvm permissions
target/release/nexkvm doctor
```

Expected result after granting macOS Accessibility and restarting NexKVM:

- `macOS input accessibility: ready`
- `capture ready: true`
- `inject ready: true`
- `input alpha runtime`
- `clipboard sync: disabled`

Run on Windows:

```powershell
.\target\release\nexkvm.exe permissions
.\target\release\nexkvm.exe doctor
```

Windows has no macOS-style Accessibility prompt. Expected `doctor` results:

- `input-capture: available`
- `input-injection: available`
- `clipboard sync: disabled`

Do not run the Windows target elevated unless the application receiving input
is also elevated. Windows UIPI can prevent a normal NexKVM process from
injecting into a higher-integrity application.

## Runtime Checks

Start the Windows target daemon first:

```powershell
.\target\release\nexkvm.exe --debug
```

Start the Mac source daemon second:

```sh
target/release/nexkvm --debug
```

Record each result:

| Check | Required for | Result | Evidence |
| --- | --- | --- | --- |
| First launch prompts are understandable on macOS and Windows | first-launch platform smoke | fail | not yet run |
| macOS Accessibility prompt and restart path works | permission prompt smoke | fail | not yet run |
| Pairing persists on both devices | pairing smoke | fail | not yet run |
| Explicit peer address connects | input alpha | fail | not yet run |
| Pointer crosses configured edge to target | cursor edge crossing | fail | not yet run |
| Keyboard input reaches target | keyboard sharing | fail | not yet run |
| Mouse buttons reach target | mouse sharing | fail | not yet run |
| Scroll reaches target | mouse sharing | fail | not yet run |
| Source input is suppressed during remote focus | input alpha safety | fail | not yet run |
| Escape releases remote focus without forwarding Escape | emergency release | fail | not yet run |
| Focus timeout releases remote focus | timeout release | fail | not yet run |
| Target disconnect releases source focus | disconnect release | fail | not yet run |
| Daemon restart preserves pairing and reconnects | restart and reconnect smoke | fail | not yet run |
| Denied macOS Accessibility prevents capture clearly | denied-permission smoke | fail | not yet run |
| Trusted rediscovery reconnect works without explicit address | trusted reconnect smoke | fail | not yet run |

## Feature Tracker Rule

Do not mark `End-to-end keyboard and mouse sharing between real devices` or
`Real cursor edge crossing between machines` complete for the Mac-to-Windows
alpha path until the relevant rows above are `pass`.

Do not mark `Pairing, restart, and trusted reconnect smoke records` complete
unless pairing, daemon restart, and trusted rediscovery reconnect rows are all
`pass`.
