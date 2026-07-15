# Security Policy

NexKVM handles keyboard, mouse, clipboard, and file data. Please do not report
security issues in a public issue, discussion, or pull request.

## Supported scope

The current security-supported release scope is the latest tagged `0.1.x`
release running between two Apple Silicon Macs on a trusted LAN. Older builds,
unreleased branches, Windows/Linux backends, relay/WebRTC paths, screen/audio
streaming, and plugins are not currently security-supported release targets.

## Reporting a vulnerability

Use the repository's private
[GitHub Security Advisory form](https://github.com/FehmiCitiloglu/NexKVM/security/advisories/new).
Include the affected version and platform, impact, minimal reproduction steps,
and whether the report involves a paired peer, local account access, or an
untrusted LAN participant. Remove real clipboard contents, keys, file paths,
and personal data from the report.

Please allow the maintainers time to reproduce and remediate the issue before
publishing details. The project's implemented controls, trust assumptions, and
known limitations are documented in [docs/security.md](docs/security.md).
