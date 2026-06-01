//! End-to-end clipboard sync orchestration.
//!
//! [`ClipboardSync`] is sans-IO and [`Clipboard`] is the platform pasteboard
//! boundary; [`ClipboardEngine`] is the thin async glue that drives one against
//! the other and records a local [`ClipboardHistory`] along the way. It is the
//! piece the daemon owns per peer link:
//!
//! ```text
//! local copy detected ─▶ poll_local ─▶ (history) ─▶ ClipboardUpdate ─▶ transport
//! transport ─▶ apply_remote ─▶ ClipboardSync.open ─▶ (history) ─▶ Clipboard.write
//! ```
//!
//! The engine performs no I/O of its own beyond the injected [`Clipboard`]
//! reads/writes and never holds a lock across `.await`: each method awaits the
//! platform backend, then runs the pure sync/history steps synchronously.

use coklu_core::identity::DeviceId;

use crate::content::ClipboardSnapshot;
use crate::history::{ClipboardHistory, HistoryConfig};
use crate::sync::{ClipboardSync, ClipboardUpdate};
use crate::{Clipboard, ClipboardError};

/// Drives a platform [`Clipboard`] through a [`ClipboardSync`] state machine,
/// recording observed selections in a bounded [`ClipboardHistory`].
///
/// Generic over the platform backend so tests can inject a fake pasteboard.
#[derive(Debug)]
pub struct ClipboardEngine<C> {
    local: DeviceId,
    clipboard: C,
    sync: ClipboardSync,
    history: ClipboardHistory,
}

impl<C: Clipboard> ClipboardEngine<C> {
    /// Build an engine for `local` over `clipboard`, driving `sync` and a
    /// history sized by `history`.
    #[must_use]
    pub fn new(local: DeviceId, clipboard: C, sync: ClipboardSync, history: HistoryConfig) -> Self {
        Self {
            local,
            clipboard,
            sync,
            history: ClipboardHistory::new(history),
        }
    }

    /// Borrow the recorded history (most-recent first).
    #[must_use]
    pub fn history(&self) -> &ClipboardHistory {
        &self.history
    }

    /// Read the local pasteboard and, if it changed, produce the outbound
    /// update to broadcast to peers.
    ///
    /// Returns `Ok(None)` when the pasteboard is empty or the change is an echo
    /// of content the engine just applied (loop prevention via [`ClipboardSync`]).
    /// Non-concealed selections are recorded to history regardless of whether
    /// they are broadcast.
    ///
    /// # Errors
    /// Returns [`ClipboardError`] on backend read failure or seal/encode errors.
    pub async fn poll_local(
        &mut self,
        now_millis: u64,
    ) -> Result<Option<ClipboardUpdate>, ClipboardError> {
        let Some(snapshot) = self.clipboard.read().await? else {
            return Ok(None);
        };
        let update = self.sync.prepare_outbound(&snapshot, now_millis)?;
        if update.is_some() {
            // Only record selections we actually originate/broadcast; echoes are
            // suppressed above and were already recorded when first applied.
            let _ = self.history.record(snapshot, self.local, now_millis);
        }
        Ok(update)
    }

    /// Apply an inbound update from a peer: decrypt/decode it, write the
    /// resulting selection to the local pasteboard, and record it in history.
    ///
    /// Returns `Ok(false)` when the update was stale or an echo and nothing was
    /// applied.
    ///
    /// # Errors
    /// Returns [`ClipboardError`] on open/decompress/decode failure (e.g. a
    /// forged or corrupt message) or on backend write failure.
    pub async fn apply_remote(
        &mut self,
        update: ClipboardUpdate,
        now_millis: u64,
    ) -> Result<bool, ClipboardError> {
        let origin = update.origin;
        let Some(snapshot) = self.sync.accept_inbound(update)? else {
            return Ok(false);
        };
        self.write_applied(snapshot, origin, now_millis).await?;
        Ok(true)
    }

