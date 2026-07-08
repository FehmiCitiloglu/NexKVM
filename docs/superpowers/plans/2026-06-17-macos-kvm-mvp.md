# macOS KVM MVP Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the first macOS keyboard and mouse sharing path work over NexKVM's existing trusted TCP peer connections, with permission diagnostics and release signing/notarization validation.

**Architecture:** Add a small input session layer in `apps/desktop` that routes `MessageKind::Input` envelopes between peer connections and platform input traits. Extend `nexkvm-platform-macos` in narrow layers: permission reporting, pure input translation, then native CGEvent capture/posting. Harden macOS packaging by checking entitlements, hardened runtime, notarization, stapling, and Gatekeeper acceptance.

**Tech Stack:** Rust, Tokio, existing `nexkvm-input` traits, existing `nexkvm-network::Connection`, macOS ApplicationServices/CoreGraphics FFI, `codesign`, `xcrun notarytool`, `xcrun stapler`, `spctl`.

---

## File Structure

- Create `apps/desktop/src/input_session.rs`: owns input envelope encode/decode, source forwarding, target injection, and tests using in-memory fake connections/injectors.
- Modify `apps/desktop/src/main.rs`: wire `input_session` into accepted and outbound peer connections after the testable session module exists.
- Modify `apps/desktop/src/connection.rs`: return established peer connections to the session router instead of only holding and logging them.
- Modify `crates/storage/src/lib.rs`: add a minimal `[input]` config section for `control_role`, `active_peer`, and `emergency_stop_keycode`.
- Modify `crates/platform/platform-macos/src/lib.rs`: expose macOS permission report and native capture/injector constructors.
- Create `crates/platform/platform-macos/src/permissions.rs`: typed macOS input permission report built on Accessibility status.
- Create `crates/platform/platform-macos/src/capture.rs`: CGEvent tap wrapper behind `InputCapture`.
- Modify `crates/platform/platform-macos/src/inject.rs`: add native `MacosInputInjector` that posts `CgEventPlan`s.
- Modify `apps/desktop/src/cli.rs`: format macOS input permission/signing diagnostics in `doctor`.
- Modify `packaging/macos/Info.plist`: add usage strings needed by macOS privacy prompts where applicable.
- Create `packaging/macos/nexkvm.entitlements`: hardened runtime entitlements for release signing.
- Modify `scripts/package-macos.sh`: require release signing inputs when `NEXKVM_RELEASE=1`, pass entitlements, notarize, staple, and validate.
- Modify `docs/features.md`: mark only the implemented macOS runtime and packaging pieces.
- Create `docs/smoke/macos-kvm-mvp.md`: record required manual smoke checks and exact commands.

---

### Task 1: Add Input Runtime Config

**Files:**
- Modify: `crates/storage/src/lib.rs`

- [ ] **Step 1: Write the failing config round-trip test**

Add this test inside `crates/storage/src/lib.rs` `#[cfg(test)] mod tests`:

```rust
#[test]
fn input_config_round_trips_through_toml() {
    let text = r#"
[input]
control_role = "source"
active_peer = "studio-mac"
emergency_stop_keycode = 41
"#;

    let parsed: Config = toml::from_str(text).unwrap();

    assert_eq!(parsed.input.control_role, InputControlRole::Source);
    assert_eq!(parsed.input.active_peer.as_deref(), Some("studio-mac"));
    assert_eq!(parsed.input.emergency_stop_keycode, 41);

    let rendered = toml::to_string_pretty(&parsed).unwrap();
    assert!(rendered.contains("[input]"));
    assert!(rendered.contains("control_role = \"source\""));
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p nexkvm-storage input_config_round_trips_through_toml`

Expected: FAIL because `Config` has no `input` field and `InputControlRole` is undefined.

- [ ] **Step 3: Add the minimal config types**

In `crates/storage/src/lib.rs`, add an `input` field to `Config`:

```rust
/// Input-sharing runtime settings.
pub input: InputConfig,
```

Add these types near the other config sections:

```rust
/// `[input]` section.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct InputConfig {
    /// Runtime role for keyboard/mouse sharing.
    pub control_role: InputControlRole,
    /// Friendly trusted-peer name or fingerprint selected as the active target.
    pub active_peer: Option<String>,
    /// HID usage id for the emergency stop key. Default 41 is Escape.
    pub emergency_stop_keycode: u32,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            control_role: InputControlRole::Disabled,
            active_peer: None,
            emergency_stop_keycode: 41,
        }
    }
}

/// Whether this daemon captures, injects, both, or neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InputControlRole {
    /// Do not run keyboard/mouse sharing.
    Disabled,
    /// Capture local input and send it to `active_peer`.
    Source,
    /// Inject input received from a trusted peer.
    Target,
    /// Enable source and target behavior.
    Both,
}

impl Default for InputControlRole {
    fn default() -> Self {
        Self::Disabled
    }
}
```

- [ ] **Step 4: Extend the existing default round-trip test**

In `defaults_round_trip_through_toml`, add:

