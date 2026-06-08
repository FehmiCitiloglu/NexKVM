# CLI Simulation Design

## Scope

This slice improves the existing `coklu simulate [toml]` developer command so
the foundation-phase project can show useful, testable progress without adding
native OS integrations. The command will validate the local simulation file and
render a deterministic summary of devices, trust state, and planned connection
behavior.

The first implementation does not open sockets, pair devices, or start remote
sessions. It stays Sans-IO and uses existing config, discovery, crypto, and
network planning types where they already fit.

## Goals

- Parse `tools/sim/local-workspace.toml` into a typed simulation model.
- Report each simulated device with id, display name, OS, address, and trust
  state.
- Report connection intent between devices as direct LAN, reconnect candidate,
  blocked by missing trust, or invalid configuration.
- Fail with clear CLI errors when the TOML is malformed or references unknown
  devices.
- Keep output stable enough for integration tests.

## Non-Goals

- No real UDP discovery, QUIC/TCP dialing, pairing handshake, or trust-store
  writes.
- No native platform permission prompts.
- No UI or daemon runtime changes beyond reusing the simulation renderer from
  the CLI.
- No mobile app work.

## User-Facing Behavior

`cargo run -p coklu -- simulate tools/sim/local-workspace.toml` prints a concise
report:

```text
coklu simulation
config: tools/sim/local-workspace.toml

devices:
  - laptop (macos) trusted address=127.0.0.1:4101
  - desktop (linux) trusted address=127.0.0.1:4102

connections:
  - laptop -> desktop: direct-lan
  - desktop -> laptop: direct-lan

summary: 2 devices, 2 trusted, 2 planned connections
```

For invalid input, the command exits non-zero and prints the specific problem,
for example:

```text
simulate error: connection `laptop -> tablet` references unknown device `tablet`
```

## Data Model

The implementation will add a focused simulation module in the desktop app.
The model should be deliberately small:

- `SimulationConfig`: root TOML representation.
- `SimulatedDevice`: developer-provided device fixture.
- `SimulatedConnection`: desired source-to-target relationship.
- `ConnectionPlan`: validated rendered connection result.
- `SimulationReport`: deterministic data used by CLI rendering and tests.

Device identity is string-based inside the fixture to keep test data readable.
The validator maps those strings to known simulated devices before producing a
report.

## Architecture

`apps/desktop/src/cli.rs` remains responsible for argument parsing and text
rendering. A new `apps/desktop/src/simulation.rs` module owns TOML parsing,
validation, and report construction. `apps/desktop/src/main.rs` will call the
simulation module for the existing `simulate` command path instead of keeping
that behavior inline.

The boundary is:

- `simulation::load_report(path) -> anyhow::Result<SimulationReport>` reads and
  validates the TOML.
- `cli::render_simulation_report(path, &SimulationReport) -> String` renders
  stable human-readable output.

This keeps parsing and validation testable without spawning the binary, while
the existing CLI integration tests continue to cover command behavior.

## Error Handling

Validation fails before rendering when:

- The TOML cannot be parsed.
- There are zero devices.
- A device id is empty or duplicated.
- A device address is missing or not a socket address.
- A connection references an unknown source or target device.
- A connection source and target are the same device.

Errors should include the invalid device id or connection label so a developer
can fix the fixture quickly.

## Testing

Testing follows TDD:

1. Add unit tests for simulation parsing and validation.
2. Verify each new test fails for the expected reason.
3. Implement the smallest simulation model and validator that passes.
4. Add CLI rendering tests for stable output.
5. Add or update an integration test that runs `coklu simulate
   tools/sim/local-workspace.toml`.
6. Run the focused package tests, then the broader workspace tests if the
   sandbox allows them.

## Rollout

This is the first project-completion step. After it lands, the next practical
slice is pairing/trust: use the same simulation model to describe trusted and
untrusted peers, then move toward a real user-confirmed trust-store write path.
