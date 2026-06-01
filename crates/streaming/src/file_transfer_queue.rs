//! Background transfer queue and progress snapshots.

use std::collections::VecDeque;

use crate::file_transfer_types::{TransferId, TransferManifest};

/// Current lifecycle status of a queued transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueState {
    Queued,
    Running,
    Paused,
    Completed,
    Failed,
    Canceled,
}

/// UI-friendly transfer progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferProgress {
    /// Transfer identifier.
    pub id: TransferId,
    /// Bytes already sent/received.
    pub transferred_bytes: u64,
    /// Expected total bytes from manifest.
    pub total_bytes: u64,
    /// Current state.
    pub state: QueueState,
}

impl TransferProgress {
    /// Integer percent in `[0,100]`.
    #[must_use]
    pub fn percent(self) -> u8 {
        if self.total_bytes == 0 {
            return 100;
        }
        let p = self.transferred_bytes.saturating_mul(100) / self.total_bytes;
        u8::try_from(p.min(100)).unwrap_or(100)
    }
}

/// One queue item.
#[derive(Debug, Clone)]
pub struct QueuedTransfer {
    /// Transfer description.
    pub manifest: TransferManifest,
    /// Current transfer state.
    pub state: QueueState,
    /// Current byte progress.
    pub transferred_bytes: u64,
}

/// FIFO queue that allows one active transfer and many pending background jobs.
#[derive(Debug, Default)]
pub struct TransferQueue {
    active: Option<QueuedTransfer>,
    pending: VecDeque<QueuedTransfer>,
    done: VecDeque<QueuedTransfer>,
    max_done: usize,
}

impl TransferQueue {
    /// Create a queue retaining up to `max_done` finished entries.
    #[must_use]
    pub fn new(max_done: usize) -> Self {
        Self {
            active: None,
            pending: VecDeque::new(),
            done: VecDeque::new(),
            max_done,
        }
    }

    /// Enqueue a manifest.
    pub fn enqueue(&mut self, manifest: TransferManifest) {
        self.pending.push_back(QueuedTransfer {
            manifest,
            state: QueueState::Queued,
            transferred_bytes: 0,
        });
    }

    /// Start next queued transfer if none is active.
    pub fn start_next(&mut self) -> Option<TransferId> {
        if self.active.is_some() {
            return None;
        }
        let mut next = self.pending.pop_front()?;
        next.state = QueueState::Running;
        let id = next.manifest.id;
        self.active = Some(next);
        Some(id)
    }

    /// Pause the active transfer.
    pub fn pause_active(&mut self) -> Option<TransferId> {
        let mut active = self.active.take()?;
        active.state = QueueState::Paused;
        let id = active.manifest.id;
        self.pending.push_front(active);
        Some(id)
    }

    /// Mark active transfer progress in bytes.
    pub fn add_progress(&mut self, bytes: u64) -> Option<TransferProgress> {
        let active = self.active.as_mut()?;
        active.transferred_bytes = active
            .transferred_bytes
            .saturating_add(bytes)
            .min(active.manifest.total_bytes);
        Some(TransferProgress {
            id: active.manifest.id,
            transferred_bytes: active.transferred_bytes,
            total_bytes: active.manifest.total_bytes,
            state: active.state,
        })
    }

    /// Complete active transfer.
    pub fn complete_active(&mut self) -> Option<TransferId> {
        self.finish_active(QueueState::Completed)
    }

    /// Fail active transfer.
    pub fn fail_active(&mut self) -> Option<TransferId> {
        self.finish_active(QueueState::Failed)
    }

    /// Cancel active transfer.
    pub fn cancel_active(&mut self) -> Option<TransferId> {
        self.finish_active(QueueState::Canceled)
    }

    /// Progress for active transfer.
    #[must_use]
    pub fn active_progress(&self) -> Option<TransferProgress> {
        let a = self.active.as_ref()?;
        Some(TransferProgress {
            id: a.manifest.id,
            transferred_bytes: a.transferred_bytes,
            total_bytes: a.manifest.total_bytes,
            state: a.state,
        })
    }

    /// Number of waiting background jobs.
    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    fn finish_active(&mut self, final_state: QueueState) -> Option<TransferId> {
        let mut active = self.active.take()?;
        active.state = final_state;
        if final_state == QueueState::Completed {
            active.transferred_bytes = active.manifest.total_bytes;
        }
        let id = active.manifest.id;
        self.done.push_back(active);
        while self.done.len() > self.max_done {
            self.done.pop_front();
        }
        Some(id)
    }
}

#[cfg(test)]
mod tests {
    use coklu_core::identity::DeviceId;

    use super::*;
    use crate::file_transfer_types::{TransferEntry, TransferSource};

    fn manifest(bytes: u64) -> TransferManifest {
        TransferManifest::new(
            TransferId::generate(),
            DeviceId::generate(),
            None,
            TransferSource::DragDrop,
            vec![TransferEntry::file("a.bin", bytes).unwrap()],
        )
        .unwrap()
    }

    #[test]
    fn queue_runs_fifo_and_tracks_progress() {
        let mut q = TransferQueue::new(4);
        q.enqueue(manifest(100));
        q.enqueue(manifest(200));
        let first = q.start_next().unwrap();
        let p = q.add_progress(40).unwrap();
        assert_eq!(p.id, first);
        assert_eq!(p.percent(), 40);
        q.complete_active().unwrap();
        let second = q.start_next().unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn pause_puts_transfer_back_front() {
        let mut q = TransferQueue::new(2);
        q.enqueue(manifest(10));
        q.start_next();
        let paused = q.pause_active().unwrap();
        assert_eq!(q.pending_len(), 1);
        let resumed = q.start_next().unwrap();
        assert_eq!(paused, resumed);
    }
}
