# Real-Device Input Alpha Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make NexKVM's real-device keyboard and mouse sharing path safe enough for a public alpha, then document and evidence-gate the feature tracker update.

**Architecture:** Keep the current daemon, secure TCP connection, platform input, and `input_session` architecture. Harden the source-side input loop so emergency stop only applies during remote focus and every remote-focus failure releases local suppression. Keep clipboard sync explicitly opt-in so the input alpha does not accidentally ship an unproven clipboard runtime path.

**Tech Stack:** Rust, Tokio, existing `nexkvm-input` traits, existing `nexkvm-network::Connection`, `nexkvm-storage` TOML config, desktop CLI/GUI docs, Markdown smoke records.

## Global Constraints

- macOS real-device input sharing is the required smoke target.
- Windows smoke evidence is optional and does not block the alpha.
- Linux input is reported as capability-limited unless the already-started portal path passes a real Wayland session smoke.
- No screen streaming, hover previews, audio routing, mobile companion, cloud sync, plugin marketplace, WebRTC remote mode, relay mode, or file transfer.
- No broad UI redesign.
- No claim that clipboard sync is release-ready.
- No full commercial release claim.
- `docs/features.md` is updated only to the level proven by recorded evidence.

---

## File Structure

- Modify `apps/desktop/src/input_session.rs`: source-side focus, emergency key, send-error, timeout, and suppression-release behavior.
- Modify `crates/storage/src/lib.rs`: add an explicit `[clipboard] sync_enabled` config section defaulting to `false`.
- Modify `apps/desktop/src/main.rs`: gate the daemon clipboard peer handler behind the new config and expose a small testable helper.
- Modify `apps/desktop/src/cli.rs`: add pure formatting for input alpha runtime readiness.
- Modify `apps/desktop/src/main.rs`: print the input alpha runtime block in `nexkvm doctor`.
- Create `docs/smoke/real-device-input-alpha.md`: two-device smoke commands, checks, and evidence record.
- Create `docs/alpha-release-notes.md`: public-alpha scope and known limitations.
- Modify `docs/features.md`: only after manual smoke evidence passes, mark the proven input and release-readiness items complete.

---

### Task 1: Harden Source-Side Input Focus Release

**Files:**
- Modify: `apps/desktop/src/input_session.rs`

**Interfaces:**
- Consumes: `forward_extended_until_error<C, K, S>(capture, connection, first_id, edge, emergency_stop_keycode, remote_focus_timeout_millis, set_suppressed)`.
- Produces: The same function, with these behaviors:
  - emergency key before remote focus is treated as a local-only key and does not stop forwarding;
  - emergency key during remote focus returns `InputSessionError::EmergencyStop` without forwarding the key;
  - capture errors, send errors, timeout release, and remote return all release source-side suppression when remote focus was active.

- [ ] **Step 1: Add failing tests for emergency key scope and send-error release**

In `apps/desktop/src/input_session.rs`, inside `#[cfg(test)] mod tests`, add this helper after `MemoryConnection`:

```rust
#[derive(Debug, Default)]
struct FailingSendConnection {
    sent: Mutex<Vec<Envelope>>,
}

#[async_trait]
impl Connection for FailingSendConnection {
    fn kind(&self) -> TransportKind {
        TransportKind::Tcp
    }

    fn peer_addr(&self) -> SocketAddr {
        "127.0.0.1:47654".parse().unwrap()
    }

    async fn send(&self, envelope: Envelope) -> Result<(), NetworkError> {
        self.sent.lock().unwrap().push(envelope);
        Err(NetworkError::Closed)
    }

    async fn recv(&self) -> Result<Envelope, NetworkError> {
        Err(NetworkError::Closed)
    }

    async fn close(&self) -> Result<(), NetworkError> {
        Ok(())
    }
}
```

Replace the current `emergency_key_stops_forwarding_without_sending_event` test with:

