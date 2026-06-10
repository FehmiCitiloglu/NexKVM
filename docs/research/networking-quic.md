# QUIC Performance Tuning

Informs: `crates/network` (`Transport`, `Connection`, the `transport-quic`
feature). QUIC is the **preferred** transport (priority 0 in `TransportKind`),
with TCP fallback and WebRTC for remote mode.

**Recommended stack**: **`quinn`** (Tokio-native, `rustls`-based) over `quiche`.
Reasons: idiomatic async API, integrates with our runtime, pluggable congestion
control, datagram support, active maintenance.

---

## Why QUIC for nexkvm

- **Stream multiplexing without head-of-line blocking** — separate logical lanes
  (input, clipboard, file, audio) on independent QUIC streams; a stalled file
  transfer never delays an input event.
- **Unreliable datagrams** for real-time input — `send_datagram` skips
  retransmission/ordering: freshest pointer event wins, matching the lossy
  semantics of our event bus.
- **TLS 1.3 built in** — encryption + the outer auth channel for free; nexkvm
  binds it to device identity at the `crypto` layer (cert = device key).
- **Connection migration** — survives the device roaming between Wi-Fi/Ethernet
  without a full reconnect (laptop moving networks).
- **0-RTT reconnect** — fast resume for already-paired devices.

---

## Channel → QoS mapping

| Channel | QUIC primitive | Rationale |
|---------|----------------|-----------|
| Input (pointer/key) | **datagram** (unreliable) | latency over reliability; drop stale moves |
| Clipboard (small) | reliable stream | must arrive intact |
| File transfer | dedicated reliable stream | bulk; isolated to avoid HOL |
| Audio frames | reliable stream or jitter-buffered datagram | continuous, glitch-sensitive |
| Heartbeat/control | reliable stream | small, must arrive |

Map this onto the `Connection` trait: the QUIC impl routes `Envelope`s by
`MessageKind` to the appropriate stream/datagram internally, keeping callers
transport-agnostic.

---

## Tuning knobs (quinn `TransportConfig` / endpoint)

### Latency (input path)
- **Datagrams**: enable via `datagram_receive_buffer_size` /
  `datagram_send_buffer_size`; size to a few frames — small buffers prevent
  stale-event buildup.
- **Pacing / congestion control**: pluggable CC. On low-RTT LANs **BBR** (or a
  tuned CUBIC) avoids the bufferbloat latency spikes of loss-based CC under load.
- **`max_idle_timeout`** + **`keep_alive_interval`**: keep sessions warm for
  instant cursor handoff; set keepalive well below idle timeout.
- **ACK frequency**: reduce ACK overhead on chatty input streams where supported.

### Throughput (file/audio path)
- **Flow-control windows**: raise `stream_receive_window` and `receive_window`
  (connection-wide) so large transfers aren't window-limited on higher-BDP
  links. Defaults are conservative.
- **GSO/GRO**: quinn uses UDP generic segmentation/receive offload where the OS
  supports it — major throughput win; verify it's active per platform.
- **UDP socket buffers**: increase OS send/recv buffer sizes; small kernel
  buffers cap throughput and cause drops under burst.
- **MTU discovery**: enable QUIC PMTUD / set a sensible initial max UDP payload;
  larger packets = less per-packet overhead. Mind LAN jumbo-frame cases.

### Connection setup
- **0-RTT**: enable for fast reconnect of trusted devices, **but** 0-RTT data is
  **replayable** — restrict it to idempotent traffic. The **pairing/auth
  handshake must never use 0-RTT** (replay-attack boundary; consistent with the
  `crypto` replay-protection model).
- **Certificates**: self-signed cert keyed by the device identity key; peer
  verification is pinned via the `TrustStore` (TOFU-then-pinned), **not** a
  public CA.

---

## Fallback interaction

- QUIC runs over **UDP**; some networks block/throttle UDP. `TransportSelector`
  already falls back QUIC → TCP. Tune the **connect timeout** short (e.g. a few
  hundred ms on LAN) so fallback to TCP is fast when UDP is blocked.
- WebRTC (later) reuses much of this QoS thinking for the NAT-traversal remote
  case (its data channels are SCTP-over-DTLS with similar reliable/unreliable
  modes).

---

## Benchmark plan (when implementing)

1. **Input latency**: round-trip time for a `PointerMove` datagram on LAN
   (target sub-ms added by transport; p99 under load).
2. **File throughput**: saturate a gigabit LAN; verify window/GSO tuning.
3. **HOL isolation**: run a large transfer + input simultaneously; confirm input
   latency is unaffected (validates per-stream separation).
4. **UDP-blocked fallback**: firewall UDP; measure time to TCP fallback.
5. **Migration**: switch network interface mid-session; confirm no reconnect.

## Recommended crates
`quinn`, `rustls`, `rcgen` (generate device-keyed self-signed certs),
`socket2` (tune UDP socket buffers).
