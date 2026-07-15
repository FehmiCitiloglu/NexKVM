# Security

NexKVM carries keyboard, mouse, clipboard, and file data, so a connection is
useful only after both endpoints authenticate a key the user intended to trust.
The current supported security scope is two Apple Silicon Macs on a reachable,
trusted LAN using the daemon's TCP transport and application-layer session
security. QUIC, WebRTC/relay, Windows, Linux, screen/audio streaming, cloud
features, and plugins are not production-security claims in this release scope.

## Threat model and boundary

The implemented design assumes a network attacker may observe, delay, replay,
drop, or modify LAN traffic and may open connections to the listening port. It
is intended to resist:

- an unpaired device attempting to use an input, clipboard, or file lane;
- impersonation of a previously paired device during reconnect;
- modification or replay of protected envelopes;
- a clipboard update claiming to originate from a different authenticated
  device;
- unsafe file manifests, path traversal, symlinks, source mutation, or content
  corruption;
- accidental persistence or transmission of clipboard items macOS marks as
  concealed.

It does not protect data after a trusted endpoint, the local user account, or
the operating system has been compromised. It also does not promise network
availability, traffic-flow confidentiality, or resistance to an unbounded
denial-of-service attacker.

## Device identity and trust

Each installation has a long-lived Ed25519 identity. The full 32-byte public
key is the authentication identity stored in the peer's trust registry; device
names, IP addresses, discovery records, and abbreviated fingerprints are only
selection/display metadata. The stable application `DeviceId` is derived from
the public key for routing and origin attribution, but the signed public key is
the authority used by the handshake.

Trust is mutual in the normal two-Mac setup. Mac A pins Mac B's public key and
Mac B pins Mac A's public key. A raw inbound or outbound connection whose public
key is absent from the local trust set is rejected before application lanes are
attached. A peer must then prove possession of the matching private key.

The desktop daemon always constructs a trusted-session configuration for its
runtime connections. Pairing is therefore effectively required even if the
legacy `security.require_pairing` field suggests a weaker policy. The legacy
`security.trust_on_reconnect` field is also parsed for configuration
compatibility but currently has no runtime effect: pinned peers are considered
for automatic reconnect whenever LAN discovery is enabled. Disable discovery to
disable that rediscovery path. An empty trust store accepts no peer, and an
explicitly configured Active peer that cannot be resolved does not silently
select a different key.

## First pairing ceremony

The operational pairing flow is a trust bootstrap, not a network-authenticated
key exchange:

1. `nexkvm pairing-uri <ip:port>` encodes the device name, full Ed25519 public
   key, a freshly generated 32-byte random nonce, and the advertised address in
   a bounded `nexkvm://pair/v1/...` URI.
2. `nexkvm pair <uri>` decodes the URI without changing trust and displays the
   peer identity/fingerprint for comparison.
3. The user compares that fingerprint through an independent trusted channel
   and explicitly accepts the URI with `nexkvm pair --accept <uri>` or the GUI.
4. Acceptance atomically persists the full public key in the local trust store.
   The peer's private-key possession is proved later, at connection time.

This out-of-band comparison is the first-pairing MITM defense. Transferring and
approving a substituted URI over the same compromised channel can pin an
attacker's key. Device name, address, or LAN discovery alone must never be used
as confirmation. The displayed fingerprint is an abbreviated representation of
the full pinned key, so the ceremony is not equivalent to comparing every key
byte.

### Pairing nonce limitation

Although a pairing URI contains a fresh cryptographically random nonce and the
crypto model has a TTL/single-use pairing state machine, the current CLI/GUI
acceptance path does **not** consume that state machine. It does not persist a
used-nonce registry, enforce a nonce expiry, or bind the URI nonce into the
subsequent trusted-session handshake. Reusing the same URI can therefore repeat
the same trust insertion; the operational bootstrap nonce does not currently
provide a single-use or anti-replay guarantee.

Replaying an old URI by itself does not give an attacker the private key needed
for the reconnect handshake, but the nonce must not be cited as protection
against pairing-ceremony replay or URI substitution. Until runtime
consumption/expiry is implemented, security rests on explicit out-of-band key
verification and later proof of private-key possession.

## Authenticated reconnect handshake

Every accepted TCP connection completes the following bounded handshake before
input, clipboard, or file handlers receive it:

1. Each endpoint obtains a fresh 32-byte challenge and fresh ephemeral X25519
   key from the operating system CSPRNG.
2. Each sends a handshake hello containing its long-lived public key,
   challenge, ephemeral public key, and protocol version. The receiver rejects
   an unpinned public key and an incompatible protocol version.
