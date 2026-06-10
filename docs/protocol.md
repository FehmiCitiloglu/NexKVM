# Protocol

The protocol crate defines the wire-level contract shared by every nexkvm transport and feature crate. It intentionally stays dependency-light and runtime-free.

## Versioning

`PROTOCOL_VERSION` is semantic at the wire level:

- `major`: breaking changes such as frame layout changes, envelope field changes, message-kind renumbering, or removal of existing semantics.
- `minor`: backward-compatible additions such as new message kinds or optional payload features.

Peers negotiate the effective version with `VersionRange`. A mismatched major is rejected; a newer minor is capped to the highest mutually supported minor.

## Framing

Stream transports use `FrameCodec`:

```text
+----------+----------------+
| len u32  | payload bytes  |
+----------+----------------+
```

The maximum payload length is `MAX_FRAME_LEN` (16 MiB). Larger data must be chunked by domain crates such as `streaming`.

Datagram transports carry one encoded envelope per datagram and do not need the length prefix.

## Envelope

Every payload crossing the network is wrapped in `Envelope`:

- `version`: negotiated protocol version.
- `id`: monotonic `MessageId`, used with crypto/session nonces for replay protection.
- `kind`: `MessageKind` routing discriminant.
- `body`: opaque `bytes::Bytes` owned by the destination domain crate.

The protocol crate never parses input, clipboard, plugin, file, or media bodies. This avoids dependency cycles and keeps wire routing stable.

## Message Kinds

Current stable discriminants:

| Kind | Discriminant | Owner |
| --- | ---: | --- |
| `Handshake` | 0 | `core` / `crypto` |
| `Pairing` | 1 | `crypto` |
| `Heartbeat` | 2 | `network` |
| `Input` | 10 | `input` |
| `Clipboard` | 11 | `clipboard` |
| `FileTransfer` | 12 | `streaming` |
| `Discovery` | 13 | `discovery` |
| `Stream` | 14 | `streaming` |
| `Plugin` | 20 | `plugins` |
| `Workspace` | 30 | `core` |
| `Notification` | 31 | `core` |
| `Command` | 32 | `core` |
| `Mesh` | 40 | `network` |
| `Relay` | 41 | `network` |
| `CloudSync` | 42 | `core` |
| `Enterprise` | 43 | `core` |
| `Team` | 44 | `core` |
| `BrowserSession` | 45 | `network` |
| `Control` | 100 | `core` |

Never renumber an existing discriminant without a major-version bump.

## Zero-Copy Handling

`crates/network::ZeroCopyPacket` wraps complete packet bytes and decodes envelopes with body slices backed by the original `Bytes`. This is the preferred pattern for transport implementations.

## Fuzzing Contract

Malformed peer input must not panic. Protocol fuzzing lives in `fuzz/`:

```sh
cargo install cargo-fuzz --locked
cargo fuzz run protocol_decode
```

The target feeds arbitrary bytes through stream framing and envelope decoding.

## Integration Test

The cross-crate protocol pipeline test lives at `crates/network/tests/protocol_pipeline.rs` and verifies framing, zero-copy packet decoding, input latency integration, and streaming compression policy behavior.
