# Documentation

This directory collects the stable design contracts for nexkvm. Research notes live under `docs/research/`; product-facing and contributor-facing contracts live here.

## Core Docs

- [Architecture](architecture.md): crate ownership, data flow, async boundaries, and platform isolation.
- [Protocol](protocol.md): wire envelope, framing, versioning, message routing, and fuzzing expectations.
- [Security](security.md): threat model, pairing, trust, encrypted sessions, replay protection, permissions, and sandboxing.
- [Plugins](plugins.md): plugin manifests, runtime model, sandbox profiles, marketplace policy, and hot reload.
- [API Documentation](api.md): how to build and navigate Rust API docs for the workspace.
- [Tooling](tooling.md): integration tests, fuzzing, benchmarks, CLI tools, simulation, CI, and release workflows.
- [Feature List](features.md): source-of-truth implemented/planned feature
  tracker. Check it before starting work, add new planned features there before
  implementation, and mark items complete only after verified repository
  changes land.
- [Two-Mac Apple Silicon Setup](setup-macos-apple-silicon.md): permissions,
  mutual pairing, active-peer selection, edge topology, clipboard history, and
  file transfer.
- [Release Readiness](release-readiness.md): automated and manual Apple Silicon
  production gates, security requirements, and signed-artifact expectations.
- [macOS Apple Silicon KVM Smoke](smoke/macos-kvm-mvp.md): physical two-device,
  TCC, transfer, signing, notarization, and Gatekeeper evidence record.
- [Apple Silicon Preview Notes](alpha-release-notes.md)
- [Historical Mac-to-Windows Input Alpha Smoke](smoke/real-device-input-alpha.md)
  (not part of the current production support claim).

## Contribution Entry Point

Start with [../CONTRIBUTING.md](../CONTRIBUTING.md) for local setup, validation commands, coding conventions, and review expectations.
