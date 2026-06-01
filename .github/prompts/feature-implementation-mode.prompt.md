---
description: "Implement one requested continuity feature end-to-end with strict scope control, minimal interfaces, async-safe Rust, and targeted tests."
name: "Feature Implementation Mode"
argument-hint: "Describe the feature to implement (e.g. 'Implement feature LAN device discovery' or 'Start feature development: clipboard sync')."
agent: "Continuity Engineer"
tools: [read, edit, search, execute, todo]
---

Run in FEATURE IMPLEMENTATION MODE for this workspace.

Feature request:
> ${input:feature}

## Trigger Behavior
Treat requests like these as implementation requests for this prompt:
- "Implement feature X"
- "Start feature development"

If the feature name is unclear, infer it from the user message and proceed with the smallest valid implementation slice.

## Required Assumptions
Assume all of the following are already true:
- Workspace already exists.
- Core protocol already exists.
- Networking layer already exists.
- Basic daemon runs successfully.
- Test infrastructure is working.

## Goal
Move from design-phase discussion to a working prototype quickly, without destabilizing existing behavior.

## Scope Guardrails
Focus only on implementing the requested feature end-to-end.

Do NOT:
- Redesign the entire architecture.
- Suggest large refactors unless a critical bug exists.
- Rewrite unrelated crates.
- Expand scope beyond the feature.

## Feature Implementation Rules
For each feature:
1. Identify affected crates only.
2. Define minimal interfaces needed.
3. Implement incrementally.
4. Ensure cross-platform compatibility.
5. Ensure async and non-blocking behavior.
6. Add tests for core logic.
7. Add an integration test if possible.

## Strict Scope Control
Limit changes to:
- Required crates only.
- Required modules only.
- Required APIs only.

If something is missing, do one of:
- Stub it.
- Mock it.
- Add a minimal interface.

Pause for architecture redesign only when one of these is true:
- A security vulnerability exists.
- The protocol is fundamentally broken.
- A platform limitation makes the feature impossible.

## Implementation Priority Order
When multiple parts are needed, implement in this order:
1. Protocol changes (if needed).
2. Network layer integration.
3. Core logic implementation.
4. Platform abstraction binding.
5. Daemon integration.
6. UI hooks (if applicable).
7. Tests.

## Current Priority Features
Prioritize these when the user does not specify a feature explicitly:
- LAN device discovery.
- Mouse sharing.
- Keyboard sharing.
- Clipboard sync.
- File transfer.
- Secure pairing.
- Cursor transition logic.
- Multi-monitor mapping.

## Anti-Overengineering Rule
Avoid:
- Rewriting workspace structure.
- Introducing new architecture layers.
- Adding unnecessary abstractions.
- Premature plugin systems.
- Over-modularization.

Keep it simple, working, and testable.

## Required Output Format
Always provide:
1. Brief explanation (what will change).
2. Crate/module touched.
3. Implementation steps.
4. Code.
5. Minimal tests.
6. How to run/test it.

Follow Rust conventions in [rust.instructions.md](../instructions/rust.instructions.md).
