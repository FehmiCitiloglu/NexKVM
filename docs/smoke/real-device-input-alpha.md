# Real-Device Input Alpha Smoke

Automated verification before manual smoke: `cargo fmt`, focused tests,
workspace tests, and strict Clippy passed on 2026-07-14.

This smoke record is the gate for marking the public-alpha keyboard and mouse
sharing path ready in `docs/features.md`.

Result values are `pass`, `fail`, or `skipped`. A feature tracker item can move
to `[x]` only when every required row for that item is `pass`.

## Devices

| Role | Device | OS | NexKVM build | Address |
| --- | --- | --- | --- | --- |
| Source | source-mac | macOS | local release build | SOURCE_IP:47654 |
| Target | target-mac | macOS | local release build | TARGET_IP:47654 |

## Build

Run on both devices from the repository root:

```sh
cargo build -p nexkvm --release
```

Expected result: `target/release/nexkvm` exists on both devices.

## Pairing

On the target device:

```sh
target/release/nexkvm pairing-uri TARGET_IP:47654
```

On the source device:

```sh
target/release/nexkvm pair --accept '<target-uri>'
```

On the source device:

```sh
target/release/nexkvm pairing-uri SOURCE_IP:47654
```

On the target device:

```sh
target/release/nexkvm pair --accept '<source-uri>'
```

Verify trust on both devices:

```sh
target/release/nexkvm devices
```

Expected result: each device lists the other device fingerprint.

## Source Config

Set the source config to:

```toml
[network]
listen_port = 47654
connect_addr = "TARGET_IP:47654"
enable_discovery = true
transports = ["tcp"]

[input]
control_role = "source"
active_peer = "target-mac"
handoff_edge = "right"
emergency_stop_keycode = 41
remote_focus_timeout_millis = 3000

[clipboard]
sync_enabled = false
```

## Target Config

Set the target config to:

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

Run on both devices:

```sh
target/release/nexkvm permissions
target/release/nexkvm doctor
```

Expected macOS result after granting Accessibility:

- `macOS input accessibility: ready`
- `capture ready: true`
- `inject ready: true`
- `input alpha runtime`
- `clipboard sync: disabled`

## Runtime Checks

Start the target daemon first:

```sh
target/release/nexkvm --debug
```

Start the source daemon second:

```sh
target/release/nexkvm --debug
```

Record each result:

| Check | Required for | Result | Evidence |
| --- | --- | --- | --- |
| First launch prompts are understandable | first-launch platform smoke | fail | not yet run |
| Permission prompt and restart path works | permission prompt smoke | fail | not yet run |
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
| Denied Accessibility prevents capture/injection clearly | denied-permission smoke | fail | not yet run |
| Trusted rediscovery reconnect works without explicit address | trusted reconnect smoke | fail | not yet run |

## Feature Tracker Rule

Do not mark `End-to-end keyboard and mouse sharing between real devices` or
`Real cursor edge crossing between machines` complete until the relevant rows
above are `pass`.

Do not mark `Pairing, restart, and trusted reconnect smoke records` complete
unless pairing, daemon restart, and trusted rediscovery reconnect rows are all
`pass`.