3. Each Ed25519-signs a role-specific transcript containing both long-lived
   keys, both challenges, both ephemeral keys, and the protocol version.
4. Each verifies the peer's signature with the already-pinned key. This proves
   possession of the corresponding long-lived private key and binds the fresh
   key agreement to both identities and this connection.
5. X25519 produces the shared secret; low-order peer keys that yield an all-zero
   secret are rejected.
6. HKDF-SHA-256 derives complementary A→B and B→A 32-byte keys. Its context
   binds the ordered identities, challenges, ephemeral keys, endpoint ordering,
   and protocol version.

The handshake has a ten-second timeout and fails closed on malformed frames,
untrusted keys, signature failure, incompatible versions, invalid ephemeral
keys, random-source failure, or key-derivation failure. Fresh ephemeral key
agreement avoids reusing a long-term identity key as an encryption key and is
intended to provide per-session forward secrecy, but this protocol composition
has not received an independent cryptographic audit.

The hello, identity public keys, challenges, ephemeral public keys, and
signatures are visible to a LAN observer. They authenticate the key agreement;
they do not provide identity privacy.

## Session encryption, metadata binding, and replay protection

After authentication, the connection wraps every application envelope in
ChaCha20-Poly1305 using independent transmit and receive keys. Input, clipboard,
and file-transfer payloads rely on this outer authenticated session; they are
not permitted on an unauthenticated raw transport.

Before application lanes are created, a protected `Handshake` envelope with
reserved `MessageId(0)` performs session arbitration. The lower ordered public
key is the sole decision authority, and a two-phase accept/ack makes both ends
attach every lane to the same physical connection during a simultaneous
cross-dial. A bounded fallback still permits a single reverse-direction
connection. Duplicate physical sessions are closed before lane fan-out.

After that control exchange, one `SequencedConnection` allocates globally
monotonic 64-bit ids starting at 1 across every lane on that connection. This
prevents concurrent input, clipboard, and file tasks from reusing an AEAD
nonce. Exhausting the identifier space fails instead of wrapping.

For each protected envelope:

- the message id determines the AEAD nonce and is also included in associated
  data;
- protocol major/minor and message kind are copied inside the encrypted body
  and compared with the visible routing header after decryption;
- ciphertext authentication covers the payload and those bound values;
- a 128-message sliding receive window rejects duplicate and too-old ids while
  allowing limited out-of-order delivery.

A new connection gets new X25519 material and new directional keys, so an old
session's ciphertext does not authenticate in a new session even though message
ids restart. Session key buffers, ephemeral private material, and shared-secret
buffers use non-leaking debug output and zeroizing wrappers where represented
by NexKVM types.

Routing metadata needed by the transport remains observable: IP addresses,
ports, connection timing, frame length, message id, protocol version, and
message kind are not traffic-padded or hidden. Tampering with bound header
values is detected, but their confidentiality is not a goal of the current LAN
protocol.

## Lane authorization and data handling

- Input, clipboard, and file lanes are attached only after the authenticated
  connection exposes the verified peer public key.
- Input and file-transfer handlers enforce an explicit Active peer when one is
  configured. Leaving Active peer on automatic selection permits any already
  trusted peer that otherwise satisfies the lane policy, so an explicit peer is
  recommended whenever more than one device is trusted.
- Clipboard updates carry an origin `DeviceId`; the receiver derives the
  expected origin from the authenticated peer public key and rejects a mismatch.
- Clipboard synchronization and file transfer are disabled by default. Input
  control also defaults to Disabled.
- Clipboard snapshots and formats are bounded. Items carrying a recognized
  macOS concealed marker are excluded from sync and encrypted history.
- File transfer requires an enabled receive policy and the selected trusted
  peer. Manifests, entry counts, aggregate size, chunks, checkpoints, and time
  spent waiting for protocol messages are bounded. Relative paths are
  normalized; traversal, symlinks, special source files, hash mismatch, unsafe
  destinations, and source changes fail closed.
- Received files are isolated under `NexKVM/<transfer-id>/`; enabling file
  transfer is the receiving consent. There is currently no per-transfer
  approval dialog.

Session encryption protects data in transit only. Clipboard data becomes the
target's ordinary system clipboard, and completed files are ordinary plaintext
files in the receive directory.

## Local persistence

On macOS, application state normally lives under
`~/Library/Application Support/nexkvm/`. Protections and limitations differ by
artifact:

| Artifact | Implemented protection | Important limitation |
| --- | --- | --- |
| `identity.json` | CSPRNG-generated Ed25519 seed, owner-only `0600` file, atomic same-directory replacement, regular-file/symlink rejection, non-leaking key debug output | The seed is stored in a filesystem fallback, not Keychain or Secure Enclave; compromise of the local account can copy it |
| `trust.json` | Contains public keys/metadata only; owner-only atomic writes, deterministic serialization, path stabilization, unsafe file/directory-chain rejection, and explicit flush after CLI acceptance | Trust entries are not secret; end-user revocation/rotation UX is incomplete |
| `config.toml` | One-MiB read/write bound, regular-file/symlink checks, atomic owner-only writes, and `0700` directory creation on Unix when the config writer creates it | It contains operational names, addresses, peer selection, and local paths; existing parent-directory ownership still matters |
| `clipboard-history.enc` | Bounded ChaCha20-Poly1305 authenticated encryption with a fresh nonce, owner-only atomic writes, concealed-item rejection, tamper failure, and a cross-process lock | Its random key is an adjacent owner-only `clipboard-history.key`, not Keychain; a process that can read both files can decrypt history |
| file-transfer queue/receives | Owner-only durable queue records, source revalidation, authenticated hashes, safe transfer directories, and no silent overwrite | Queue metadata can reveal local source paths; received content is plaintext at rest |

The filesystem defenses reduce accidental disclosure and symlink/partial-write
attacks. They are not a sandbox against another process running as the same
user, and they cannot close every filesystem TOCTOU race if an untrusted local
principal can concurrently replace parent directories.

## macOS permissions

Accessibility permission is required for input capture/injection; macOS may
also present Input Monitoring and Local Network prompts. These permissions are
powerful and are bound to the exact app/code identity. Grant them only to the
signed build being tested, then quit and restart the app so the daemon observes
the new state. Permission denial must leave the affected input role unavailable
rather than silently weakening authentication or bypassing TCC.

TCC is not peer authorization. A process with Accessibility permission is still
restricted to authenticated, paired sessions by NexKVM's own handshake and lane
policy. Conversely, a paired peer does not grant the local process a missing
macOS permission.

Every CoreGraphics event created by NexKVM's injector carries a private source
marker. The capture tap lets those events reach the local system but excludes
them from its outbound queue. This prevents two Macs in the bidirectional
`Both` role from recapturing and bouncing the same synthetic edge event.

## Residual and manual risks

The current release decision must account for these residual risks:

- Pairing URI nonce expiry/single-use is not enforced in the operational
  acceptance/handshake path.
- The displayed fingerprint is abbreviated, and the pairing ceremony has no
  independently audited secure-attention UI.
- Long-term identity and clipboard-history keys use owner-only files rather
  than macOS Keychain/Secure Enclave.
- There is no complete user-facing trust revocation/key-rotation workflow.
- Automatic Active peer selection broadens access to any already-trusted peer;
  select one explicitly for input/file use.
- File-transfer enablement is persistent consent without a per-transfer prompt.
- Concealed clipboard filtering depends on source applications correctly
  marking sensitive selections. Unmarked passwords or tokens can still sync or
  enter history.
- Discovery and handshake traffic reveal device/network metadata, and the
  listener bounds concurrent pre-authentication handshakes and applies a
  timeout, but it has no per-source rate limiter or network-wide DoS defense.
- The Apple release dependency gate narrowly ignores two upstream maintenance
  conditions: `ttf-parser` is unmaintained with no safe egui migration yet, and
  the latest `mdns-sd` transitively pins a yanked `spin 0.9.8`. Neither exception
  currently has a RustSec vulnerability; every other advisory remains denied.
- A trusted or compromised peer legitimately receives the input, clipboard, or
  file data enabled for it; cryptography cannot constrain what that endpoint
  does after receipt.
- The custom protocol composition and native macOS FFI have not received an
  independent security audit and are not post-quantum secure.
- Developer tests cannot prove physical event-tap behavior, TCC UX, signed-app
  identity, notarization, or a clean-machine Gatekeeper launch.

Resolve or explicitly accept these risks through the manual gates in
[Release Readiness](release-readiness.md) and record the exact signed artifact in
[macOS Apple Silicon KVM Smoke Checks](smoke/macos-kvm-mvp.md). Do not describe
QUIC, WebRTC/relay, Windows, Linux, screen/audio streaming, cloud features, or
plugins as production-secure merely because supporting models or crates exist.

## Reporting security issues

Follow the repository [security policy](../SECURITY.md) and use its private
advisory channel. Do not disclose a suspected vulnerability publicly before
maintainers can assess it. Include the affected version, platform, impact, and
the smallest reproduction that contains no real secrets or user data.