```rust
assert_eq!(parsed.input.control_role, cfg.input.control_role);
assert_eq!(
    parsed.input.emergency_stop_keycode,
    cfg.input.emergency_stop_keycode
);
```

- [ ] **Step 5: Run storage tests**

Run: `cargo test -p nexkvm-storage`

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/storage/src/lib.rs
git commit -m "feat: add input sharing runtime config"
```

---

### Task 2: Add Input Envelope Codec

**Files:**
- Create: `apps/desktop/src/input_session.rs`
- Modify: `apps/desktop/src/main.rs`

- [ ] **Step 1: Write failing codec tests**

Create `apps/desktop/src/input_session.rs` with tests first:

```rust
use bytes::Bytes;
use nexkvm_input::InputEvent;
use nexkvm_protocol::{Envelope, MessageId, MessageKind, PROTOCOL_VERSION};

#[derive(Debug, thiserror::Error)]
pub enum InputSessionError {
    #[error("input payload codec error: {0}")]
    Codec(String),
    #[error("unexpected message kind: {0:?}")]
    UnexpectedKind(MessageKind),
}

pub fn encode_input_event(_id: MessageId, _event: InputEvent) -> Envelope {
    panic!("red test: implementation intentionally missing")
}

pub fn decode_input_event(_envelope: Envelope) -> Result<InputEvent, InputSessionError> {
    panic!("red test: implementation intentionally missing")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_event_round_trips_through_envelope_body() {
        let event = InputEvent::KeyPress(0x04);
        let envelope = encode_input_event(MessageId(7), event);

        assert_eq!(envelope.version, PROTOCOL_VERSION);
        assert_eq!(envelope.id, MessageId(7));
        assert_eq!(envelope.kind, MessageKind::Input);
        assert_eq!(decode_input_event(envelope).unwrap(), event);
    }

    #[test]
    fn rejects_non_input_envelopes() {
        let envelope = Envelope::new(
            PROTOCOL_VERSION,
            MessageId(1),
            MessageKind::Clipboard,
            Bytes::from_static(b"not input"),
        );

        assert!(matches!(
            decode_input_event(envelope),
            Err(InputSessionError::UnexpectedKind(MessageKind::Clipboard))
        ));
    }
}
```

Add this module line to `apps/desktop/src/main.rs`:

```rust
mod input_session;
```

- [ ] **Step 2: Run tests to verify failure**

Run: `cargo test -p nexkvm input_session`

Expected: FAIL because both functions panic with `red test: implementation intentionally missing`.

- [ ] **Step 3: Add dependencies**

In `apps/desktop/Cargo.toml`, add:

```toml
bytes.workspace = true
thiserror.workspace = true
serde_json.workspace = true
```

- [ ] **Step 4: Implement the codec**

Replace the two function bodies in `apps/desktop/src/input_session.rs`:

```rust
pub fn encode_input_event(id: MessageId, event: InputEvent) -> Envelope {
    let body = serde_json::to_vec(&event).expect("InputEvent serialization is infallible");
    Envelope::new(PROTOCOL_VERSION, id, MessageKind::Input, Bytes::from(body))
}

pub fn decode_input_event(envelope: Envelope) -> Result<InputEvent, InputSessionError> {
    if envelope.kind != MessageKind::Input {
        return Err(InputSessionError::UnexpectedKind(envelope.kind));
    }
    serde_json::from_slice(&envelope.body)
        .map_err(|error| InputSessionError::Codec(error.to_string()))
}
```

- [ ] **Step 5: Run desktop tests**

Run: `cargo test -p nexkvm input_session`

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add apps/desktop/Cargo.toml apps/desktop/src/main.rs apps/desktop/src/input_session.rs
git commit -m "feat: add input envelope codec"
```

---

### Task 3: Route Input Events Over Connections

**Files:**
- Modify: `apps/desktop/src/input_session.rs`

- [ ] **Step 1: Write failing source and target routing tests**

Append this test support code inside `#[cfg(test)] mod tests` in `apps/desktop/src/input_session.rs`:

```rust
use async_trait::async_trait;
use nexkvm_input::{InputCapture, InputError, InputInjector};
use nexkvm_network::{Connection, NetworkError, TransportKind};
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

#[derive(Debug)]
struct QueueCapture {
    events: Mutex<VecDeque<Result<InputEvent, InputError>>>,
}

impl QueueCapture {
    fn new(events: Vec<InputEvent>) -> Self {
        Self {
            events: Mutex::new(events.into_iter().map(Ok).collect()),
        }
    }
}

#[async_trait]
impl InputCapture for QueueCapture {
    async fn next_event(&self) -> Result<InputEvent, InputError> {
        self.events
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Err(InputError::Backend("empty capture queue".into())))
    }
}

#[derive(Debug, Default)]
struct RecordingInjector {
    events: Mutex<Vec<InputEvent>>,
}

#[async_trait]
impl InputInjector for RecordingInjector {
    async fn inject(&self, event: InputEvent) -> Result<(), InputError> {
        self.events.lock().unwrap().push(event);
        Ok(())
    }
}

#[derive(Debug, Default)]
struct MemoryConnection {
    sent: Mutex<Vec<Envelope>>,
    recv: Mutex<VecDeque<Envelope>>,
}

impl MemoryConnection {
    fn with_recv(envelopes: Vec<Envelope>) -> Self {
        Self {
            sent: Mutex::new(Vec::new()),
            recv: Mutex::new(envelopes.into()),
        }
    }
}

#[async_trait]
impl Connection for MemoryConnection {
    fn kind(&self) -> TransportKind {
        TransportKind::Tcp
    }

    fn peer_addr(&self) -> SocketAddr {
        "127.0.0.1:47654".parse().unwrap()
    }

    async fn send(&self, envelope: Envelope) -> Result<(), NetworkError> {
        self.sent.lock().unwrap().push(envelope);
        Ok(())
    }

    async fn recv(&self) -> Result<Envelope, NetworkError> {
        self.recv
            .lock()
            .unwrap()
            .pop_front()
            .ok_or(NetworkError::Closed)
    }

    async fn close(&self) -> Result<(), NetworkError> {
        Ok(())
    }
}

#[tokio::test]
async fn forwards_captured_events_to_connection() {
    let capture = QueueCapture::new(vec![
        InputEvent::KeyPress(0x04),
        InputEvent::KeyRelease(0x04),
    ]);
    let connection = Arc::new(MemoryConnection::default());

    forward_n_events(&capture, &*connection, MessageId(10), 2)
        .await
        .unwrap();

    let sent = connection.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].id, MessageId(10));
    assert_eq!(decode_input_event(sent[0].clone()).unwrap(), InputEvent::KeyPress(0x04));
    assert_eq!(sent[1].id, MessageId(11));
    assert_eq!(decode_input_event(sent[1].clone()).unwrap(), InputEvent::KeyRelease(0x04));
}

#[tokio::test]
async fn injects_received_input_envelopes() {
    let injector = RecordingInjector::default();
    let connection = MemoryConnection::with_recv(vec![
        encode_input_event(MessageId(1), InputEvent::ButtonPress(nexkvm_input::MouseButton::Left)),
        encode_input_event(MessageId(2), InputEvent::ButtonRelease(nexkvm_input::MouseButton::Left)),
    ]);

    inject_until_closed(&connection, &injector).await.unwrap();

    assert_eq!(
        injector.events.lock().unwrap().as_slice(),
        &[
            InputEvent::ButtonPress(nexkvm_input::MouseButton::Left),
            InputEvent::ButtonRelease(nexkvm_input::MouseButton::Left),
        ]
    );
}
```

- [ ] **Step 2: Run tests to verify failure**

Run: `cargo test -p nexkvm input_session`

Expected: FAIL because `forward_n_events` and `inject_until_closed` are undefined, and `async-trait` may be missing.

- [ ] **Step 3: Add dependency**

In `apps/desktop/Cargo.toml`, add:

```toml
async-trait.workspace = true
```

- [ ] **Step 4: Implement source and target routing helpers**

Add these imports and functions to `apps/desktop/src/input_session.rs`:

```rust
use nexkvm_input::{InputCapture, InputError, InputInjector};
use nexkvm_network::{Connection, NetworkError};

impl From<NetworkError> for InputSessionError {
    fn from(error: NetworkError) -> Self {
        Self::Codec(error.to_string())
    }
}

impl From<InputError> for InputSessionError {
    fn from(error: InputError) -> Self {
        Self::Codec(error.to_string())
    }
}

pub async fn forward_n_events<C, K>(
    capture: &C,
    connection: &K,
    first_id: MessageId,
    count: usize,
) -> Result<MessageId, InputSessionError>
where
    C: InputCapture + ?Sized,
    K: Connection + ?Sized,
{
    let mut next_id = first_id;
    for _ in 0..count {
        let event = capture.next_event().await?;
        connection.send(encode_input_event(next_id, event)).await?;
        next_id = next_id.next();
    }
    Ok(next_id)
}

pub async fn inject_until_closed<K, I>(
    connection: &K,
    injector: &I,
) -> Result<(), InputSessionError>
where
    K: Connection + ?Sized,
    I: InputInjector + ?Sized,
{
    loop {
        match connection.recv().await {
            Ok(envelope) => {
                if envelope.kind != MessageKind::Input {
                    continue;
                }
                injector.inject(decode_input_event(envelope)?).await?;
            }
            Err(NetworkError::Closed) => return Ok(()),
            Err(error) => return Err(error.into()),
        }
    }
}
```

- [ ] **Step 5: Run desktop tests**

