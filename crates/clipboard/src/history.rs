//! Clipboard history.
//!
//! A bounded, most-recent-first ring of past selections. This powers the
//! "pick from your last N copies" UX and is the substrate for future
//! cross-device history and AI clipboard actions. It is pure, in-memory state;
//! durable/encrypted persistence is owned by the `storage` crate and wired in a
//! later phase.
//!
//! Behavior:
//! - **Dedup** — re-copying existing content moves that entry to the front
//!   instead of creating a duplicate.
//! - **Pinning** — pinned entries are never evicted by capacity pressure and
//!   survive [`clear`](ClipboardHistory::clear).
//! - **Privacy** — concealed/secret selections (password managers) are skipped
//!   unless explicitly allowed, and oversized payloads are not retained.

use std::collections::VecDeque;

use nexkvm_core::identity::DeviceId;

use crate::content::{ClipboardSnapshot, ContentFingerprint};

/// Tuning for a [`ClipboardHistory`].
#[derive(Debug, Clone)]
pub struct HistoryConfig {
    /// Maximum number of (unpinned) entries retained.
    pub capacity: usize,
    /// Skip entries whose total payload exceeds this many bytes.
    pub max_entry_bytes: usize,
    /// Retain concealed/secret content (default `false` for privacy).
    pub store_concealed: bool,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            capacity: 50,
            max_entry_bytes: 8 * 1024 * 1024,
            store_concealed: false,
        }
    }
}

/// One remembered clipboard selection.
#[derive(Debug, Clone)]
pub struct HistoryEntry {
    /// The captured multi-format selection.
    pub snapshot: ClipboardSnapshot,
    /// Device that produced it.
    pub origin: DeviceId,
    /// Wall-clock millis when recorded/last refreshed.
    pub at_millis: u64,
    /// Whether the user pinned this entry.
    pub pinned: bool,
    /// Cached fingerprint for dedup/lookup.
    fingerprint: ContentFingerprint,
}

impl HistoryEntry {
    /// The fingerprint identifying this entry's content.
    #[must_use]
    pub fn fingerprint(&self) -> ContentFingerprint {
        self.fingerprint
    }
}

/// Why a [`record`](ClipboardHistory::record) call did not store an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// The snapshot was empty.
    Empty,
    /// The snapshot was concealed/secret and `store_concealed` is off.
    Concealed,
    /// The snapshot exceeded `max_entry_bytes`.
    TooLarge,
}

/// A bounded, most-recent-first history of clipboard selections.
#[derive(Debug)]
pub struct ClipboardHistory {
    config: HistoryConfig,
    entries: VecDeque<HistoryEntry>,
}

impl ClipboardHistory {
    /// Create an empty history with the given configuration.
    #[must_use]
    pub fn new(config: HistoryConfig) -> Self {
        Self {
            config,
            entries: VecDeque::new(),
        }
    }

    /// Number of retained entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the history is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Entries, most-recent first.
    pub fn entries(&self) -> impl Iterator<Item = &HistoryEntry> {
        self.entries.iter()
    }

    /// The most recent entry, if any.
    #[must_use]
    pub fn most_recent(&self) -> Option<&HistoryEntry> {
        self.entries.front()
    }

    /// Record a selection at the front, applying dedup/privacy/eviction rules.
    ///
    /// Returns `Ok(fingerprint)` of the (re)inserted entry, or `Err(reason)` if
    /// it was deliberately not stored.
    ///
    /// # Errors
    /// Returns a [`SkipReason`] when the snapshot is empty, concealed, or too
    /// large to retain.
    pub fn record(
        &mut self,
        snapshot: ClipboardSnapshot,
        origin: DeviceId,
        at_millis: u64,
    ) -> Result<ContentFingerprint, SkipReason> {
        if snapshot.is_empty() {
            return Err(SkipReason::Empty);
        }
        if snapshot.is_concealed() && !self.config.store_concealed {
            return Err(SkipReason::Concealed);
        }
        if snapshot.total_len() > self.config.max_entry_bytes {
            return Err(SkipReason::TooLarge);
        }

        let fingerprint = snapshot.fingerprint();

        // Dedup: if the same content exists, move it to the front and refresh.
        if let Some(pos) = self
            .entries
            .iter()
            .position(|e| e.fingerprint == fingerprint)
        {
            let mut entry = self.entries.remove(pos).expect("position is valid");
            entry.at_millis = at_millis;
            entry.origin = origin;
            self.entries.push_front(entry);
            return Ok(fingerprint);
        }

        self.entries.push_front(HistoryEntry {
            snapshot,
            origin,
            at_millis,
            pinned: false,
            fingerprint,
        });
        self.evict();
        Ok(fingerprint)
    }

    /// Pin the entry with `fingerprint`, protecting it from eviction/clear.
    /// Returns whether an entry was found.
    pub fn pin(&mut self, fingerprint: ContentFingerprint) -> bool {
        self.set_pinned(fingerprint, true)
    }

    /// Unpin the entry with `fingerprint`. Returns whether an entry was found.
    pub fn unpin(&mut self, fingerprint: ContentFingerprint) -> bool {
        self.set_pinned(fingerprint, false)
    }