```rust
#[tokio::test]
async fn local_emergency_key_before_handoff_does_not_stop_forwarding() {
    let capture = QueueCapture::new(vec![InputEvent::KeyPress(41)]);
    let connection = Arc::new(MemoryConnection::default());
    let suppressions = Arc::new(Mutex::new(Vec::new()));
    let suppressions_for_callback = Arc::clone(&suppressions);

    let error = forward_extended_until_error(
        &capture,
        &*connection,
        MessageId(30),
        HandoffEdge::Right,
        41,
        3_000,
        move |suppressed| suppressions_for_callback.lock().unwrap().push(suppressed),
    )
    .await
    .unwrap_err();

    assert!(matches!(error, InputSessionError::Codec(_)));
    assert!(connection.sent.lock().unwrap().is_empty());
    assert!(suppressions.lock().unwrap().is_empty());
}

#[tokio::test]
async fn emergency_key_stops_remote_forwarding_without_sending_key() {
    let capture = QueueCapture::new(vec![
        InputEvent::PointerMove { x: 1.0, y: 0.5 },
        InputEvent::KeyPress(41),
    ]);
    let connection = Arc::new(MemoryConnection::default());
    let suppressions = Arc::new(Mutex::new(Vec::new()));
    let suppressions_for_callback = Arc::clone(&suppressions);

    let error = forward_extended_until_error(
        &capture,
        &*connection,
        MessageId(30),
        HandoffEdge::Right,
        41,
        3_000,
        move |suppressed| suppressions_for_callback.lock().unwrap().push(suppressed),
    )
    .await
    .unwrap_err();

    assert!(matches!(error, InputSessionError::EmergencyStop));
    let sent = connection.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        decode_input_event(sent[0].clone()).unwrap(),
        InputEvent::PointerMove { x: 0.0, y: 0.5 }
    );
    assert_eq!(suppressions.lock().unwrap().as_slice(), &[true, false]);
}

#[tokio::test]
async fn send_failure_releases_remote_suppression() {
    let capture = QueueCapture::new(vec![InputEvent::PointerMove { x: 1.0, y: 0.5 }]);
    let connection = Arc::new(FailingSendConnection::default());
    let suppressions = Arc::new(Mutex::new(Vec::new()));
    let suppressions_for_callback = Arc::clone(&suppressions);

    let error = forward_extended_until_error(
        &capture,
        &*connection,
        MessageId(70),
        HandoffEdge::Right,
        41,
        3_000,
        move |suppressed| suppressions_for_callback.lock().unwrap().push(suppressed),
    )
    .await
    .unwrap_err();

    assert!(matches!(error, InputSessionError::Codec(_)));
    assert_eq!(connection.sent.lock().unwrap().len(), 1);
    assert_eq!(suppressions.lock().unwrap().as_slice(), &[true, false]);
}
```

- [ ] **Step 2: Run the focused tests and confirm they fail**

Run:

```bash
cargo test -p nexkvm input_session
```

Expected: FAIL. The current implementation stops on a local emergency key before remote focus and does not release suppression after a send failure.

- [ ] **Step 3: Add release-on-error behavior**

In `forward_extended_until_error`, replace the event retrieval and emergency-key block with:

```rust
let event = if share.is_remote() && remote_focus_timeout_millis > 0 {
    match tokio::time::timeout(
        std::time::Duration::from_millis(remote_focus_timeout_millis),
        capture.next_event(),
    )
    .await
    {
        Ok(Ok(event)) => event,
        Ok(Err(error)) => {
            if share.release_remote() {
                set_suppressed(false);
            }
            return Err(error.into());
        }
        Err(_) => {
            if share.release_remote() {
                set_suppressed(false);
            }
            continue;
        }
    }
} else {
    capture.next_event().await?
};

if share.is_remote()
    && matches!(event, InputEvent::KeyPress(keycode) if keycode == emergency_stop_keycode)
{
    if share.release_remote() {
        set_suppressed(false);
    }
    return Err(InputSessionError::EmergencyStop);
}
```

Then replace the send block at the bottom of the loop with:

```rust
if let Some(event) = routed {
    if let Err(error) = connection.send(encode_input_event(next_id, event)).await {
        if share.release_remote() {
            set_suppressed(false);
        }
        return Err(error.into());
    }
    next_id = next_id.next();
}
```

- [ ] **Step 4: Run the focused tests and confirm they pass**

Run:

```bash
cargo test -p nexkvm input_session
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add apps/desktop/src/input_session.rs
git commit -m "fix: release input focus on forwarding failures"
```

