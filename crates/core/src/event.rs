//! The in-process event bus — nexkvm's decoupling backbone.
//!
//! # Architecture
//! Producers (platform input capture, network ingress, discovery) and
//! consumers (input injection, clipboard sync, UI, plugins) never reference
//! each other directly. They communicate through a single typed pub/sub
//! [`EventBus`] built on [`tokio::sync::broadcast`], which gives cheap
//! multi-consumer fan-out.
//!
//! ## Why broadcast, and the backpressure tradeoff
//! `broadcast` fan-out means every subscriber sees every event. It is bounded:
//! a subscriber that lags past the capacity receives
//! [`RecvError::Lagged`](tokio::sync::broadcast::error::RecvError::Lagged) and
//! skips ahead rather than stalling fast producers. This is the right tradeoff
//! for *real-time* signals (pointer motion, key events) where the freshest
//! event matters more than every historical one. Reliable, ordered streams
//! (file transfer, clipboard payloads) must use a dedicated channel rather than
//! the lossy broadcast path — those are addressed in the streaming/clipboard
//! crates, not here.
//!
//! Event *bodies* that originate from the wire stay opaque ([`bytes::Bytes`]),
//! matching [`nexkvm_protocol::Envelope`]; domain crates decode their own kinds.
//! This keeps `core` free of dependencies on the feature crates.

use bytes::Bytes;
use nexkvm_protocol::MessageKind;
use tokio::sync::broadcast;

use crate::automation::{CommandId, CrossDeviceNotification};
use crate::identity::{DeviceId, DeviceInfo};

/// Default bus capacity (events retained for lagging subscribers).
const DEFAULT_CAPACITY: usize = 1024;

/// A high-level event flowing on the [`EventBus`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Event {
    /// A device was discovered on the LAN (not yet connected).
    DeviceDiscovered(DeviceInfo),
    /// A secure session with a device was established.
    DeviceConnected(DeviceId),
    /// A session with a device ended.
    DeviceDisconnected(DeviceId),
    /// A decoded message arrived from a peer and needs domain dispatch.
    Inbound {
        /// Sending device.
        from: DeviceId,
        /// Routing discriminant (matches the wire envelope).
        kind: MessageKind,
        /// Opaque, already-decrypted payload for the owning crate to decode.
        payload: Bytes,
    },
    /// A locally produced message that should be sent to peer(s).
    Outbound {
        /// Target device, or `None` to broadcast to all connected peers.
        to: Option<DeviceId>,
        /// Routing discriminant.
        kind: MessageKind,
        /// Opaque payload to encrypt + frame at the network layer.
        payload: Bytes,
    },
    /// A cross-device notification is ready for local display or forwarding.
    Notification(CrossDeviceNotification),
    /// A universal quick command was invoked by the user or a trusted peer.
    QuickCommandInvoked(CommandId),
    /// Graceful shutdown requested.
    Shutdown,
}

/// An [`Event`] tagged with delivery metadata.
#[derive(Debug, Clone)]
pub struct EventEnvelope {
    /// The event payload.
    pub event: Event,
    /// Monotonic publish timestamp (milliseconds since process start basis is
    /// the bus's clock; used for ordering/diagnostics, not security).
    pub at_millis: u64,
}

/// Cloneable handle to the shared event bus.
///
/// Cloning is cheap (an `Arc`-like broadcast sender clone) and every clone
/// publishes to the same bus. Subscribe to receive a fresh stream.
#[derive(Debug, Clone)]
pub struct EventBus {
    tx: broadcast::Sender<EventEnvelope>,
}

impl EventBus {
    /// Create a bus with the default capacity.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// Create a bus retaining up to `capacity` events for lagging subscribers.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        let (tx, _rx) = broadcast::channel(capacity);
        Self { tx }
    }

    /// Publish an event to all current subscribers.
    ///
    /// Returns the number of subscribers that received it. A return of `0`
    /// means no consumers are currently attached (not an error — producers may
    /// run ahead of consumers during startup).
    pub fn publish(&self, event: Event) -> usize {
        let envelope = EventEnvelope {
            event,
            at_millis: now_millis(),
        };
        self.tx.send(envelope).unwrap_or(0)
    }

    /// Subscribe to receive future events.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<EventEnvelope> {
        self.tx.subscribe()
    }

    /// Number of currently attached subscribers.
    #[must_use]
    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

fn now_millis() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn delivers_to_multiple_subscribers() {
        let bus = EventBus::new();
        let mut a = bus.subscribe();
        let mut b = bus.subscribe();

        let n = bus.publish(Event::Shutdown);
        assert_eq!(n, 2);

        assert!(matches!(a.recv().await.unwrap().event, Event::Shutdown));
        assert!(matches!(b.recv().await.unwrap().event, Event::Shutdown));
    }

    #[tokio::test]
    async fn publish_with_no_subscribers_is_not_fatal() {
        let bus = EventBus::new();
        assert_eq!(bus.publish(Event::Shutdown), 0);
    }
}