    /// Remove the entry with `fingerprint`. Returns whether one was removed.
    pub fn remove(&mut self, fingerprint: ContentFingerprint) -> bool {
        if let Some(pos) = self
            .entries
            .iter()
            .position(|e| e.fingerprint == fingerprint)
        {
            self.entries.remove(pos);
            true
        } else {
            false
        }
    }

    /// Clear all unpinned entries.
    pub fn clear(&mut self) {
        self.entries.retain(|e| e.pinned);
    }

    /// Case-insensitive substring search over the text rendering of entries,
    /// most-recent first.
    #[must_use]
    pub fn search(&self, query: &str) -> Vec<&HistoryEntry> {
        let needle = query.to_lowercase();
        self.entries
            .iter()
            .filter(|e| {
                e.snapshot
                    .best_text()
                    .is_some_and(|t| t.to_lowercase().contains(&needle))
            })
            .collect()
    }

    fn set_pinned(&mut self, fingerprint: ContentFingerprint, pinned: bool) -> bool {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|e| e.fingerprint == fingerprint)
        {
            entry.pinned = pinned;
            true
        } else {
            false
        }
    }

    /// Evict oldest unpinned entries until within capacity.
    fn evict(&mut self) {
        while self.entries.len() > self.config.capacity {
            // Find the oldest unpinned entry (search from the back/front age).
            let victim = self.entries.iter().rposition(|e| !e.pinned);
            match victim {
                Some(pos) => {
                    self.entries.remove(pos);
                }
                // Everything left is pinned — respect user intent, stop evicting.
                None => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::ClipboardContent;
    use bytes::Bytes;

    fn dev() -> DeviceId {
        DeviceId::generate()
    }

    #[test]
    fn records_most_recent_first() {
        let mut h = ClipboardHistory::new(HistoryConfig::default());
        h.record(ClipboardSnapshot::from_text("one"), dev(), 1)
            .unwrap();
        h.record(ClipboardSnapshot::from_text("two"), dev(), 2)
            .unwrap();
        assert_eq!(h.most_recent().unwrap().snapshot.best_text(), Some("two"));
        assert_eq!(h.len(), 2);
    }

    #[test]
    fn dedups_and_moves_to_front() {
        let mut h = ClipboardHistory::new(HistoryConfig::default());
        h.record(ClipboardSnapshot::from_text("a"), dev(), 1)
            .unwrap();
        h.record(ClipboardSnapshot::from_text("b"), dev(), 2)
            .unwrap();
        h.record(ClipboardSnapshot::from_text("a"), dev(), 3)
            .unwrap();
        assert_eq!(h.len(), 2);
        assert_eq!(h.most_recent().unwrap().snapshot.best_text(), Some("a"));
    }

    #[test]
    fn skips_concealed_and_empty() {
        let mut h = ClipboardHistory::new(HistoryConfig::default());
        let concealed = ClipboardSnapshot::new(vec![ClipboardContent {
            mime: "x-kde-passwordManagerHint".into(),
            data: Bytes::from_static(b"hunter2"),
        }]);
        assert_eq!(h.record(concealed, dev(), 1), Err(SkipReason::Concealed));
        assert_eq!(
            h.record(ClipboardSnapshot::new(vec![]), dev(), 1),
            Err(SkipReason::Empty)
        );
        assert!(h.is_empty());
    }

    #[test]
    fn evicts_oldest_unpinned_over_capacity() {
        let mut h = ClipboardHistory::new(HistoryConfig {
            capacity: 2,
            ..Default::default()
        });
        h.record(ClipboardSnapshot::from_text("a"), dev(), 1)
            .unwrap();
        h.record(ClipboardSnapshot::from_text("b"), dev(), 2)
            .unwrap();
        h.record(ClipboardSnapshot::from_text("c"), dev(), 3)
            .unwrap();
        assert_eq!(h.len(), 2);
        // "a" (oldest) evicted.
        let texts: Vec<_> = h.entries().filter_map(|e| e.snapshot.best_text()).collect();
        assert_eq!(texts, vec!["c", "b"]);
    }

    #[test]
    fn pinned_entries_survive_eviction_and_clear() {
        let mut h = ClipboardHistory::new(HistoryConfig {
            capacity: 1,
            ..Default::default()
        });
        let pin_fp = h
            .record(ClipboardSnapshot::from_text("keep"), dev(), 1)
            .unwrap();
        assert!(h.pin(pin_fp));
        h.record(ClipboardSnapshot::from_text("x"), dev(), 2)
            .unwrap();
        h.record(ClipboardSnapshot::from_text("y"), dev(), 3)
            .unwrap();
        // Pinned entry never evicted.
        assert!(h.entries().any(|e| e.snapshot.best_text() == Some("keep")));
        h.clear();
        assert_eq!(h.len(), 1);
        assert_eq!(h.most_recent().unwrap().snapshot.best_text(), Some("keep"));
    }

    #[test]
    fn search_matches_substring_case_insensitive() {
        let mut h = ClipboardHistory::new(HistoryConfig::default());
        h.record(ClipboardSnapshot::from_text("Hello World"), dev(), 1)
            .unwrap();
        h.record(ClipboardSnapshot::from_text("goodbye"), dev(), 2)
            .unwrap();
        let hits = h.search("world");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].snapshot.best_text(), Some("Hello World"));
    }
}
