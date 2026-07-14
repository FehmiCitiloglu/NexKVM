# AGENTS

## Quick‑start for an agent
- **Workspace layout**: `crates/*` contain core logic; `apps/desktop` is the daemon + CLI; platform‑specific code lives in `crates/platform/*`.  The single binary is built with `cargo run -p nexkvm`.
- **Build**: `cargo build --workspace --all-features`.
- **Run daemon**: `cargo run -p nexkvm` (no subcommand starts the desktop daemon).
- **CLI commands**:
  - `cargo run -p nexkvm -- doctor`
  - `cargo run -p nexkvm -- protocol`
  - `cargo run -p nexkvm -- simulate tools/sim/local-workspace.toml`

## Test‑driven development workflow
1. **Write a test** in the appropriate `tests/` or crate unit‑test module.
2. Run the test suite: `cargo test --workspace --all-features`.
3. Implement minimal code to make the test pass.
4. Run `cargo clippy --workspace --all-targets --all-features` to keep code clean.
5. Repeat until the feature is fully implemented and all tests pass.

## Important test commands
- **All tests**: `cargo test --workspace --all-features`
- **Integration test** (protocol‑pipeline): `cargo test -p nexkvm-network --test protocol_pipeline`
- **Fuzz target**: `cargo install cargo-fuzz && cargo fuzz run protocol_decode`
- **Benchmarks**: `cargo bench -p nexkvm-network --bench latency_suite` or `sh scripts/bench.sh`

## Local simulation & packaging
- Run the local simulator: `sh scripts/simulate-local.sh` (uses `tools/sim/local-workspace.toml`).
- Full local project check: `./scripts/test-project.sh` – prepares isolated config, runs format/lint/test, builds binary and package.
- Packaging flags:
  - `NEXKVM_SKIP_PACKAGE=1` to skip creating the macOS `.app`.
  - `NEXKVM_RUN_LINUX_PACKAGING=1` to build Linux `.deb/.rpm/AppImage`.

## CI behaviour
The GitHub workflow (`.github/workflows/ci.yml`) runs:
- `cargo fmt --all --check`
- `cargo clippy` with warnings as errors
- full test suite (`--workspace --all-features`)
- API docs generation with `RUSTDOCFLAGS=-D warnings`
- release build (`cargo build --release`).

## Note on TDD
All feature development starts with a failing test.  Agents should **never** add code that compiles without a corresponding test.  Use the `tests/` directory for integration tests and the crate's unit‑test modules for pure logic.
