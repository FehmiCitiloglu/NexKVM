# Input Capture & Injection

Informs: `crates/input` (`InputCapture`, `InputInjector`, `InputEvent`) and the
`platform-*` backends (`PlatformBackend`, `PlatformCapabilities`).

coklu needs two capabilities per platform:

- **Capture** — observe local pointer/keyboard events, and on the *active edge*
  **suppress** them locally while the user is driving a remote device
  (Synergy/Universal Control style).
- **Injection** — synthesize remote events into the local OS.

The hard part everywhere is **suppression** (consuming the local event) and
**absolute positioning** across heterogeneous multi-monitor layouts.

---

## Windows

### Capture
- **Low-level hooks**: `SetWindowsHookEx(WH_MOUSE_LL / WH_KEYBOARD_LL)`. The hook
  callback can **return non-zero to swallow** an event — this is how we suppress
  local input while controlling a remote. Requires a thread with a running
  message pump (`GetMessage` loop).
- **Raw Input** (`RegisterRawInputDevices` + `WM_INPUT`): device-level deltas,
  good for high-rate/gaming pointer data, but **cannot suppress** events. Use it
  as a complement for raw deltas, not for edge capture.
- Multi-monitor virtual desktop bounds: `GetSystemMetrics(SM_XVIRTUALSCREEN, …)`.

### Injection
- **`SendInput`** (use this; `mouse_event`/`keybd_event` are deprecated).
- Absolute pointer: `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK`, coordinates
  normalized to `0..=65535` over the virtual desktop — maps cleanly from our
  normalized `f64`.
- **UIPI constraint**: a process cannot `SendInput` into a window of higher
  integrity level. Injection into elevated/admin windows requires the coklu
  process to run at matching integrity (or with `uiAccess`, which needs signing +
  install in a trusted location). Surface this via `can_inject_input`.

### Crate / threading
- `windows` (official Microsoft bindings).
- Hook callback runs on the message-pump thread → forward events over a bounded
  `mpsc` channel into the async world. Never run the pump on a Tokio worker.

---

## macOS

### Capture
- **`CGEventTap`** (`CGEventTapCreate`) at `kCGSessionEventTap` (or
  `kCGHIDEventTap` for earliest interception). Returning `NULL` from the tap
  callback **suppresses** the event. The tap is driven by a `CFRunLoop`.
- **Permission**: Accessibility (`AXIsProcessTrustedWithOptions` with the prompt
  option). This is the prompt `request_permissions()` must trigger on macOS.
- **Secure input**: when a password field calls `EnableSecureEventInput`, taps
  stop seeing keystrokes — by design; cannot be bypassed (and should not be).

### Injection
- `CGEventCreateMouseEvent` / `CGEventCreateKeyboardEvent` + `CGEventPost`.
- Absolute cursor placement: `CGWarpMouseCursorPosition` (note: warp can
  desync OS cursor association; pair with a posted move event).
- Multi-display global coordinates via `CGDisplay` bounds.

### Crate / threading
- `core-graphics` + `core-foundation` (or `objc2-*`). Run the `CFRunLoop` on a
  dedicated thread; bridge via `mpsc`. Screen Recording permission is separate
  and only needed if/when we capture display content (later phase).

---

## Linux — evdev / uinput (kernel level)

### Capture
- Read `/dev/input/event*` via **evdev**. `EVIOCGRAB` ioctl grabs a device
  **exclusively**, which is how local suppression works at the kernel level.
- Permission: read access to the device nodes → user in the `input` group, or a
  udev rule. No GUI prompt; document setup.

### Injection
- **`/dev/uinput`**: create a virtual keyboard/pointer device and emit events.
  Needs write access to `/dev/uinput` (udev rule).

### Crate / threading
- `evdev` (has Tokio support via `AsyncFd`) and `uinput` / `input-linux`.
- Because the fd is pollable, capture can integrate with Tokio via `AsyncFd`
  instead of a dedicated thread — preferred where possible.

### The catch
evdev/uinput operate **below** the display server, so they work under both X11
and Wayland — **but** they have **no concept of cursor position or windows**.
Absolute pointer warp and per-window targeting are not available at this layer.
For a KVM-style absolute-position experience under Wayland this is insufficient
on its own (see below).

---

## Wayland (the hard platform)

By design Wayland **forbids global input capture/injection** for security — there
is no equivalent of `XTEST`. Sanctioned paths:

- **`libei` / `libeis`** (Emulated Input) — the modern protocol for input
  emulation, brokered by the compositor.
- **`xdg-desktop-portal`**:
  - `RemoteDesktop` portal → inject pointer/keyboard (backed by libei).
  - `InputCapture` portal (newer) → **edge-based capture** for KVM use cases.
    This is the correct, supported route for coklu's follow-mouse model.
- All require a portal interaction/grant; support **varies by compositor**
  (GNOME/Mutter, KDE/KWin, wlroots differ in maturity).

### X11 fallback
Where an X11 session is present (or XWayland for some cases): `XTEST` for
injection, `XInput2` / `XRecord` for capture — permissive, easy, legacy. Detect
session type (`XDG_SESSION_TYPE`, `WAYLAND_DISPLAY`) and prefer native Wayland
portals, falling back to X11.

### Capability resolution for `LinuxBackend`
Resolve at runtime, in priority order:
1. Wayland + InputCapture/RemoteDesktop portal available → full caps (after grant).
2. X11 session → full caps via XTEST/XInput2.
3. evdev/uinput accessible → capture/inject without cursor warp (degraded).
4. None → `can_*_input = false`, `permission_pending` as appropriate.

---

## Cross-platform design decisions

- **Keycodes**: `InputEvent::Key{Press,Release}(u32)` currently carries an
  OS-neutral code. **Open item**: standardize on a single keymap. Recommend
  **USB HID usage codes** (or Linux evdev codes) as the canonical wire
  representation, with per-platform translation tables. Track as a follow-up
  before input implementation lands.
- **Coordinates**: keep normalized `f64` `[0,1]` on the wire (already chosen);
  each injector denormalizes against its local virtual-desktop bounds.
- **Threading rule**: hooks/taps/run-loops live on dedicated OS threads; only
  evdev (`AsyncFd`) integrates directly with Tokio. All feed a bounded `mpsc`.
- **Event batching**: coalesce high-rate `PointerMove` events (drop intermediate
  moves under backpressure — freshest wins) to protect latency; deliver
  key/button events losslessly.

## Recommended crates

| Platform | Capture | Injection |
|----------|---------|-----------|
| Windows | `windows` (LL hooks + Raw Input) | `windows` (`SendInput`) |
| macOS | `core-graphics` (`CGEventTap`) | `core-graphics` (`CGEventPost`) |
| Linux/X11 | `x11rb` (XInput2/XRecord) | `x11rb` (XTEST) |
| Linux/Wayland | portal `InputCapture` (libei) | portal `RemoteDesktop` (libei) |
| Linux/kernel | `evdev` (`AsyncFd`, `EVIOCGRAB`) | `uinput` / `input-linux` |

`input-event-codes` / a HID table crate for the canonical keymap.