Run: `cargo test -p nexkvm input_session`

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add apps/desktop/Cargo.toml apps/desktop/src/input_session.rs
git commit -m "feat: route input envelopes over peer connections"
```

---

### Task 4: Add macOS Input Permission Report

**Files:**
- Create: `crates/platform/platform-macos/src/permissions.rs`
- Modify: `crates/platform/platform-macos/src/lib.rs`
- Modify: `apps/desktop/src/cli.rs`
- Modify: `apps/desktop/src/main.rs`

- [ ] **Step 1: Write failing macOS permission report tests**

Create `crates/platform/platform-macos/src/permissions.rs`:

```rust
use crate::accessibility::AccessibilityStatus;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MacosPermissionState {
    Ready,
    PermissionRequired,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MacosInputPermissionReport {
    pub accessibility: MacosPermissionState,
    pub can_capture_input: bool,
    pub can_inject_input: bool,
    pub next_step: Option<&'static str>,
}

pub fn input_permission_report(
    _accessibility: &dyn AccessibilityStatus,
) -> MacosInputPermissionReport {
    panic!("red test: implementation intentionally missing")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct StubAccessibility(bool);

    impl AccessibilityStatus for StubAccessibility {
        fn is_trusted(&self) -> bool {
            self.0
        }

        fn prompt_and_check(&self) -> bool {
            self.0
        }
    }

    #[test]
    fn trusted_accessibility_enables_capture_and_injection() {
        let report = input_permission_report(&StubAccessibility(true));

        assert_eq!(report.accessibility, MacosPermissionState::Ready);
        assert!(report.can_capture_input);
        assert!(report.can_inject_input);
        assert_eq!(report.next_step, None);
    }

    #[test]
    fn missing_accessibility_reports_next_step() {
        let report = input_permission_report(&StubAccessibility(false));

        assert_eq!(report.accessibility, MacosPermissionState::PermissionRequired);
        assert!(!report.can_capture_input);
        assert!(!report.can_inject_input);
        assert_eq!(
            report.next_step,
            Some("Grant Accessibility permission in System Settings > Privacy & Security > Accessibility")
        );
    }
}
```

- [ ] **Step 2: Run test to verify failure**

Run on macOS: `cargo test -p nexkvm-platform-macos permissions`

Expected: FAIL because `input_permission_report` panics with `red test: implementation intentionally missing`.

- [ ] **Step 3: Implement permission reporting**

Replace the function:

```rust
pub fn input_permission_report(
    accessibility: &dyn AccessibilityStatus,
) -> MacosInputPermissionReport {
    if accessibility.is_trusted() {
        MacosInputPermissionReport {
            accessibility: MacosPermissionState::Ready,
            can_capture_input: true,
            can_inject_input: true,
            next_step: None,
        }
    } else {
        MacosInputPermissionReport {
            accessibility: MacosPermissionState::PermissionRequired,
            can_capture_input: false,
            can_inject_input: false,
            next_step: Some(
                "Grant Accessibility permission in System Settings > Privacy & Security > Accessibility",
            ),
        }
    }
}
```

- [ ] **Step 4: Export report from macOS backend**

In `crates/platform/platform-macos/src/lib.rs`, add:

```rust
pub mod permissions;
pub use permissions::{MacosInputPermissionReport, MacosPermissionState};
```

Add a method to `impl MacosBackend`:

```rust
#[must_use]
pub fn input_permission_report(&self) -> MacosInputPermissionReport {
    permissions::input_permission_report(self.accessibility.as_ref())
}
```

- [ ] **Step 5: Add doctor formatter test**

In `apps/desktop/src/cli.rs` tests, add a pure formatter test after creating a small formatter:

```rust
#[test]
fn macos_input_report_includes_next_step_when_permission_missing() {
    let rendered = format_macos_input_report(
        "permission-required",
        false,
        false,
        Some("Grant Accessibility permission in System Settings > Privacy & Security > Accessibility"),
    );

    assert!(rendered.contains("macOS input accessibility: permission-required"));
    assert!(rendered.contains("capture ready: false"));
    assert!(rendered.contains("inject ready: false"));
    assert!(rendered.contains("Grant Accessibility permission"));
}
```

- [ ] **Step 6: Implement doctor formatter**

In `apps/desktop/src/cli.rs`, add:

```rust
#[must_use]
pub fn format_macos_input_report(
    accessibility: &str,
    can_capture_input: bool,
    can_inject_input: bool,
    next_step: Option<&str>,
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "macOS input accessibility: {accessibility}");
    let _ = writeln!(out, "  capture ready: {can_capture_input}");
    let _ = writeln!(out, "  inject ready: {can_inject_input}");
    if let Some(next_step) = next_step {
        let _ = writeln!(out, "  next step: {next_step}");
    }
    out.truncate(out.trim_end().len());
    out
}
```

In `apps/desktop/src/main.rs`, inside `doctor()` and under `#[cfg(target_os = "macos")]`, print the real report:

```rust
#[cfg(target_os = "macos")]
{
    let backend = nexkvm_platform_macos::MacosBackend::new();
    let report = backend.input_permission_report();
    let accessibility = match report.accessibility {
        nexkvm_platform_macos::MacosPermissionState::Ready => "ready",
        nexkvm_platform_macos::MacosPermissionState::PermissionRequired => "permission-required",
    };
    for line in cli::format_macos_input_report(
        accessibility,
        report.can_capture_input,
        report.can_inject_input,
        report.next_step,
    )
    .lines()
    {
        println!("  {line}");
    }
}
```

