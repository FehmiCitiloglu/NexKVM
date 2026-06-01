//! Hot-reload tracking for plugins.
//!
//! Filesystem watching is platform/runtime integration. This module models the
//! decisions: detect changed artifacts, debounce repeated notifications, and
//! classify reload actions so the host can unload/reload safely.

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

/// Observed plugin artifact state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PluginArtifactState {
    /// Last modification time.
    pub modified: SystemTime,
    /// Artifact byte length.
    pub len: u64,
}

/// Hot reload decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReloadDecision {
    /// First observation of this plugin.
    TrackOnly,
    /// Artifact changed and should be reloaded.
    Reload { plugin_id: String },
    /// Change ignored due to debounce window or unchanged metadata.
    Ignore,
}

/// Pure hot-reload state machine.
#[derive(Debug, Clone)]
pub struct HotReloadTracker {
    debounce: Duration,
    last_seen: HashMap<String, PluginArtifactState>,
    last_reload: HashMap<String, SystemTime>,
}

impl HotReloadTracker {
    /// Create a tracker.
    #[must_use]
    pub fn new(debounce: Duration) -> Self {
        Self {
            debounce,
            last_seen: HashMap::new(),
            last_reload: HashMap::new(),
        }
    }

    /// Observe current artifact state and decide whether to reload.
    pub fn observe(
        &mut self,
        plugin_id: impl Into<String>,
        state: PluginArtifactState,
        now: SystemTime,
    ) -> ReloadDecision {
        let plugin_id = plugin_id.into();
        let Some(previous) = self.last_seen.insert(plugin_id.clone(), state) else {
            return ReloadDecision::TrackOnly;
        };

        if previous == state {
            return ReloadDecision::Ignore;
        }

        if self
            .last_reload
            .get(&plugin_id)
            .is_some_and(|last| now.duration_since(*last).unwrap_or_default() < self.debounce)
        {
            return ReloadDecision::Ignore;
        }

        self.last_reload.insert(plugin_id.clone(), now);
        ReloadDecision::Reload { plugin_id }
    }
}

impl Default for HotReloadTracker {
    fn default() -> Self {
        Self::new(Duration::from_millis(250))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(len: u64) -> PluginArtifactState {
        PluginArtifactState {
            modified: SystemTime::UNIX_EPOCH + Duration::from_secs(len),
            len,
        }
    }

    #[test]
    fn first_observation_only_tracks() {
        let mut tracker = HotReloadTracker::default();
        assert_eq!(
            tracker.observe("p", state(1), SystemTime::UNIX_EPOCH),
            ReloadDecision::TrackOnly
        );
    }

    #[test]
    fn changed_artifact_reloads_after_debounce() {
        let mut tracker = HotReloadTracker::new(Duration::from_millis(100));
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10);
        tracker.observe("p", state(1), now);
        assert_eq!(
            tracker.observe("p", state(2), now + Duration::from_millis(101)),
            ReloadDecision::Reload {
                plugin_id: "p".into()
            }
        );
    }

    #[test]
    fn debounce_suppresses_rapid_reloads() {
        let mut tracker = HotReloadTracker::new(Duration::from_secs(1));
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10);
        tracker.observe("p", state(1), now);
        assert!(matches!(
            tracker.observe("p", state(2), now + Duration::from_secs(2)),
            ReloadDecision::Reload { .. }
        ));
        assert_eq!(
            tracker.observe("p", state(3), now + Duration::from_secs(2)),
            ReloadDecision::Ignore
        );
    }
}
