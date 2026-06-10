# Clipboard Interoperability

Informs: `crates/clipboard` (`Clipboard`, `ClipboardContent`) and the
`platform-*` backends.

`ClipboardContent { mime: String, data: Bytes }` is already the right shape:
**MIME is the canonical interchange type**. The research problem is mapping each
OS's native format model onto MIME, and handling large/lazy payloads without
flooding the event bus.

---

## Native format models

| OS | API | Format identifiers | Example text / image / files |
|----|-----|--------------------|------------------------------|
| Windows | Clipboard API (`OpenClipboard`, `Get/SetClipboardData`) | numeric `CF_*` + registered formats | `CF_UNICODETEXT` · `CF_DIB`/`CF_DIBV5` (PNG via registered "PNG") · `CF_HDROP` |
| macOS | `NSPasteboard` | UTIs (reverse-DNS) | `public.utf8-plain-text` · `public.png` · `public.file-url` |
| Linux (X11) | selections (`CLIPBOARD`) | MIME targets | `text/plain;charset=utf-8` · `image/png` · `text/uri-list` |
| Linux (Wayland) | `wlr-data-control` / portal | MIME targets | same as X11 |

### Canonical mapping (to/from MIME)
- Text → `text/plain;charset=utf-8` (always UTF-8 on the wire; convert
  `CF_UNICODETEXT` UTF-16 ↔ UTF-8 at the Windows boundary).
- Image → prefer `image/png` as the lossless interchange; convert `CF_DIB`↔PNG
  on Windows.
- Files → `text/uri-list` canonical; `CF_HDROP`↔uri-list on Windows,
  `public.file-url`↔uri-list on macOS. **File contents are not clipboard data** —
  a uri-list triggers the file-transfer/streaming path (a core drag-and-drop
  differentiator), not an inline blob.
- Rich text/HTML → `text/html` with a `text/plain` fallback offered alongside.

---

## Large payloads & lazy rendering

Putting images/files on the lossy broadcast event bus is wrong. Two rules:

1. **Clipboard *metadata*/small text** may flow as a normal message; **large
   blobs go over a reliable `streaming` channel**.
2. **Lazy / delayed rendering** — don't transfer the payload on every copy;
   transfer only when the *peer actually pastes*. All three OSes support this:
   - Windows: **delayed rendering** (`SetClipboardData(fmt, NULL)` + respond to
     `WM_RENDERFORMAT`).
   - macOS: lazy `NSPasteboard` via a `pasteboard:provideDataForType:` provider.
   - Wayland/X11: data offers are inherently pull-based — the source streams data
     to the fd only when a target requests it.

   nexkvm mirrors this across devices: copying advertises *available types +
   size*; the receiving device requests bytes on paste. This avoids syncing
   multi-MB images that are never pasted.

---

## Sync loop & hazards

- **Echo/loop prevention**: writing a received clipboard fires the local change
  notification → must tag/suppress self-originated writes (content hash or an
  origin marker) to avoid ping-pong between devices.
- **Change detection**:
  - Windows: `AddClipboardFormatListener` → `WM_CLIPBOARDUPDATE`.
  - macOS: poll `NSPasteboard.changeCount` (no native event; low-rate poll on a
    dedicated thread).
  - Wayland: `wlr-data-control` offers events; X11: `XFixes` selection notify.
- **Sensitive content**: respect "concealed/transient" hints
  (macOS `org.nspasteboard.ConcealedType`, Windows `ExcludeClipboardContentFromMonitorProcessing`)
  — do **not** sync password-manager clips. Honor a config opt-out.
- **Threading**: change listeners run on OS message loops/pollers → bridge via
  bounded `mpsc`, consistent with the input backends.

---

## Recommended crates

- Baseline cross-platform: **`arboard`** (text + image, all three OSes). Good for
  the MVP text/image path.
- Advanced (file lists, delayed rendering, concealed hints) likely needs
  platform-specific code: `clipboard-win`, `wl-clipboard-rs`, `x11-clipboard` /
  `x11rb` + `XFixes`, and `objc2-app-kit` for `NSPasteboard` providers.

### Phasing
- **MVP**: text sync via `arboard` + echo suppression + change detection.
- **Phase 2**: images (`image/png`), lazy rendering.
- **Phase 3**: file lists → drag-and-drop file transfer over `streaming`.