- [ ] **Step 7: Run tests**

Run:

```bash
cargo test -p nexkvm-platform-macos permissions
cargo test -p nexkvm macos_input_report_includes_next_step_when_permission_missing
```

Expected: PASS on macOS.

- [ ] **Step 8: Commit**

```bash
git add crates/platform/platform-macos/src/lib.rs crates/platform/platform-macos/src/permissions.rs apps/desktop/src/cli.rs apps/desktop/src/main.rs
git commit -m "feat: report macOS input permissions"
```

---

### Task 5: Implement macOS Event Translation And Injector Boundary

**Files:**
- Modify: `crates/platform/platform-macos/src/inject.rs`

- [ ] **Step 1: Write failing test for injector permission gate**

Add this test to `crates/platform/platform-macos/src/inject.rs`:

```rust
#[tokio::test]
async fn injector_refuses_without_accessibility_permission() {
    let injector = MacosInputInjector::new(false);
    let result = injector.inject(nexkvm_input::InputEvent::KeyPress(0x04)).await;

    assert!(matches!(result, Err(nexkvm_input::InputError::PermissionDenied)));
}
```

- [ ] **Step 2: Run test to verify failure**

Run: `cargo test -p nexkvm-platform-macos injector_refuses_without_accessibility_permission`

Expected: FAIL because `MacosInputInjector` does not exist.

- [ ] **Step 3: Add the minimal injector boundary**

In `crates/platform/platform-macos/src/inject.rs`, add:

```rust
use async_trait::async_trait;
use nexkvm_input::{InputError, InputEvent, InputInjector};
```

Add the injector type:

```rust
#[derive(Debug, Clone, Copy)]
pub struct MacosInputInjector {
    accessibility_trusted: bool,
}

impl MacosInputInjector {
    #[must_use]
    pub fn new(accessibility_trusted: bool) -> Self {
        Self {
            accessibility_trusted,
        }
    }
}

#[async_trait]
impl InputInjector for MacosInputInjector {
    async fn inject(&self, event: InputEvent) -> Result<(), InputError> {
        if !self.accessibility_trusted {
            return Err(InputError::PermissionDenied);
        }
        let command = event.to_injection_command();
        let _event_plan = plan(command);
        Ok(())
    }
}
```

- [ ] **Step 4: Add passing translation smoke test**

Add:

```rust
#[tokio::test]
async fn injector_accepts_supported_event_when_accessibility_is_ready() {
    let injector = MacosInputInjector::new(true);

    injector
        .inject(nexkvm_input::InputEvent::ButtonPress(MouseButton::Left))
        .await
        .unwrap();
}
```

- [ ] **Step 5: Run platform tests**

Run: `cargo test -p nexkvm-platform-macos inject`

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/platform/platform-macos/src/inject.rs
git commit -m "feat: add macOS input injector boundary"
```

---

### Task 6: Wire Input Sessions Into Peer Connections

**Files:**
- Modify: `apps/desktop/src/connection.rs`
- Modify: `apps/desktop/src/main.rs`
- Modify: `apps/desktop/src/input_session.rs`

- [ ] **Step 1: Write failing task-selection test**

In `apps/desktop/src/input_session.rs`, add outside tests:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputRuntimeRole {
    Disabled,
    Source,
    Target,
    Both,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputRuntimePlan {
    pub start_capture_forwarder: bool,
    pub start_inject_receiver: bool,
}

pub fn plan_runtime(role: InputRuntimeRole, permissions_ready: bool) -> InputRuntimePlan {
    let _ = (role, permissions_ready);
    panic!("red test: implementation intentionally missing")
}
```

Add tests:

```rust
#[test]
fn target_role_starts_receiver_only_when_permissions_are_ready() {
    assert_eq!(
        plan_runtime(InputRuntimeRole::Target, true),
        InputRuntimePlan {
            start_capture_forwarder: false,
            start_inject_receiver: true,
        }
    );
    assert_eq!(
        plan_runtime(InputRuntimeRole::Target, false),
        InputRuntimePlan {
            start_capture_forwarder: false,
            start_inject_receiver: false,
        }
    );
}

#[test]
fn source_role_starts_capture_only_when_permissions_are_ready() {
    assert_eq!(
        plan_runtime(InputRuntimeRole::Source, true),
        InputRuntimePlan {
            start_capture_forwarder: true,
            start_inject_receiver: false,
        }
    );
}
```

- [ ] **Step 2: Run tests to verify failure**

Run: `cargo test -p nexkvm plan_runtime`

Expected: FAIL because `plan_runtime` panics with `red test: implementation intentionally missing`.

- [ ] **Step 3: Implement runtime planning**

Replace `plan_runtime`:

