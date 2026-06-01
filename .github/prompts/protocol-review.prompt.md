---
description: "Audit protocol versioning, state synchronization, and security boundaries across the continuity platform's network/protocol/crypto crates."
tools: [read, search, web]
argument-hint: "Optionally name a crate, message type, or boundary to focus on (e.g. 'discovery handshake' or 'clipboard transfer')"
---
Perform a focused review of the protocol, networking, and security boundaries for this Rust continuity platform. If the user named a specific area (`${input}`), scope the audit to it; otherwise review the `protocol`, `network`, and `crypto` crates broadly.

Do NOT modify code in this prompt — produce findings and recommendations only.

## Review Checklist

### Protocol Versioning
- Is there an explicit, negotiated protocol version on connection setup?
- Are messages forward/backward compatible (additive Serde fields, `#[serde(default)]`, no breaking reorders)?
- Is there a clean rejection path for incompatible peers?

### State Synchronization
- Are input/clipboard/session states reconciled deterministically after reconnect?
- Are events ordered/sequenced (sequence numbers, monotonic timestamps) and idempotent on replay?
- Is backpressure handled on hot paths (bounded channels, batching, no unbounded buffering)?

### Security Boundaries
- Is all peer-supplied / deserialized data validated before use?
- Is end-to-end encryption and mutual authentication enforced on every transport (QUIC/TCP/WebRTC)?
- Are pairing tokens short-lived, single-use, and bound to a device identity?
- Are trust roots / certificates verified, with no insecure fallbacks?
- Are plugins sandboxed with explicit permission boundaries?

### Latency & Resilience
- Any blocking operations on async paths? Any avoidable copies on hot paths?
- Are heartbeats, timeouts, and reconnect/backoff defined and bounded?

## Output Format
For each finding: **severity** (critical/high/medium/low), the file/line reference, the issue, and a concrete remediation. End with a prioritized action list.