---

### Task 2: Keep Clipboard Sync Out Of The Input Alpha By Default

**Files:**
- Modify: `crates/storage/src/lib.rs`
- Modify: `apps/desktop/src/main.rs`

**Interfaces:**
- Produces: `Config::clipboard.sync_enabled: bool`, default `false`.
- Produces: `clipboard_runtime_enabled(sync_enabled: bool, can_access_clipboard: bool) -> bool` in `apps/desktop/src/main.rs`.
- Consumers: `run_daemon` uses `clipboard_runtime_enabled(config.clipboard.sync_enabled, clipboard_can_access)` before creating the clipboard peer handler.

- [ ] **Step 1: Add failing storage tests**

In `crates/storage/src/lib.rs`, inside `#[cfg(test)] mod tests`, add:

```rust
#[test]
fn clipboard_config_defaults_to_disabled() {
    let cfg = Config::default();
    assert!(!cfg.clipboard.sync_enabled);
}

#[test]
fn clipboard_config_round_trips_through_toml() {
    let text = r#"
[clipboard]
sync_enabled = true
"#;

    let parsed: Config = toml::from_str(text).unwrap();
    assert!(parsed.clipboard.sync_enabled);

    let rendered = toml::to_string_pretty(&parsed).unwrap();
    assert!(rendered.contains("[clipboard]"));
    assert!(rendered.contains("sync_enabled = true"));
}
```

Also add this assertion to `defaults_round_trip_through_toml`:

```rust
assert_eq!(parsed.clipboard.sync_enabled, cfg.clipboard.sync_enabled);
```

- [ ] **Step 2: Run the storage tests and confirm they fail**

Run:

```bash
cargo test -p nexkvm-storage clipboard_config
```

Expected: FAIL because `Config` has no `clipboard` field.

- [ ] **Step 3: Add the config section**

In `Config`, after `pub input: InputConfig,` add:

```rust
/// Clipboard runtime settings.
pub clipboard: ClipboardConfig,
```

After `InputControlRole`, add:

```rust
/// `[clipboard]` section.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ClipboardConfig {
    /// Enable runtime clipboard synchronization with trusted peers.
    pub sync_enabled: bool,
}

impl Default for ClipboardConfig {
    fn default() -> Self {
        Self {
            sync_enabled: false,
        }
    }
}
```

- [ ] **Step 4: Run the storage tests and confirm they pass**

Run:

```bash
cargo test -p nexkvm-storage clipboard_config
cargo test -p nexkvm-storage defaults_round_trip_through_toml
```

Expected: PASS.

- [ ] **Step 5: Add daemon gating tests**

At the bottom of `apps/desktop/src/main.rs`, add:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_runtime_requires_config_and_platform_access() {
        assert!(!clipboard_runtime_enabled(false, false));
        assert!(!clipboard_runtime_enabled(false, true));
        assert!(!clipboard_runtime_enabled(true, false));
        assert!(clipboard_runtime_enabled(true, true));
    }
}
```

- [ ] **Step 6: Run the daemon gating test and confirm it fails**

Run:

```bash
cargo test -p nexkvm clipboard_runtime_requires_config_and_platform_access
```

Expected: FAIL because `clipboard_runtime_enabled` is not defined.

- [ ] **Step 7: Gate the clipboard peer handler**

In `run_daemon`, replace:

```rust
let clipboard_peer_handler = create_clipboard_peer_handler(clipboard_can_access, device.id);
```

with:

```rust
let clipboard_peer_handler = create_clipboard_peer_handler(
    clipboard_runtime_enabled(config.clipboard.sync_enabled, clipboard_can_access),
    device.id,
);
```

Add this helper near `merge_peer_handlers`:

```rust
fn clipboard_runtime_enabled(sync_enabled: bool, can_access_clipboard: bool) -> bool {
    sync_enabled && can_access_clipboard
}
```

In `create_clipboard_peer_handler`, replace the comment above `let sync = Arc::new(...)` with:

```rust
// Clipboard sync is opt-in for the input alpha and still uses the existing
// clipboard state machine until the dedicated clipboard release slice.
```

- [ ] **Step 8: Run the focused tests and confirm they pass**

Run:

```bash
cargo test -p nexkvm-storage clipboard_config
cargo test -p nexkvm clipboard_runtime_requires_config_and_platform_access
```

Expected: PASS.

- [ ] **Step 9: Commit**

```bash
git add crates/storage/src/lib.rs apps/desktop/src/main.rs
git commit -m "feat: make clipboard sync explicitly opt-in"
```

---

### Task 3: Add Input Alpha Readiness To Doctor Output

**Files:**
- Modify: `apps/desktop/src/cli.rs`
- Modify: `apps/desktop/src/main.rs`

**Interfaces:**
- Produces: `format_input_alpha_runtime(role, active_peer, handoff_edge, emergency_stop_keycode, remote_focus_timeout_millis, connect_addr, clipboard_sync_enabled) -> String`.
- Consumes: `Config.input`, `Config.network.connect_addr`, and `Config.clipboard.sync_enabled` in `doctor()`.

- [ ] **Step 1: Add failing formatter tests**

In `apps/desktop/src/cli.rs`, inside `#[cfg(test)] mod tests`, add:

```rust
#[test]
fn input_alpha_runtime_report_lists_release_relevant_settings() {
    let rendered = format_input_alpha_runtime(
        "source",
        Some("studio-mac"),
        "right",
        41,
        3_000,
        Some("192.168.1.20:47654"),
        false,
    );

    assert!(rendered.contains("input alpha runtime"));
    assert!(rendered.contains("role: source"));
    assert!(rendered.contains("active peer: studio-mac"));
    assert!(rendered.contains("handoff edge: right"));
    assert!(rendered.contains("emergency keycode: 41"));
    assert!(rendered.contains("remote focus timeout: 3000 ms"));
    assert!(rendered.contains("explicit connect: 192.168.1.20:47654"));
    assert!(rendered.contains("clipboard sync: disabled"));
}

#[test]
fn input_alpha_runtime_report_handles_unset_peer_and_connect_addr() {
    let rendered = format_input_alpha_runtime("disabled", None, "right", 41, 3_000, None, false);

    assert!(rendered.contains("active peer: unset"));
    assert!(rendered.contains("explicit connect: disabled"));
}
```

- [ ] **Step 2: Run formatter tests and confirm they fail**

Run:

```bash
cargo test -p nexkvm input_alpha_runtime_report
```

Expected: FAIL because `format_input_alpha_runtime` is not defined.

- [ ] **Step 3: Implement the formatter**

In `apps/desktop/src/cli.rs`, after `format_macos_input_report`, add:

```rust
/// Render release-relevant input runtime configuration for `nexkvm doctor`.
#[must_use]
pub fn format_input_alpha_runtime(
    role: &str,
    active_peer: Option<&str>,
    handoff_edge: &str,
    emergency_stop_keycode: u32,
    remote_focus_timeout_millis: u64,
    connect_addr: Option<&str>,
    clipboard_sync_enabled: bool,
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "input alpha runtime");
    let _ = writeln!(out, "  role: {role}");
    let _ = writeln!(out, "  active peer: {}", active_peer.unwrap_or("unset"));
    let _ = writeln!(out, "  handoff edge: {handoff_edge}");
    let _ = writeln!(out, "  emergency keycode: {emergency_stop_keycode}");
    let _ = writeln!(out, "  remote focus timeout: {remote_focus_timeout_millis} ms");
    let _ = writeln!(out, "  explicit connect: {}", connect_addr.unwrap_or("disabled"));
    let _ = writeln!(
        out,
        "  clipboard sync: {}",
        if clipboard_sync_enabled {
            "enabled"
        } else {
            "disabled"
        }
    );
    out.truncate(out.trim_end().len());
    out
}
```

- [ ] **Step 4: Wire the formatter into `doctor`**

In `apps/desktop/src/main.rs`, inside `doctor()` after `println!("  require pairing: {}", config.security.require_pairing);`, add:

```rust
for line in cli::format_input_alpha_runtime(
    storage_input_role_label(config.input.control_role),
    config.input.active_peer.as_deref(),
    storage_input_edge_label(config.input.handoff_edge),
    config.input.emergency_stop_keycode,
    config.input.remote_focus_timeout_millis,
    config.network.connect_addr.as_deref(),
    config.clipboard.sync_enabled,
)
.lines()
{
    println!("  {line}");
}
```

