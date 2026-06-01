//! Shared clipboard timeline.
//!
//! The timeline is the cross-device UX layer on top of [`ClipboardHistory`]: it
//! records trusted local/remote copies, tracks which devices should be able to
//! restore them, and returns explicit restore plans. It keeps the existing
//! privacy behavior from history: concealed selections are skipped by default,
//! oversized payloads are not retained, and bytes are still sealed before they
//! leave the clipboard crate.

use std::collections::HashMap;

use coklu_core::identity::DeviceId;

use crate::content::{ClipboardSnapshot, ContentFingerprint};
use crate::history::{ClipboardHistory, HistoryConfig, SkipReason};

/// Timeline configuration.
#[derive(Debug, Clone)]
pub struct TimelineConfig {
    /// Underlying history retention policy.
    pub history: HistoryConfig,
    /// Whether newly-recorded entries should be shared to trusted peers by default.
    pub share_new_entries: bool,
}

impl Default for TimelineConfig {
    fn default() -> Self {
        Self {
            history: HistoryConfig::default(),
            share_new_entries: true,
        }
    }
}

/// One timeline row, most-recent first.
#[derive(Debug, Clone)]
pub struct TimelineEntry {
    /// Entry fingerprint.
    pub fingerprint: ContentFingerprint,
    /// Clipboard snapshot.
    pub snapshot: ClipboardSnapshot,
    /// Device that produced the selection.
    pub origin: DeviceId,
    /// Record timestamp chosen by caller.
    pub at_millis: u64,
    /// Whether the entry is pinned.
    pub pinned: bool,
    /// Trusted devices this entry may be restored on.
    pub available_on: Vec<DeviceId>,
}

/// Plan to restore a timeline entry onto a target device.
#[derive(Debug, Clone)]
pub struct ClipboardRestorePlan {
    /// Target device whose pasteboard should be written.
    pub target: DeviceId,
    /// Snapshot to write.
    pub snapshot: ClipboardSnapshot,
    /// Fingerprint of the restored entry.
    pub fingerprint: ContentFingerprint,
}

/// In-memory shared clipboard timeline.
#[derive(Debug)]
pub struct SharedClipboardTimeline {
    history: ClipboardHistory,
    share_new_entries: bool,
    availability: HashMap<ContentFingerprint, Vec<DeviceId>>,
}

impl SharedClipboardTimeline {
    /// Create an empty timeline.
    #[must_use]
    pub fn new(config: TimelineConfig) -> Self {
        Self {
            history: ClipboardHistory::new(config.history),
            share_new_entries: config.share_new_entries,
            availability: HashMap::new(),
        }
    }

    /// Number of retained entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.history.len()
    }

    /// Whether the timeline is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.history.is_empty()
    }

    /// Record a copy from `origin` and optionally mark trusted peers as available.
    ///
    /// # Errors
    /// Returns [`SkipReason`] when privacy/size policy refuses the snapshot.
    pub fn record(
        &mut self,
        snapshot: ClipboardSnapshot,
        origin: DeviceId,
        trusted_peers: &[DeviceId],
        at_millis: u64,
    ) -> Result<ContentFingerprint, SkipReason> {
        let fingerprint = self.history.record(snapshot, origin, at_millis)?;
        let mut available_on = vec![origin];
        if self.share_new_entries {
            available_on.extend(trusted_peers.iter().copied());
        }
        dedup_devices(&mut available_on);
        self.availability.insert(fingerprint, available_on);
        self.retain_availability_for_history();
        Ok(fingerprint)
    }

    /// Mark an existing entry as available on `device`.
    pub fn share_to(&mut self, fingerprint: ContentFingerprint, device: DeviceId) -> bool {
        if !self.contains(fingerprint) {
            return false;
        }
        let devices = self.availability.entry(fingerprint).or_default();
        if !devices.contains(&device) {
            devices.push(device);
        }
        true
    }

    /// Pin an entry.
    pub fn pin(&mut self, fingerprint: ContentFingerprint) -> bool {
        self.history.pin(fingerprint)
    }

    /// Search timeline entries by text.
    #[must_use]
    pub fn search(&self, query: &str) -> Vec<TimelineEntry> {
        self.history
            .search(query)
            .into_iter()
            .map(|entry| self.timeline_entry(entry))
            .collect()
    }

    /// Entries, most-recent first.
    #[must_use]
    pub fn entries(&self) -> Vec<TimelineEntry> {
        self.history
            .entries()
            .map(|entry| self.timeline_entry(entry))
            .collect()
    }

    /// Build a restore plan if the entry exists and is available on `target`.
    #[must_use]
    pub fn restore_plan(
        &self,
        fingerprint: ContentFingerprint,
        target: DeviceId,
    ) -> Option<ClipboardRestorePlan> {
        let entry = self
            .history
            .entries()
            .find(|entry| entry.fingerprint() == fingerprint)?;
        self.availability
            .get(&fingerprint)
            .filter(|devices| devices.contains(&target))?;
        Some(ClipboardRestorePlan {
            target,
            snapshot: entry.snapshot.clone(),
            fingerprint,
        })
    }

    fn contains(&self, fingerprint: ContentFingerprint) -> bool {
        self.history
            .entries()
            .any(|entry| entry.fingerprint() == fingerprint)
    }

    fn timeline_entry(&self, entry: &crate::history::HistoryEntry) -> TimelineEntry {
        TimelineEntry {
            fingerprint: entry.fingerprint(),
            snapshot: entry.snapshot.clone(),
            origin: entry.origin,
            at_millis: entry.at_millis,
            pinned: entry.pinned,
            available_on: self
                .availability
                .get(&entry.fingerprint())
                .cloned()
                .unwrap_or_else(|| vec![entry.origin]),
        }
    }

    fn retain_availability_for_history(&mut self) {
        let fingerprints: Vec<_> = self
            .history
            .entries()
            .map(|entry| entry.fingerprint())
            .collect();
        self.availability
            .retain(|fingerprint, _| fingerprints.contains(fingerprint));
    }
}

impl Default for SharedClipboardTimeline {
    fn default() -> Self {
        Self::new(TimelineConfig::default())
    }
}

fn dedup_devices(devices: &mut Vec<DeviceId>) {
    let mut deduped = Vec::with_capacity(devices.len());
    for device in devices.drain(..) {
        if !deduped.contains(&device) {
            deduped.push(device);
        }
    }
    *devices = deduped;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_restores_for_trusted_peer() {
        let origin = DeviceId::generate();
        let peer = DeviceId::generate();
        let mut timeline = SharedClipboardTimeline::default();
        let fingerprint = timeline
            .record(ClipboardSnapshot::from_text("hello"), origin, &[peer], 10)
            .unwrap();

        let plan = timeline.restore_plan(fingerprint, peer).unwrap();
        assert_eq!(plan.target, peer);
        assert_eq!(plan.snapshot.best_text(), Some("hello"));
    }

    #[test]
    fn private_timeline_requires_explicit_share() {
        let origin = DeviceId::generate();
        let peer = DeviceId::generate();
        let mut timeline = SharedClipboardTimeline::new(TimelineConfig {
            share_new_entries: false,
            ..TimelineConfig::default()
        });
        let fingerprint = timeline
            .record(
                ClipboardSnapshot::from_text("secret-ish"),
                origin,
                &[peer],
                10,
            )
            .unwrap();
        assert!(timeline.restore_plan(fingerprint, peer).is_none());
        assert!(timeline.share_to(fingerprint, peer));
        assert!(timeline.restore_plan(fingerprint, peer).is_some());
    }
}