```rust
pub fn plan_runtime(role: InputRuntimeRole, permissions_ready: bool) -> InputRuntimePlan {
    if !permissions_ready {
        return InputRuntimePlan {
            start_capture_forwarder: false,
            start_inject_receiver: false,
        };
    }
    match role {
        InputRuntimeRole::Disabled => InputRuntimePlan {
            start_capture_forwarder: false,
            start_inject_receiver: false,
        },
        InputRuntimeRole::Source => InputRuntimePlan {
            start_capture_forwarder: true,
            start_inject_receiver: false,
        },
        InputRuntimeRole::Target => InputRuntimePlan {
            start_capture_forwarder: false,
            start_inject_receiver: true,
        },
        InputRuntimeRole::Both => InputRuntimePlan {
            start_capture_forwarder: true,
            start_inject_receiver: true,
        },
    }
}
```

- [ ] **Step 4: Convert storage role to runtime role**

In `apps/desktop/src/main.rs`, add a small helper:

```rust
fn input_runtime_role(role: nexkvm_storage::InputControlRole) -> input_session::InputRuntimeRole {
    match role {
        nexkvm_storage::InputControlRole::Disabled => input_session::InputRuntimeRole::Disabled,
        nexkvm_storage::InputControlRole::Source => input_session::InputRuntimeRole::Source,
        nexkvm_storage::InputControlRole::Target => input_session::InputRuntimeRole::Target,
        nexkvm_storage::InputControlRole::Both => input_session::InputRuntimeRole::Both,
    }
}
```

- [ ] **Step 5: Wire accepted connections for target receive path**

Change `connection::spawn_inbound_accept_loop` to accept an optional input-session handler:

```rust
pub type PeerConnectionHandler = Arc<dyn Fn(Box<dyn Connection>) + Send + Sync>;

pub fn spawn_inbound_accept_loop(
    transport: Arc<dyn Transport>,
    handler: Option<PeerConnectionHandler>,
) {
    tokio::spawn(async move {
        loop {
            match transport.accept().await {
                Ok(connection) => {
                    let peer = connection.peer_addr();
                    let kind = connection.kind();
                    info!(%peer, ?kind, "accepted peer connection");
                    if let Some(handler) = &handler {
                        handler(connection);
                    } else {
                        tokio::spawn(async move {
                            hold_connection_until_closed(connection).await;
                        });
                    }
                }
                Err(error) => {
                    warn!(%error, "failed to accept peer connection");
                }
            }
        }
    });
}
```

Update the call in `main.rs` to pass `None` first. Then, once a concrete macOS injector is available in Task 7, replace `None` with a handler that spawns `input_session::inject_until_closed`.

- [ ] **Step 6: Run tests**

Run:

```bash
cargo test -p nexkvm input_session
cargo test -p nexkvm connection
```

Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add apps/desktop/src/input_session.rs apps/desktop/src/connection.rs apps/desktop/src/main.rs
git commit -m "feat: plan input runtime by role and permissions"
```

---

### Task 7: Add macOS Native Capture And Posting

**Files:**
- Create: `crates/platform/platform-macos/src/capture.rs`
- Modify: `crates/platform/platform-macos/src/lib.rs`
- Modify: `crates/platform/platform-macos/src/inject.rs`
- Modify: `apps/desktop/src/main.rs`

- [ ] **Step 1: Write capture permission test**

Create `crates/platform/platform-macos/src/capture.rs`:

```rust
use async_trait::async_trait;
use nexkvm_input::{InputCapture, InputError, InputEvent};

#[derive(Debug, Clone, Copy)]
pub struct MacosInputCapture {
    accessibility_trusted: bool,
}

impl MacosInputCapture {
    #[must_use]
    pub fn new(accessibility_trusted: bool) -> Self {
        Self {
            accessibility_trusted,
        }
    }
}

