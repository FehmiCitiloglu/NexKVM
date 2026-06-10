# API Documentation

Rust API documentation is generated directly from crate-level and item-level doc comments.

## Build API Docs

```sh
cargo doc --workspace --all-features --no-deps
```

Open:

```text
target/doc/nexkvm/index.html
```

For strict link checking in CI/local validation:

```sh
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps
```

## Crate Entry Points

| Crate | API Focus |
| --- | --- |
| `nexkvm-core` | `EventBus`, platform traits, workspace model, collaboration sessions. |
| `nexkvm-protocol` | `FrameCodec`, `Envelope`, `MessageKind`, `VersionRange`. |
| `nexkvm-crypto` | pairing, trust, session-security traits/models. |
| `nexkvm-network` | transport traits, sessions, heartbeat, RTT/quality, zero-copy packets. |
| `nexkvm-input` | `InputEvent`, capture/inject traits, topology, batching, polling, prediction. |
| `nexkvm-clipboard` | clipboard sync payload/history/conflict/encryption models. |
| `nexkvm-streaming` | file transfer, audio routing, screen streaming, stream traits. |
| `nexkvm-plugins` | plugin traits, manifests, permissions, runtime/sandbox/marketplace/hot reload. |
| `nexkvm-storage` | TOML config and trust persistence. |
| `nexkvm-telemetry` | tracing configuration. |

## Documentation Style

- Public APIs should explain ownership and error behavior.
- Backend traits should say where blocking work must be handled.
- Security-sensitive APIs should name the permission or trust boundary involved.
- Platform-specific APIs should identify relevant OS permissions and degraded behavior.
- Complex Sans-IO state machines should document who owns timers/tasks and what is pure state.

## Examples

Keep examples small and compile-friendly. Prefer examples inside tests when behavior matters across crates.

Useful commands:

```sh
cargo test --workspace --all-features
cargo test -p nexkvm-network --test protocol_pipeline
cargo run -p nexkvm -- doctor
```
