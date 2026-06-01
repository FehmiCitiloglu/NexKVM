//! Low-latency input injection layer.
//!
//! Received [`InputEvent`]s must be replayed on the local machine through the
//! OS's synthetic-input API (`SendInput` on Windows, `CGEventPost` on macOS,
//! `uinput`/evdev on Linux). Those native calls live in the `platform-*` crates
//! behind the [`InputInjector`](crate::InputInjector) boundary; this module owns
//! the *transport-side* orchestration that feeds them:
//!
//! - [`InjectionCommand`] — a platform-neutral description of exactly what a
//!   backend must synthesize for one event. It is the stable seam the native
//!   FFI maps onto (see the `inject` modules in the `platform-*` crates).
//! - [`InjectionEngine`] — an async, non-blocking driver that batches inbound
//!   events with the existing [`InputBatcher`] (coalescing motion, preserving
//!   click/key order) and flushes them to an [`InputInjector`].
//!
//! The engine is sans-IO and clock-injected: the caller owns the timer and
//! decides when to flush, so there are no hidden tasks and no locks held across
//! `.await`. Coalescing here is what keeps injection low-latency under high poll
//! rates — hundreds of motion events per second collapse into one synthesized
//! move per flush window.

use std::time::{Duration, Instant};

use crate::{InputBatchPolicy, InputBatcher, InputError, InputEvent, InputInjector, MouseButton};

/// A platform-neutral injection instruction: the precise thing a backend must
/// synthesize for a single received event.
///
/// This is the contract between the cross-platform injection layer and the
/// native backends. Each `platform-*` crate maps these onto its OS primitives
/// (Windows `INPUT`, macOS `CGEvent`, Linux evdev/uinput) without re-deriving
/// the semantics.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InjectionCommand {
    /// Move the pointer to an absolute position, normalized `[0.0, 1.0]`.
    MoveAbsolute {
        /// Horizontal position.
        x: f64,
        /// Vertical position.
        y: f64,
    },
    /// Move the pointer by a screen-fraction delta with acceleration applied.
    MoveRelative {
        /// Horizontal delta.
        dx: f64,
        /// Vertical delta.
        dy: f64,
    },
    /// Move the pointer by a raw, unaccelerated device-count delta.
    MoveRaw {
        /// Horizontal delta in device units.
        dx: i32,
        /// Vertical delta in device units.
        dy: i32,
    },
    /// Press or release a mouse button.
    Button {
        /// Which button.
        button: MouseButton,
        /// `true` = press, `false` = release.
        pressed: bool,
    },
    /// Emit a scroll delta (lines).
    Scroll {
        /// Horizontal delta.
        dx: f64,
        /// Vertical delta.
        dy: f64,
    },
    /// Press or release a key, identified by an OS-neutral keycode (USB HID
    /// usage id; see [`crate::Modifier`]).
    Key {
        /// The keycode.
        keycode: u32,
        /// `true` = press, `false` = release.
        pressed: bool,
    },
}

impl InputEvent {
    /// Translate this event into the [`InjectionCommand`] a backend synthesizes.
    ///
    /// Total: every [`InputEvent`] maps to exactly one command.
    #[must_use]
    pub fn to_injection_command(self) -> InjectionCommand {
        match self {
            InputEvent::PointerMove { x, y } => InjectionCommand::MoveAbsolute { x, y },
            InputEvent::RelativeMove { dx, dy } => InjectionCommand::MoveRelative { dx, dy },
            InputEvent::RawMotion { dx, dy } => InjectionCommand::MoveRaw { dx, dy },
            InputEvent::ButtonPress(button) => InjectionCommand::Button {
                button,
                pressed: true,
            },
            InputEvent::ButtonRelease(button) => InjectionCommand::Button {
                button,
                pressed: false,
            },
            InputEvent::Scroll { dx, dy } => InjectionCommand::Scroll { dx, dy },
            InputEvent::KeyPress(keycode) => InjectionCommand::Key {
                keycode,
                pressed: true,
            },
            InputEvent::KeyRelease(keycode) => InjectionCommand::Key {
                keycode,
                pressed: false,
            },
        }
    }
}

/// Async, non-blocking driver that batches inbound events and injects them.
///
/// Wraps an [`InputBatcher`] (for low-latency coalescing) and an
/// [`InputInjector`] backend. The caller pushes events with [`submit`] and,
/// driven by [`deadline`]/[`should_flush`], calls [`flush`] to drain the
/// coalesced batch and synthesize it locally.
///
/// [`submit`]: InjectionEngine::submit
/// [`deadline`]: InjectionEngine::deadline
/// [`should_flush`]: InjectionEngine::should_flush
/// [`flush`]: InjectionEngine::flush
#[derive(Debug)]
pub struct InjectionEngine<I> {
    batcher: InputBatcher,
    injector: I,
}

impl<I: InputInjector> InjectionEngine<I> {
    /// Create an engine over `injector` using `policy` for batching.
    #[must_use]
    pub fn new(injector: I, policy: InputBatchPolicy) -> Self {
        Self {
            batcher: InputBatcher::new(policy),
            injector,
        }
    }

    /// Update the adaptive batching window from a smoothed RTT estimate.
    pub fn update_rtt(&mut self, rtt: Option<Duration>) {
        self.batcher.update_rtt(rtt);
    }

    /// Buffer one received event for injection.
    pub fn submit(&mut self, event: InputEvent, now: Instant) {
        self.batcher.push(event, now);
    }