Near `input_runtime_role`, add:

```rust
fn storage_input_role_label(role: nexkvm_storage::InputControlRole) -> &'static str {
    match role {
        nexkvm_storage::InputControlRole::Disabled => "disabled",
        nexkvm_storage::InputControlRole::Source => "source",
        nexkvm_storage::InputControlRole::Target => "target",
        nexkvm_storage::InputControlRole::Both => "both",
    }
}

fn storage_input_edge_label(edge: nexkvm_storage::InputHandoffEdge) -> &'static str {
    match edge {
        nexkvm_storage::InputHandoffEdge::Left => "left",
        nexkvm_storage::InputHandoffEdge::Right => "right",
        nexkvm_storage::InputHandoffEdge::Top => "top",
        nexkvm_storage::InputHandoffEdge::Bottom => "bottom",
    }
}
```

- [ ] **Step 5: Run formatter tests and a CLI smoke test**

Run:

```bash
cargo test -p nexkvm input_alpha_runtime_report
cargo test -p nexkvm --test cli help_lists_the_subcommands
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add apps/desktop/src/cli.rs apps/desktop/src/main.rs
git commit -m "feat: show input alpha readiness in doctor"
```

---

### Task 4: Add Real-Device Input Smoke Documentation And Alpha Notes

**Files:**
- Create: `docs/smoke/real-device-input-alpha.md`
- Create: `docs/alpha-release-notes.md`
- Modify: `docs/README.md`

**Interfaces:**
- Produces: a manual evidence checklist with pass/fail/skipped result values.
- Produces: alpha release notes that explicitly exclude unproven roadmap features.

- [ ] **Step 1: Create the smoke record**

Create `docs/smoke/real-device-input-alpha.md`:

```markdown
# Real-Device Input Alpha Smoke

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
```

- [ ] **Step 2: Create alpha release notes**

Create `docs/alpha-release-notes.md`:

```markdown
# NexKVM Public Alpha Notes

This alpha focuses on real-device keyboard and mouse sharing between trusted
desktop peers over a LAN connection.

## Included In The Alpha

- macOS real-device keyboard and mouse sharing when the smoke record passes.
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
- Windows input smoke evidence is optional for this alpha.
- Linux input is capability-limited unless a real Wayland portal smoke passes.
- Signed installers, SBOM, checksums, and every-OS smoke evidence remain
  production release gates.

## Publishing Rule

Publish only with the current `docs/smoke/real-device-input-alpha.md` evidence
record and keep every unsupported feature listed above in the known limitations.
```

- [ ] **Step 3: Link the docs from `docs/README.md`**

Add these bullets to the documentation list in `docs/README.md`:

```markdown
- [Real-Device Input Alpha Smoke](smoke/real-device-input-alpha.md)
- [Public Alpha Notes](alpha-release-notes.md)
```

- [ ] **Step 4: Run Markdown/diff checks**

Run:

```bash
git diff --check
```

Expected: PASS with no whitespace errors.

- [ ] **Step 5: Commit**

```bash
git add docs/smoke/real-device-input-alpha.md docs/alpha-release-notes.md docs/README.md
git commit -m "docs: add real-device input alpha smoke gate"
```

---

### Task 5: Run Automated Verification For The Alpha Spine

**Files:**
- Modify: `docs/smoke/real-device-input-alpha.md` after automated verification passes.

**Interfaces:**
- Consumes all code and docs changed in Tasks 1-4.
- Produces a clean automated verification baseline before manual smoke.

- [ ] **Step 1: Run formatting**

Run:

```bash
cargo fmt --all -- --check
```

Expected: PASS.

- [ ] **Step 2: Run focused package tests**

Run:

```bash
cargo test -p nexkvm input_session
cargo test -p nexkvm input_alpha_runtime_report
cargo test -p nexkvm clipboard_runtime_requires_config_and_platform_access
cargo test -p nexkvm-storage clipboard_config
cargo test -p nexkvm --test cli pair_accept_persists_trusted_device
```

Expected: PASS for every command.

- [ ] **Step 3: Run full workspace tests**

Run:

```bash
cargo test --workspace --all-features
```

