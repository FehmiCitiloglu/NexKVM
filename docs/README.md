# Documentation

This directory collects the stable design contracts for nexkvm. Research notes live under `docs/research/`; product-facing and contributor-facing contracts live here.

## Core Docs

- [Architecture](architecture.md): crate ownership, data flow, async boundaries, and platform isolation.
- [Protocol](protocol.md): wire envelope, framing, versioning, message routing, and fuzzing expectations.
- [Security](security.md): threat model, pairing, trust, encrypted sessions, replay protection, permissions, and sandboxing.
- [Plugins](plugins.md): plugin manifests, runtime model, sandbox profiles, marketplace policy, and hot reload.
- [API Documentation](api.md): how to build and navigate Rust API docs for the workspace.
- [Tooling](tooling.md): integration tests, fuzzing, benchmarks, CLI tools, simulation, CI, and release workflows.
- [Release Readiness](release-readiness.md): production gates, security requirements, platform smoke checks, and artifact expectations.

## Contribution Entry Point

Start with [../CONTRIBUTING.md](../CONTRIBUTING.md) for local setup, validation commands, coding conventions, and review expectations.
