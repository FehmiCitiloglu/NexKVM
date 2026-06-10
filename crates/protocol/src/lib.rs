//! nexkvm wire protocol.
//!
//! This crate is the dependency-light foundation of the platform: every other
//! crate that crosses the network boundary speaks the types defined here. It
//! intentionally depends only on `serde`, `bytes`, and `thiserror` so it can be
//! shared by desktop, mobile, and (future) embedded targets without pulling in
//! a runtime.
//!
//! # Layering
//! - `version` — protocol version + negotiation rules.
//! - `message` — the [`Envelope`] that wraps every framed message, plus the
//!   [`MessageKind`] discriminant that routes payloads to the owning crate.
//! - `frame` — length-prefixed framing for stream transports (TCP/QUIC).
//!
//! Payload *bodies* are intentionally opaque ([`bytes::Bytes`]). The protocol
//! crate does not know how to interpret an input event or a clipboard blob;
//! the owning domain crate serializes into / deserializes out of the body. This
//! keeps the protocol decoupled and avoids a dependency cycle.

mod error;
mod frame;
mod message;
mod version;

pub use error::ProtocolError;
pub use frame::{FrameCodec, MAX_FRAME_LEN};
pub use message::{Envelope, MessageId, MessageKind};
pub use version::{PROTOCOL_VERSION, ProtocolVersion, VersionRange};
