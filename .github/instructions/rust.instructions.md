---
description: "Idiomatic Rust conventions for the continuity platform: error handling, unsafe policy, async/Tokio, modularity, and security."
applyTo: "**/*.rs"
---
# Rust Conventions

## Style & Idioms
- Write idiomatic, stable Rust. Run `cargo fmt` and keep code `clippy`-clean (`cargo clippy --all-targets`).
- Prefer composition and trait-based abstractions over inheritance-like patterns.
- Prefer iterators and combinators over manual loops where it improves clarity.
- Avoid premature abstraction and overengineering — solve the current need simply.

## Error Handling
- Library crates: define typed errors with `thiserror`; return `Result<T, E>` — never `unwrap()`/`expect()`/`panic!` on recoverable paths.
- Application/binary crates: use `anyhow` for top-level error propagation and context.
- Add context with `.context(...)` rather than swallowing errors.
- Reserve `panic!`/`unwrap`/`expect` for genuinely unreachable invariants, and document why.

## Unsafe
- Minimize `unsafe`. Every `unsafe` block requires a `// SAFETY:` comment justifying the invariants.
- Isolate platform-specific `unsafe` (FFI, raw input APIs) inside the relevant `platform-*` crate behind a safe trait boundary.

## Async / Tokio
- Async-first. Never block the async runtime — no blocking I/O, `std::thread::sleep`, or heavy CPU on async tasks.
- Offload blocking/CPU-bound work via `tokio::task::spawn_blocking` or a dedicated thread pool.
- Use bounded channels for backpressure on hot paths; prefer zero-copy (`Bytes`) where possible.

## Modularity & Safety
- Keep platform-specific code isolated in `platform-*` crates; expose cross-platform traits from `core`/`platform`.
- Gate optional functionality behind Cargo feature flags rather than runtime branches.
- No unsafe global mutable state; prefer dependency injection and explicitly passed handles.
- Validate input at system/network boundaries; never trust deserialized or peer-supplied data.

## Testing
- Unit-test pure logic; use `#[tokio::test]` for async. Add integration tests under `tests/` for cross-crate behavior.
- Document complex logic and public APIs with `///` doc comments; keep examples compiling where practical.