    async fn write_applied(
        &mut self,
        snapshot: ClipboardSnapshot,
        origin: DeviceId,
        now_millis: u64,
    ) -> Result<(), ClipboardError> {
        self.clipboard.write(snapshot.clone()).await?;
        let _ = self.history.record(snapshot, origin, now_millis);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cipher::PlaintextCipher;
    use crate::content::ClipboardContent;
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// In-memory fake pasteboard.
    #[derive(Debug, Default)]
    struct FakeClipboard {
        slot: Mutex<Option<ClipboardSnapshot>>,
    }

    impl FakeClipboard {
        fn loaded(snapshot: ClipboardSnapshot) -> Self {
            Self {
                slot: Mutex::new(Some(snapshot)),
            }
        }

        fn current(&self) -> Option<ClipboardSnapshot> {
            self.slot.lock().expect("poisoned").clone()
        }

        fn set(&self, snapshot: Option<ClipboardSnapshot>) {
            *self.slot.lock().expect("poisoned") = snapshot;
        }
    }

    #[async_trait]
    impl Clipboard for FakeClipboard {
        async fn read(&self) -> Result<Option<ClipboardSnapshot>, ClipboardError> {
            Ok(self.current())
        }

        async fn write(&self, snapshot: ClipboardSnapshot) -> Result<(), ClipboardError> {
            self.set(Some(snapshot));
            Ok(())
        }
    }

    fn engine(local: DeviceId, clip: FakeClipboard) -> ClipboardEngine<FakeClipboard> {
        let sync = ClipboardSync::new(local, Box::new(PlaintextCipher));
        ClipboardEngine::new(local, clip, sync, HistoryConfig::default())
    }

    #[tokio::test]
    async fn empty_clipboard_yields_no_update() {
        let dev = DeviceId::generate();
        let mut eng = engine(dev, FakeClipboard::default());
        assert!(eng.poll_local(1).await.unwrap().is_none());
        assert!(eng.history().is_empty());
    }

    #[tokio::test]
    async fn local_copy_produces_update_and_records_history() {
        let dev = DeviceId::generate();
        let snap = ClipboardSnapshot::from_text("hello");
        let mut eng = engine(dev, FakeClipboard::loaded(snap.clone()));

        let update = eng.poll_local(10).await.unwrap().expect("broadcast");
        assert_eq!(update.origin, dev);
        assert_eq!(eng.history().len(), 1);

        // Re-polling the unchanged pasteboard is suppressed (no duplicate).
        assert!(eng.poll_local(11).await.unwrap().is_none());
        assert_eq!(eng.history().len(), 1);
    }

    #[tokio::test]
    async fn remote_update_is_applied_to_pasteboard() {
        let dev_a = DeviceId::generate();
        let dev_b = DeviceId::generate();

        // A copies rich + image content.
        let snap = ClipboardSnapshot::new(vec![
            ClipboardContent::text("hi"),
            ClipboardContent::html("<i>hi</i>"),
            ClipboardContent::image_png(vec![0x89, 0x50, 0x4e, 0x47]),
        ]);
        let mut a = engine(dev_a, FakeClipboard::loaded(snap.clone()));
        let update = a.poll_local(1).await.unwrap().expect("broadcast");

        // B applies it: pasteboard updated, history recorded with A's origin.
        let clip_b = FakeClipboard::default();
        let mut b = engine(dev_b, clip_b);
        assert!(b.apply_remote(update, 2).await.unwrap());
        assert_eq!(b.clipboard.current(), Some(snap.clone()));
        assert_eq!(b.history().most_recent().unwrap().origin, dev_a);
    }

    #[tokio::test]
    async fn applied_remote_is_not_rebroadcast() {
        let dev_a = DeviceId::generate();
        let dev_b = DeviceId::generate();

        let snap = ClipboardSnapshot::from_text("ping");
        let mut a = engine(dev_a, FakeClipboard::loaded(snap.clone()));
        let update = a.poll_local(1).await.unwrap().unwrap();

        let mut b = engine(dev_b, FakeClipboard::default());
        assert!(b.apply_remote(update, 2).await.unwrap());

        // B's watcher now observes the applied content; it must not loop it back.
        assert!(b.poll_local(3).await.unwrap().is_none());
    }
}