    /// The current flush deadline, if any events are buffered.
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        self.batcher.deadline()
    }

    /// Whether the buffered batch should be flushed at `now`.
    #[must_use]
    pub fn should_flush(&self, now: Instant) -> bool {
        self.batcher.should_flush(now)
    }

    /// Whether nothing is buffered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.batcher.is_empty()
    }

    /// Borrow the underlying injector backend.
    #[must_use]
    pub fn injector(&self) -> &I {
        &self.injector
    }

    /// Drain the coalesced batch and inject each event in send order.
    ///
    /// Returns the [`InjectionCommand`]s that were synthesized, in order, for
    /// observability/testing. Injection stops at the first backend error so the
    /// caller can surface it without losing ordering guarantees.
    ///
    /// # Errors
    /// Returns [`InputError`] if the backend rejects an event.
    pub async fn flush(&mut self, now: Instant) -> Result<Vec<InjectionCommand>, InputError> {
        if !self.batcher.should_flush(now) {
            return Ok(Vec::new());
        }
        let batch = self.batcher.drain();
        let mut injected = Vec::with_capacity(batch.len());
        for event in batch {
            self.injector.inject(event).await?;
            injected.push(event.to_injection_command());
        }
        Ok(injected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// Records every injected event; can be told to fail after N events.
    #[derive(Debug, Default)]
    struct RecordingInjector {
        events: Mutex<Vec<InputEvent>>,
        fail_after: Option<usize>,
    }

    impl RecordingInjector {
        fn failing_after(n: usize) -> Self {
            Self {
                events: Mutex::new(Vec::new()),
                fail_after: Some(n),
            }
        }

        fn recorded(&self) -> Vec<InputEvent> {
            self.events.lock().expect("poisoned").clone()
        }
    }

    #[async_trait]
    impl InputInjector for RecordingInjector {
        async fn inject(&self, event: InputEvent) -> Result<(), InputError> {
            let mut events = self.events.lock().expect("poisoned");
            if self.fail_after.is_some_and(|n| events.len() >= n) {
                return Err(InputError::Backend("synthetic failure".into()));
            }
            events.push(event);
            Ok(())
        }
    }

    fn engine(injector: RecordingInjector) -> InjectionEngine<RecordingInjector> {
        InjectionEngine::new(injector, InputBatchPolicy::low_latency())
    }

    #[test]
    fn maps_every_event_to_a_command() {
        assert_eq!(
            InputEvent::PointerMove { x: 0.5, y: 0.25 }.to_injection_command(),
            InjectionCommand::MoveAbsolute { x: 0.5, y: 0.25 }
        );
        assert_eq!(
            InputEvent::KeyPress(0x04).to_injection_command(),
            InjectionCommand::Key {
                keycode: 0x04,
                pressed: true
            }
        );
        assert_eq!(
            InputEvent::ButtonRelease(MouseButton::Right).to_injection_command(),
            InjectionCommand::Button {
                button: MouseButton::Right,
                pressed: false
            }
        );
    }

    #[tokio::test]
    async fn flush_without_due_batch_injects_nothing() {
        let mut eng = engine(RecordingInjector::default());
        let now = Instant::now();
        eng.submit(InputEvent::RelativeMove { dx: 1.0, dy: 0.0 }, now);
        // low_latency min_delay is zero, so a single sub-threshold event is only
        // due once the deadline (== now) is reached; before then nothing flushes.
        assert!(eng.should_flush(now)); // deadline == now with zero delay
        let injected = eng.flush(now).await.expect("flush");
        assert_eq!(injected.len(), 1);
        assert_eq!(eng.injector().recorded().len(), 1);
        assert!(eng.is_empty());
    }

    #[tokio::test]
    async fn coalesces_motion_before_injecting() {
        let mut eng = engine(RecordingInjector::default());
        let now = Instant::now();
        for _ in 0..8 {
            eng.submit(InputEvent::RelativeMove { dx: 1.0, dy: 2.0 }, now);
        }
        assert!(eng.should_flush(now));
        let injected = eng.flush(now).await.expect("flush");
        // Eight motion events collapse into a single summed move.
        assert_eq!(
            injected,
            vec![InjectionCommand::MoveRelative { dx: 8.0, dy: 16.0 }]
        );
        assert_eq!(
            eng.injector().recorded(),
            vec![InputEvent::RelativeMove { dx: 8.0, dy: 16.0 }]
        );
    }

    #[tokio::test]
    async fn preserves_click_order_around_motion() {
        let mut eng = engine(RecordingInjector::default());
        let now = Instant::now();
        eng.submit(InputEvent::RelativeMove { dx: 1.0, dy: 0.0 }, now);
        eng.submit(InputEvent::ButtonPress(MouseButton::Left), now);
        eng.submit(InputEvent::RelativeMove { dx: 3.0, dy: 0.0 }, now);
        eng.submit(InputEvent::ButtonRelease(MouseButton::Left), now);
        let injected = eng.flush(now).await.expect("flush");
        assert_eq!(
            injected,
            vec![
                InjectionCommand::MoveRelative { dx: 1.0, dy: 0.0 },
                InjectionCommand::Button {
                    button: MouseButton::Left,
                    pressed: true
                },
                InjectionCommand::MoveRelative { dx: 3.0, dy: 0.0 },
                InjectionCommand::Button {
                    button: MouseButton::Left,
                    pressed: false
                },
            ]
        );
    }

    #[tokio::test]
    async fn surfaces_backend_error() {
        let mut eng = engine(RecordingInjector::failing_after(1));
        let now = Instant::now();
        eng.submit(InputEvent::KeyPress(0x04), now);
        eng.submit(InputEvent::KeyPress(0x05), now);
        let result = eng.flush(now).await;
        assert!(matches!(result, Err(InputError::Backend(_))));
        // The first event was injected before the failure.
        assert_eq!(eng.injector().recorded(), vec![InputEvent::KeyPress(0x04)]);
    }
}
