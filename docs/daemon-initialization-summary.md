# Desktop Daemon Runtime Initialization & Subsystem Wiring

## Overview
The NexKVM desktop daemon initializes in **8 sequential phases**, establishing an async task-based architecture where subsystems communicate through an in-process **EventBus** and **channel-based handlers**. Platform backends are instantiated early to gate capability-dependent features.

---

## 1) Platform Backend Instantiation & Capability Resolution

### Location
- **Instantiation**: [apps/desktop/src/main.rs](apps/desktop/src/main.rs#L1486) – `platform_backend()` (lines 1486-1503)
- **Capability check**: [apps/desktop/src/main.rs](apps/desktop/src/main.rs#L83) (line 83 onward)
- **Platform traits**: [crates/core/src/platform.rs](crates/core/src/platform.rs#L1)

### Pattern
```rust
// Platform selection by compile-time OS guard
#[cfg(target_os = "macos")]
Some(Box::new(nexkvm_platform_macos::MacosBackend::new()))
#[cfg(target_os = "windows")]
Some(Box::new(nexkvm_platform_windows::WindowsBackend::new()))
#[cfg(target_os = "linux")]
Some(Box::new(nexkvm_platform_linux::LinuxBackend::new()))
#[cfg(not(...))] 
None  // headless mode
```

### Capabilities Queried
The backend is interrogated immediately (line 83+) for:
- `can_inject_input` – synthetic input synthesis
- `can_capture_input` – global input event capture
- `can_access_clipboard` – system pasteboard read/write
- `permission_pending` – OS permission dialog still needed

These **gate subsystem initialization** (e.g., input capture only starts if `can_capture_input && !permission_pending`).

---

## 2) How Subsystems Are Spawned as Tasks

### Pattern: Per-Connection Handler Closure

Each subsystem that interacts with **peers** uses a **`PeerConnectionHandler`** – a cloneable `Arc<dyn Fn(Box<dyn Connection>)>` – instantiated once at daemon startup and invoked per accepted/established peer connection.

### Location
- **Handler type definition**: [apps/desktop/src/connection.rs](apps/desktop/src/connection.rs#L15) (line 15)
- **Input handler construction**: [apps/desktop/src/main.rs](apps/desktop/src/main.rs#L246) – `input_peer_handler()` function
- **Handler invocation on inbound**: [apps/desktop/src/connection.rs](apps/desktop/src/connection.rs#L96) – `spawn_inbound_accept_loop()` (line 96+)
- **Handler invocation on outbound**: [apps/desktop/src/connection.rs](apps/desktop/src/connection.rs#L189) – `spawn_reconnect_driver()` (line 189+)

### Input Subsystem Spawn Example
```rust
// 1. Build platform-specific capture/inject objects (not yet running tasks)
let capture = Some(nexkvm_platform_macos::MacosInputCapture::new(capture_ready));
let injector = Some(nexkvm_platform_macos::MacosInputInjector::new(inject_ready));

// 2. Return a handler closure that captures these references
let handler: PeerConnectionHandler = Arc::new(move |connection| {
    let connection: Arc<dyn Connection> = Arc::from(connection);
    
    // 3. When peer connects, spawn **per-connection tasks**
    if let Some(capture) = capture.clone() {
        tokio::spawn(async move {
            input_session::forward_extended_until_error(
                &capture,
                &*connection,
                MessageId(0),
                handoff_edge,
                emergency_stop_keycode,
                remote_focus_timeout_millis,
                move |suppressed| capture.set_suppressed(suppressed),
            ).await
        });
    }
    if let Some(injector) = injector.clone() {
        tokio::spawn(async move {
            input_session::inject_until_closed(&*connection, &injector).await
        });
    }
});
```

**Key insight**: 
- Platform backends are **created once** (lines 215-231)
- The handler is **created once** with those backends captured
- Per-peer **tasks spawn inside the handler** when the connection arrives

### All Subsystems Using This Pattern
1. **Input**: [apps/desktop/src/main.rs](apps/desktop/src/main.rs#L215) – capture + injection per peer
2. **Network inbound accept**: [apps/desktop/src/connection.rs](apps/desktop/src/connection.rs#L96) – spawns per-connection secure handshake + handler invocation
3. **Explicit reconnect driver**: [apps/desktop/src/connection.rs](apps/desktop/src/connection.rs#L140) – spawns task polling for one configured peer
4. **Discovery reconnect driver**: [apps/desktop/src/connection.rs](apps/desktop/src/connection.rs#L189) – spawns task consuming rediscovery targets from the UDP discovery service

---

## 3) Event Bus & Channel Coordination

### EventBus (Intra-Daemon Pub/Sub)
**Location**: [crates/core/src/event.rs](crates/core/src/event.rs#L1)

**Purpose**: Typed, lossy broadcast for **real-time signals** (pointer motion, key events, device discovery) using `tokio::sync::broadcast`.

**Instantiation**: [apps/desktop/src/main.rs](apps/desktop/src/main.rs#L89) (line 89)
```rust
let bus = EventBus::new();  // capacity: 1024 events retained
```

**Event Kinds**:
```rust
pub enum Event {
    DeviceDiscovered(DeviceInfo),
    DeviceConnected(DeviceId),
    DeviceDisconnected(DeviceId),
    Inbound { from: DeviceId, kind: MessageKind, payload: Bytes },
    Outbound { to: Option<DeviceId>, kind: MessageKind, payload: Bytes },
    Notification(CrossDeviceNotification),
    QuickCommandInvoked(CommandId),
    Shutdown,
}
```

**Current Usage in Daemon**:
- **Published on shutdown**: [apps/desktop/src/main.rs](apps/desktop/src/main.rs#L224) (line 224)
  ```rust
  bus.publish(nexkvm_core::Event::Shutdown);
  ```
- **Not actively used by subsystems yet** (wiring is in place but unused)

### Channels (Dedicated Streams)
Per-peer `Connection` is passed to handlers, which read/write directly to that connection. This bypasses the event bus for low-latency bilateral messaging.

**Discovery service** also has its own channel:
```rust
let mut targets = driver.start(&info, listen_addr, Some(&local_fingerprint)).await?;
// targets is an mpsc::Receiver<ReconnectTarget> streaming trusted peer rediscovery
```

---

## 4) Clipboard Engine Integration Point

### Current State
Clipboard is **configured but not integrated** into the daemon runtime:
- Config flag exists: [apps/desktop/src/main.rs](apps/desktop/src/main.rs#L707) (`config.features.clipboard`)
- No instantiation in `run_daemon()`
- No per-peer clipboard task spawned

### Where It Would Fit

**Option A: Per-Connection Clipboard Task (Recommended)**

Follow the **input subsystem pattern**. In `input_peer_handler()` or a sibling `clipboard_peer_handler()`:

```rust
// At daemon startup (before peers connect):
let local_device_id = device.id;
let clipboard_backend = match nexkvm_platform_macos::MacosClipboard::new() {
    Ok(c) => c,
    Err(e) => { warn!("clipboard unavailable: {}", e); return None; }
};
let sync = ClipboardSync::new();  // conflict resolver, dedup
let history = ClipboardHistory::new(HistoryConfig::default());

// Wrap platform + sync in an engine per peer
let engine = ClipboardEngine::new(
    local_device_id,
    clipboard_backend,
    sync,
    history
);

// Return a handler that spawns clipboard poll/apply tasks
let handler: PeerConnectionHandler = Arc::new(move |connection| {
    let mut engine = engine.clone();
    let connection = Arc::from(connection);
    
    tokio::spawn(async move {
        loop {
            // Poll local clipboard; if changed, send to peer
            if let Ok(Some(update)) = engine.poll_local(now_millis()).await {
                // Encode and send on MessageKind::Clipboard
                connection.send(clipboard_update_to_envelope(update)).await.ok();
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    });
    
    // Receive remote clipboard updates
    let engine = engine.clone();
    let connection = connection.clone();
    tokio::spawn(async move {
        while let Ok(envelope) = connection.recv().await {
            if envelope.kind == MessageKind::Clipboard {
                if let Ok(update) = decode_clipboard_update(&envelope) {
                    engine.apply_remote(update, now_millis()).await.ok();
                }
            }
        }
    });
});
```

**Location to add**: Right after input handler construction, around [apps/desktop/src/main.rs](apps/desktop/src/main.rs#L246).

**Integration with EventBus (future)**:
- Publish `Event::Outbound { kind: MessageKind::Clipboard, payload }` when local clipboard changes
- Subscribe and forward to peer connections
- This allows future UI/plugins to observe clipboard changes via the event bus

**Issues to Resolve**:
- Platform backends (`MacosClipboard`, `WindowsClipboard`, `LinuxClipboard`) must be instantiated and wrapped in the `Clipboard` trait
- Per-device clipboard isolation (each peer connection might sync different content)
- Conflict resolution: `ClipboardSync` and `ConflictResolver` handle concurrent writes; verify `OriginStamp` is trusted

---

## Runtime Sequence Diagram

```
startup
  ├─ 1. Config load
  ├─ 2. Telemetry init
  ├─ 3. Device identity
  ├─ 4. EventBus new()
  ├─ 5. Platform backend instantiate (OS check)
  │  ├─ Query capabilities (input/clipboard/permissions)
  │  └─ Log capabilities
  ├─ 6. Input subsystem setup
  │  ├─ Plan runtime (Source/Target/Both roles)
  │  ├─ Create platform input capture/injector (not spawned yet)
  │  └─ Build PeerConnectionHandler (closure capturing platforms)
  ├─ 7. Network transport bind + spawn_inbound_accept_loop()
  │  └─ Loop: transport.accept() → secure_connection() → handler(peer)
  │     └─ (handler spawns per-peer capture/inject tasks here)
  ├─ 8. Explicit peer connect (if configured)
  │  └─ spawn_explicit_connect_driver() → handler(peer)
  ├─ 9. LAN discovery start
  │  ├─ UdpDiscovery bind + DiscoveryService new
  │  └─ spawn async task advertising + spawn_reconnect_driver()
  │     └─ reconnect_driver polls targets from discovery
  │        → connect_reconnect_target() → handler(peer)
  └─ 10. Wait for Ctrl-C
       └─ bus.publish(Event::Shutdown)
       └─ Exit

Per-peer lifecycle:
  peer connects → handler(connection)
    ├─ tokio::spawn(input_session::forward_...)    [if capture enabled]
    ├─ tokio::spawn(input_session::inject_...)     [if inject enabled]
    └─ [future] tokio::spawn(clipboard_engine::poll_and_sync)
```

---

## Key Files & Patterns Summary

| Concern | File | Lines | Pattern |
|---------|------|-------|---------|
| **Foundation** | [apps/desktop/src/main.rs](apps/desktop/src/main.rs#L25) | 25–230 | 8-phase `run_daemon()` |
| **Platform backends** | [apps/desktop/src/main.rs](apps/desktop/src/main.rs#L1486) | 1486–1503 | Conditional compilation by OS |
| **Capabilities** | [crates/core/src/platform.rs](crates/core/src/platform.rs#L29) | 29–47 | `PlatformCapabilities` queried early |
| **EventBus** | [crates/core/src/event.rs](crates/core/src/event.rs#L1) | 1–145 | Broadcast pub/sub; startup published once |
| **Input handler** | [apps/desktop/src/main.rs](apps/desktop/src/main.rs#L246) | 246–335 | Per-connection closure spawning tasks |
| **Inbound loop** | [apps/desktop/src/connection.rs](apps/desktop/src/connection.rs#L96) | 96–135 | Accepts + invokes handler per peer |
| **Explicit connect** | [apps/desktop/src/connection.rs](apps/desktop/src/connection.rs#L140) | 140–187 | Retries single configured endpoint |
| **Rediscover loop** | [apps/desktop/src/connection.rs](apps/desktop/src/connection.rs#L189) | 189–230 | Consumes discovery targets stream |
| **Discovery service** | [apps/desktop/src/main.rs](apps/desktop/src/main.rs#L331) | 331–384 | UDP advertise + trusted reconnect |
| **Clipboard engine** | [crates/clipboard/src/engine.rs](crates/clipboard/src/engine.rs#L1) | 1–120 | `poll_local()` + `apply_remote()` per peer |

---

## Implications for Clipboard Integration

1. **No global daemon-wide clipboard state needed** — each peer connection independently polls/syncs
2. **Platform backends are shared** across peers (one `MacosInputCapture` handles all peer events; same pattern for clipboard)
3. **EventBus is passive** — currently not actively used by subsystems; clipboard could publish to it for future UI/plugin observation
4. **Graceful degradation** — if platform backend denies clipboard access, the handler returns `None` and feature is disabled
5. **Safety isolation** — each peer's `Connection` is independent; clipboard writes are synchronized to all peers, but conflict resolution (`ConflictResolver`) prevents divergence