Expected: PASS. If UDP discovery tests fail with `Operation not permitted` inside a restricted sandbox, rerun the same target on a normal developer machine before treating it as a code failure.

- [ ] **Step 4: Run clippy**

Run:

```bash
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Expected: PASS.

- [ ] **Step 5: Record verification result**

If all commands above pass, add this line to the top of `docs/smoke/real-device-input-alpha.md` under the title:

```markdown
Automated verification before manual smoke: `cargo fmt`, focused tests, workspace tests, and clippy passed on 2026-07-09.
```

- [ ] **Step 6: Commit the verification note**

```bash
git add docs/smoke/real-device-input-alpha.md
git commit -m "docs: record input alpha automated verification"
```

---

### Task 6: Record Manual Smoke Evidence And Update Feature Tracker

**Files:**
- Modify: `docs/smoke/real-device-input-alpha.md`
- Modify: `docs/features.md`

**Interfaces:**
- Consumes: the manual smoke table in `docs/smoke/real-device-input-alpha.md`.
- Produces: feature tracker updates only for rows that have recorded `pass` results.

- [ ] **Step 1: Run the manual smoke on two devices**

Follow `docs/smoke/real-device-input-alpha.md` exactly. Replace each `fail | not yet run` table row with the actual result and evidence. Use concrete evidence such as:

```markdown
| Explicit peer address connects | input alpha | pass | source log: `explicit peer connected`; target log: `accepted peer connection` |
```

- [ ] **Step 2: Update product feature rows only after passing evidence**

If the rows for explicit connection, pointer edge crossing, keyboard input, mouse buttons, scroll, suppression, emergency release, timeout release, and disconnect release are all `pass`, replace these rows in `docs/features.md`:

```markdown
- [ ] End-to-end keyboard and mouse sharing between real devices.
- [ ] Real cursor edge crossing between machines.
```

with:

```markdown
- [x] End-to-end keyboard and mouse sharing between real devices for the macOS
  public-alpha path, with evidence recorded in
  `docs/smoke/real-device-input-alpha.md`.
- [x] Real cursor edge crossing between machines for the macOS public-alpha
  path, with evidence recorded in `docs/smoke/real-device-input-alpha.md`.
```

- [ ] **Step 3: Update release-readiness rows only after passing evidence**

If the matching smoke rows are `pass`, replace these release-readiness rows in `docs/features.md`:

```markdown
- [ ] First-launch platform smoke records.
- [ ] Permission prompt smoke records.
- [ ] Input capture and injection smoke records.
- [ ] Pairing, restart, and trusted reconnect smoke records.
- [ ] Denied-permission behavior smoke records.
```

with:

```markdown
- [x] First-launch platform smoke records for the macOS public-alpha input
  path.
- [x] Permission prompt smoke records for the macOS public-alpha input path.
- [x] Input capture and injection smoke records for the macOS public-alpha input
  path.
- [x] Pairing, restart, and trusted reconnect smoke records for the macOS
  public-alpha input path.
- [x] Denied-permission behavior smoke records for the macOS public-alpha input
  path.
```

If trusted rediscovery reconnect does not pass, leave `Pairing, restart, and trusted reconnect smoke records` as `[ ]` and add this adjacent planned row:

```markdown
- [ ] Trusted rediscovery reconnect smoke follow-up for the macOS public-alpha
  input path.
```

- [ ] **Step 4: Keep non-alpha features planned**

Confirm these rows remain `[ ]`:

```markdown
- [ ] Real shared clipboard read/write/sync between machines.
- [ ] Drag-and-drop file transfer between machines.
- [ ] Follow-mouse audio routing.
- [ ] Shared headset mode.
- [ ] Screen streaming.
```

- [ ] **Step 5: Run diff and focused docs checks**

Run:

```bash
git diff --check
rg -n "not yet run|\\| fail \\|" docs/smoke/real-device-input-alpha.md
```

Expected: `git diff --check` passes. The `rg` command prints no rows for required smoke checks that were needed to mark a feature complete.

- [ ] **Step 6: Commit**

```bash
git add docs/smoke/real-device-input-alpha.md docs/features.md
git commit -m "docs: record real-device input alpha evidence"
```
