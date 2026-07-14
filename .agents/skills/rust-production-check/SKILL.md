---
name: rust-production-check
description: Run a production-readiness gate before completing Rust implementation, refactoring, release, or release-preparation tasks. Verify formatting, compilation, Clippy, tests, and release builds; inspect production paths for panic risks, unsafe code, async blocking, error handling, dependency hygiene, and tracked build artifacts; fix safe issues and report the evidence.
---

# Rust Production Check

Perform this gate after implementation is functionally complete and before declaring the task complete. Treat repository instructions and CI configuration as authoritative when they are stricter than the defaults below.

## Establish scope

1. Read `AGENTS.md`, `Cargo.toml`, the workspace manifests, and relevant CI configuration.
2. Inspect `git status --short` and preserve unrelated user changes.
3. Identify the changed crates and production paths. Exclude tests, examples, benchmarks, generated files, and fixtures from production-only judgments unless they ship in the artifact.
4. Determine required features, targets, toolchain overrides, and warnings policy from repository configuration. Prefer workspace-wide commands for a workspace task and crate-scoped commands only when the task is explicitly limited.

## Run the verification gate

Run each required command from the workspace root. Do not stop after the first failure; collect independent results when continuing is safe.

```sh
cargo fmt --all --check
cargo check --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo build --workspace --all-features --release
```

Adapt flags when the repository defines a different supported feature matrix or target set. Never enable mutually exclusive features merely to satisfy `--all-features`. Add repository-specific commands such as documentation tests or platform checks when required by `AGENTS.md` or CI.

If formatting fails after task-owned edits, run `cargo fmt --all`, review the diff, and rerun the check. Do not reformat unrelated user work without confirming the affected diff is safe.

## Inspect production risks

Use targeted searches and then review every relevant match in context. Do not classify a match as a defect solely from text search.

- Find `unwrap`, `expect`, explicit `panic!`, `unreachable!`, `todo!`, unchecked indexing, and assertion macros in production paths. Accept only cases whose invariant is local and convincingly documented; otherwise propagate or handle the error.
- Find every `unsafe` block, function, trait implementation, and FFI boundary. Confirm the safety invariant is documented, inputs are validated, and the unsafe region is minimal. Do not rewrite sound unsafe code without evidence of a defect.
- In async functions and tasks, look for blocking filesystem or networking APIs, synchronous locks, sleeps, process waits, CPU-heavy loops, and blocking channel operations. Move necessary blocking work to the runtime's blocking facility or an appropriate worker without holding async locks across the boundary.
- Trace errors at external boundaries. Preserve useful context and sources, avoid silent drops and blanket conversions, and ensure cleanup or partial-write behavior is correct.
- Review direct dependencies in changed manifests against production, development, build, and target-specific usage. Prefer moving test-only crates to `dev-dependencies` or removing clearly unused direct dependencies. Do not install an extra dependency-audit tool unless already required or explicitly authorized.
- Run `git ls-files` for `target` paths and inspect `.gitignore`. Flag tracked binaries, incremental state, package output, coverage data, and other generated build artifacts. Never delete user artifacts or rewrite Git history as part of this check.

Use searches such as these as starting points, adapting globs to the repository:

```sh
rg -n --glob '*.rs' '\.(unwrap|expect)\(|panic!|unreachable!|todo!|unsafe\b'
rg -n --glob '*.rs' 'std::thread::sleep|std::fs::|std::process::Command|blocking_(send|recv)|\.lock\(\)'
git ls-files | rg '(^|/)target(/|$)|\.(rlib|rmeta|pdb|dSYM)$'
```

## Fix and re-verify

Fix issues only when the change is clearly correct, local, within task scope, and covered by existing or newly added tests. Follow the repository's TDD policy for behavior changes. Leave semantic, API, dependency, platform, or safety decisions unresolved when they require user intent or broader design work.

After each fix, run the narrowest useful test for quick feedback. Then rerun every gate affected by the final diff. Before reporting completion, inspect `git diff --check`, `git status --short`, and the final diff for unintended changes.

## Report

Provide a concise verification report containing:

- Overall result: pass, pass with caveats, or fail.
- Commands run and their results, including any intentionally adapted flags.
- Safe fixes made.
- Production-risk inspection findings, including justified exceptions.
- Remaining blockers or unverified platform-specific work.

Never claim a command passed if it was skipped, timed out, or could not run. Include the failure reason and the most useful next action.