#[async_trait]
impl InputCapture for MacosInputCapture {
    async fn next_event(&self) -> Result<InputEvent, InputError> {
        let _ = self;
        panic!("red test: implementation intentionally missing")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn capture_refuses_without_accessibility_permission() {
        let capture = MacosInputCapture::new(false);
        let result = capture.next_event().await;

        assert!(matches!(result, Err(InputError::PermissionDenied)));
    }
}
```

- [ ] **Step 2: Run test to verify failure**

Run: `cargo test -p nexkvm-platform-macos capture_refuses_without_accessibility_permission`

Expected: FAIL because `next_event` panics with `red test: implementation intentionally missing`.

- [ ] **Step 3: Implement permission gate**

Replace `next_event`:

```rust
async fn next_event(&self) -> Result<InputEvent, InputError> {
    if !self.accessibility_trusted {
        return Err(InputError::PermissionDenied);
    }
    Err(InputError::Backend(
        "macOS CGEvent tap capture loop is not running".into(),
    ))
}
```

- [ ] **Step 4: Export capture**

In `crates/platform/platform-macos/src/lib.rs`, add:

```rust
pub mod capture;
pub use capture::MacosInputCapture;
pub use inject::MacosInputInjector;
```

- [ ] **Step 5: Add native posting behind injector**

In `crates/platform/platform-macos/src/inject.rs`, add private FFI functions after the testable `plan` layer. The implementation should map:

```rust
fn post_plan(event_plan: CgEventPlan) -> Result<(), InputError> {
    match event_plan {
        CgEventPlan::WarpAbsolute { x, y } => post_mouse_move(x, y),
        CgEventPlan::MoveRelative { dx, dy } => post_relative_move(dx, dy),
        CgEventPlan::MoveRaw { dx, dy } => post_relative_move(dx as f64, dy as f64),
        CgEventPlan::MouseButton { event_type, button } => post_mouse_button(event_type, button),
        CgEventPlan::Scroll { dx, dy } => post_scroll(dx, dy),
        CgEventPlan::Key { event_type, keycode } => post_key(event_type, keycode),
    }
}
```

Change `MacosInputInjector::inject` so the trusted branch calls:

```rust
post_plan(plan(event.to_injection_command()))
```

Keep HID-to-CGKeyCode mapping small and explicit for the MVP:

```rust
fn hid_to_cg_keycode(keycode: u32) -> Option<u16> {
    match keycode {
        0x04 => Some(0),  // A
        0x05 => Some(11), // B
        0x06 => Some(8),  // C
        0x07 => Some(2),  // D
        0x08 => Some(14), // E
        0x09 => Some(3),  // F
        0x0A => Some(5),  // G
        0x0B => Some(4),  // H
        0x0C => Some(34), // I
        0x0D => Some(38), // J
        0x0E => Some(40), // K
        0x0F => Some(37), // L
        0x10 => Some(46), // M
        0x11 => Some(45), // N
        0x12 => Some(31), // O
        0x13 => Some(35), // P
        0x14 => Some(12), // Q
        0x15 => Some(15), // R
        0x16 => Some(1),  // S
        0x17 => Some(17), // T
        0x18 => Some(32), // U
        0x19 => Some(9),  // V
        0x1A => Some(13), // W
        0x1B => Some(7),  // X
        0x1C => Some(16), // Y
        0x1D => Some(6),  // Z
        0x29 => Some(53), // Escape
        0x2C => Some(49), // Space
        _ => None,
    }
}
```

If a key is not in this map, return:

```rust
Err(InputError::Backend(format!("unsupported macOS HID keycode: {keycode}")))
```

- [ ] **Step 6: Wire runtime with macOS source/target constructors**

In `apps/desktop/src/main.rs`, compute:

```rust
let role = input_runtime_role(config.input.control_role);
```

On macOS, compute permission readiness:

```rust
#[cfg(target_os = "macos")]
let input_permissions_ready = {
    let macos = nexkvm_platform_macos::MacosBackend::new();
    let report = macos.input_permission_report();
    report.can_capture_input && report.can_inject_input
};

#[cfg(not(target_os = "macos"))]
let input_permissions_ready = false;
```

Use `input_session::plan_runtime(role, input_permissions_ready)` to decide which tasks to start.

- [ ] **Step 7: Run tests**

Run:

```bash
cargo test -p nexkvm-platform-macos
cargo test -p nexkvm input_session
```

Expected: PASS on macOS. On non-macOS, `nexkvm-platform-macos` is cfg-gated and should not be selected by cross-platform CI.

- [ ] **Step 8: Manual smoke checks**

Run on the source Mac after granting Accessibility:

```bash
cargo run -p nexkvm -- doctor
cargo run -p nexkvm -- --debug
```

Expected: `doctor` reports macOS input capture/inject ready. Daemon does not start capture if permission is missing.

- [ ] **Step 9: Commit**

```bash
git add crates/platform/platform-macos/src/capture.rs crates/platform/platform-macos/src/lib.rs crates/platform/platform-macos/src/inject.rs apps/desktop/src/main.rs
git commit -m "feat: add macOS native input runtime boundaries"
```

---

### Task 8: Harden macOS Packaging And Gatekeeper Validation

**Files:**
- Create: `packaging/macos/nexkvm.entitlements`
- Modify: `packaging/macos/Info.plist`
- Modify: `scripts/package-macos.sh`
- Create: `docs/smoke/macos-kvm-mvp.md`
- Modify: `docs/features.md`

- [ ] **Step 1: Write packaging validation shell checks first**

Create `docs/smoke/macos-kvm-mvp.md`:

````markdown
# macOS KVM MVP Smoke Checks

## Permission Smoke

Run:

```sh
cargo run -p nexkvm -- doctor
```

Expected after Accessibility is granted:

- `macOS input accessibility: ready`
- `capture ready: true`
- `inject ready: true`

Expected before Accessibility is granted:

- `macOS input accessibility: permission-required`
- `capture ready: false`
- `inject ready: false`

## Release Signing Smoke

Run:

```sh
: "${APPLE_CODESIGN_IDENTITY:?set Developer ID Application identity from security find-identity}"
: "${APPLE_NOTARY_PROFILE:?set notarytool keychain profile}"
NEXKVM_RELEASE=1 ./scripts/package-macos.sh
```

Then validate:

```sh
codesign -dvvv --entitlements :- target/package/nexkvm.app
xcrun stapler validate target/package/nexkvm.app
spctl -a -vv target/package/nexkvm.app
```

Expected:

- `codesign` shows Developer ID signing and hardened runtime.
- `stapler validate` succeeds.
- `spctl` reports accepted source for the app bundle.
````

- [ ] **Step 2: Add entitlements file**

Create `packaging/macos/nexkvm.entitlements`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
</dict>
</plist>
```

For this MVP, keep the entitlement set empty unless a concrete API requires one. Accessibility and Input Monitoring are privacy permissions, not entitlements.

- [ ] **Step 3: Add Info.plist privacy usage strings**

In `packaging/macos/Info.plist`, add before `</dict>`:

```xml
  <key>NSInputMonitoringUsageDescription</key>
  <string>NexKVM needs Input Monitoring to capture keyboard and mouse input for sharing with your trusted devices.</string>
  <key>NSAppleEventsUsageDescription</key>
  <string>NexKVM may use Apple Events only for trusted local automation integrations.</string>
```

- [ ] **Step 4: Require release signing inputs**

In `scripts/package-macos.sh`, after variable declarations, add:

```bash
ENTITLEMENTS="$ROOT/packaging/macos/nexkvm.entitlements"
RELEASE="${NEXKVM_RELEASE:-0}"

if [[ "$RELEASE" == "1" ]]; then
  if [[ -z "${APPLE_CODESIGN_IDENTITY:-}" ]]; then
    echo "NEXKVM_RELEASE=1 requires APPLE_CODESIGN_IDENTITY"
    exit 1
  fi
  if [[ -z "${APPLE_NOTARY_PROFILE:-}" ]]; then
    echo "NEXKVM_RELEASE=1 requires APPLE_NOTARY_PROFILE"
    exit 1
  fi
fi
```

- [ ] **Step 5: Sign with entitlements**

Replace the `codesign` invocation with:

```bash
codesign --force --deep --timestamp --options runtime --entitlements "$ENTITLEMENTS" --sign "$APPLE_CODESIGN_IDENTITY" "$APP_DIR"
```

- [ ] **Step 6: Validate release artifacts**

After stapling and rebuilding the archive, add:

```bash
if [[ "$RELEASE" == "1" ]]; then
  echo "Validating signed and notarized app..."
  codesign --verify --deep --strict --verbose=2 "$APP_DIR"
  codesign -dvvv --entitlements :- "$APP_DIR"
  xcrun stapler validate "$APP_DIR"
  spctl -a -vv "$APP_DIR"
fi
```

- [ ] **Step 7: Run packaging script negative check**

Run:

```bash
NEXKVM_RELEASE=1 ./scripts/package-macos.sh
```

Expected: FAIL quickly with `NEXKVM_RELEASE=1 requires APPLE_CODESIGN_IDENTITY`.

- [ ] **Step 8: Run signed release packaging**

Run:

```bash
: "${APPLE_CODESIGN_IDENTITY:?set Developer ID Application identity from security find-identity}"
: "${APPLE_NOTARY_PROFILE:?set notarytool keychain profile}"
NEXKVM_RELEASE=1 ./scripts/package-macos.sh
```

Expected: PASS after Apple notarization completes. `spctl -a -vv target/package/nexkvm.app` reports accepted.

- [ ] **Step 9: Update feature tracker**

In `docs/features.md`, move or mark only these implemented pieces once their tasks pass:

```markdown
- [x] macOS input permission diagnostics for keyboard and mouse sharing.
- [x] macOS input sharing runtime boundaries for source/target roles.
- [x] macOS release packaging validation for Developer ID signing, notarization, stapling, and Gatekeeper acceptance.
```

Do not mark Linux/Windows native input sharing, clipboard sync, or file transfer complete in this task.

- [ ] **Step 10: Commit**

```bash
git add packaging/macos/nexkvm.entitlements packaging/macos/Info.plist scripts/package-macos.sh docs/smoke/macos-kvm-mvp.md docs/features.md
git commit -m "build: harden macOS release signing validation"
```

---

## Final Verification

- [ ] Run formatting:

```bash
cargo fmt --all -- --check
```

Expected: PASS.

- [ ] Run focused Rust tests:

```bash
cargo test -p nexkvm-storage
cargo test -p nexkvm
cargo test -p nexkvm-platform-macos
```

Expected: PASS on macOS.

- [ ] Run focused clippy:

```bash
cargo clippy -p nexkvm-storage -p nexkvm -p nexkvm-platform-macos --all-targets -- -D warnings
```

Expected: PASS on macOS.

- [ ] Run macOS permission smoke:

```bash
cargo run -p nexkvm -- doctor
```

Expected: Reports Accessibility readiness accurately.

- [ ] Run Gatekeeper validation for a signed release archive:

```bash
spctl -a -vv target/package/nexkvm.app
```

Expected: Accepted Developer ID/notarized app. If this fails, do not claim the app avoids Gatekeeper warnings.
