//! Hover-driven screen preview cache and refresh policy.
//!
//! [`negotiate_screen_stream`](crate::negotiate_screen_stream) decides *how* a
//! preview stream is encoded; this module decides *when* a low-res thumbnail
//! should be (re)captured as the user hovers sources, and serves cached
//! thumbnails in between. It is the sans-IO UX glue behind "window preview on
//! hover": it debounces rapid hover sweeps, ages thumbnails out, serves a stale
//! thumbnail immediately while a fresh one is captured, and bounds memory with
//! an LRU cache.
//!
//! It owns no clock and no I/O — the caller passes a monotonic timestamp and
//! drives capture via the returned [`PreviewDecision`] (which carries a ready
//! [`ScreenStreamRequest`] for the existing negotiation path). Privacy and
//! permissions stay with negotiation/capture; this layer never bypasses them.

use std::collections::HashMap;

use crate::screen::{CaptureSource, CaptureSourceId, EncodedScreenFrame, ScreenStreamRequest};
use nexkvm_core::identity::DeviceId;

/// Tuning for thumbnail freshness, hover debounce, and cache size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreviewPolicy {
    /// A thumbnail older than this is considered stale and triggers a refresh.
    pub max_age_micros: u64,
    /// Minimum gap between capture requests for the *same* source; rapid hover
    /// within this window is debounced when no cache exists.
    pub debounce_micros: u64,
    /// Maximum cached thumbnails before least-recently-used eviction.
    pub max_entries: usize,
}

impl PreviewPolicy {
    /// Default hover-preview policy: 2 s freshness, 150 ms debounce, 32 entries.
    #[must_use]
    pub const fn preview_default() -> Self {
        Self {
            max_age_micros: 2_000_000,
            debounce_micros: 150_000,
            max_entries: 32,
        }
    }
}

impl Default for PreviewPolicy {
    fn default() -> Self {
        Self::preview_default()
    }
}

#[derive(Debug, Clone)]
struct CacheEntry {
    frame: EncodedScreenFrame,
    captured_at_micros: u64,
    last_access_micros: u64,
}

/// What the caller should do in response to a hover.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewDecision {
    /// A fresh cached thumbnail exists; show it, no capture needed.
    ServeCached(EncodedScreenFrame),
    /// No usable cache; capture a new thumbnail using this request.
    Refresh(Box<ScreenStreamRequest>),
    /// A stale thumbnail exists; show it now and capture a fresh one.
    ServeStaleAndRefresh {
        /// Stale thumbnail to display immediately.
        stale: EncodedScreenFrame,
        /// Request to capture a fresh thumbnail.
        request: Box<ScreenStreamRequest>,
    },
    /// Hover is sweeping too fast and no cache exists; do nothing this tick.
    Debounced,
}

/// Sans-IO controller that turns hover events into preview/capture decisions.
#[derive(Debug, Clone)]
pub struct HoverPreviewController {
    local_device: DeviceId,
    policy: PreviewPolicy,
    cache: HashMap<CaptureSourceId, CacheEntry>,
    last_hover_micros: HashMap<CaptureSourceId, u64>,
}

impl HoverPreviewController {
    /// Create a controller for the device that owns the capture sources.
    #[must_use]
    pub fn new(local_device: DeviceId, policy: PreviewPolicy) -> Self {
        Self {
            local_device,
            policy,
            cache: HashMap::new(),
            last_hover_micros: HashMap::new(),
        }
    }

    /// React to `viewer` hovering `source` at monotonic time `now_micros`.
    pub fn on_hover(
        &mut self,
        source: CaptureSource,
        viewer: DeviceId,
        now_micros: u64,
    ) -> PreviewDecision {
        let source_id = source_id_of(&source);
        let previous_hover = self.last_hover_micros.insert(source_id.clone(), now_micros);

        if let Some(entry) = self.cache.get_mut(&source_id) {
            entry.last_access_micros = now_micros;
            let age = now_micros.saturating_sub(entry.captured_at_micros);
            if age <= self.policy.max_age_micros {
                return PreviewDecision::ServeCached(entry.frame.clone());
            }
            let stale = entry.frame.clone();
            return PreviewDecision::ServeStaleAndRefresh {
                stale,
                request: Box::new(self.preview_request(source, viewer)),
            };
        }

        // No cache: debounce rapid hover sweeps so we don't capture on every
        // pixel of mouse travel across a source.
        if let Some(previous) = previous_hover
            && now_micros.saturating_sub(previous) < self.policy.debounce_micros
        {
            return PreviewDecision::Debounced;
        }

        PreviewDecision::Refresh(Box::new(self.preview_request(source, viewer)))
    }

    /// Store a freshly captured thumbnail for `source_id`, captured at
    /// `now_micros`. Evicts the least-recently-used entry if over capacity.
    pub fn store_thumbnail(
        &mut self,
        source_id: CaptureSourceId,
        frame: EncodedScreenFrame,
        now_micros: u64,
    ) {
        self.cache.insert(
            source_id,
            CacheEntry {
                frame,
                captured_at_micros: now_micros,
                last_access_micros: now_micros,
            },
        );
        self.evict_if_needed();
    }

    /// Drop a cached thumbnail (e.g. window closed or content changed).
    /// Returns whether an entry was removed.
    pub fn invalidate(&mut self, source_id: &CaptureSourceId) -> bool {
        self.last_hover_micros.remove(source_id);
        self.cache.remove(source_id).is_some()
    }

    /// Number of cached thumbnails.
    #[must_use]
    pub fn cached_count(&self) -> usize {
        self.cache.len()
    }

    fn preview_request(&self, source: CaptureSource, viewer: DeviceId) -> ScreenStreamRequest {
        ScreenStreamRequest::mini_preview(self.local_device, viewer, source)
    }

    fn evict_if_needed(&mut self) {
        while self.cache.len() > self.policy.max_entries {
            let Some(victim) = self
                .cache
                .iter()
                .min_by_key(|(_, entry)| entry.last_access_micros)
                .map(|(id, _)| id.clone())
            else {
                break;
            };
            self.cache.remove(&victim);
        }
    }
}

fn source_id_of(source: &CaptureSource) -> CaptureSourceId {
    match source {
        CaptureSource::Display { id, .. }
        | CaptureSource::Window { id, .. }
        | CaptureSource::Application { id, .. } => id.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::{ScreenCodec, ScreenFrameType, ScreenResolution};
    use crate::{FrameDependency, HardwareEncoder};
    use bytes::Bytes;

    fn window(id: &str) -> CaptureSource {
        CaptureSource::Window {
            id: CaptureSourceId::new(id),
            title: "Editor".into(),
            app_id: Some("com.example.editor".into()),
            visibility: crate::WindowVisibility::Visible,
        }
    }

    fn thumbnail(seq: u64) -> EncodedScreenFrame {
        EncodedScreenFrame {
            sequence: seq,
            capture_time_micros: seq,
            resolution: ScreenResolution::preview(),
            codec: ScreenCodec::H264,
            encoder: HardwareEncoder::Software,
            dependency: FrameDependency::Key,
            frame_type: ScreenFrameType::I,
            payload: Bytes::from(format!("thumb{seq}")),
        }
    }

    fn controller() -> HoverPreviewController {
        HoverPreviewController::new(DeviceId::generate(), PreviewPolicy::preview_default())
    }

    #[test]
    fn first_hover_requests_refresh() {
        let mut ctl = controller();
        let viewer = DeviceId::generate();
        let decision = ctl.on_hover(window("w1"), viewer, 1_000_000);
        let PreviewDecision::Refresh(request) = decision else {
            panic!("expected refresh on first hover");
        };
        assert_eq!(request.to, viewer);
        assert_eq!(request.quality, crate::ScreenQualityPreset::Preview);
    }

    #[test]
    fn fresh_cache_is_served_without_capture() {
        let mut ctl = controller();
        let viewer = DeviceId::generate();
        ctl.store_thumbnail(CaptureSourceId::new("w1"), thumbnail(1), 1_000_000);
        let decision = ctl.on_hover(window("w1"), viewer, 1_500_000);
        assert_eq!(decision, PreviewDecision::ServeCached(thumbnail(1)));
    }

    #[test]
    fn stale_cache_serves_and_refreshes() {
        let mut ctl = controller();
        let viewer = DeviceId::generate();
        ctl.store_thumbnail(CaptureSourceId::new("w1"), thumbnail(1), 1_000_000);
        // 3 s later (> 2 s max_age) the thumbnail is stale.
        let decision = ctl.on_hover(window("w1"), viewer, 4_000_000);
        let PreviewDecision::ServeStaleAndRefresh { stale, request } = decision else {
            panic!("expected stale-and-refresh");
        };
        assert_eq!(stale, thumbnail(1));
        assert_eq!(request.to, viewer);
    }

    #[test]
    fn rapid_hover_without_cache_is_debounced() {
        let mut ctl = controller();
        let viewer = DeviceId::generate();
        assert!(matches!(
            ctl.on_hover(window("w1"), viewer, 1_000_000),
            PreviewDecision::Refresh(_)
        ));
        // 50 ms later (< 150 ms debounce), still no cache → debounced.
        assert_eq!(
            ctl.on_hover(window("w1"), viewer, 1_050_000),
            PreviewDecision::Debounced
        );
    }

    #[test]
    fn invalidate_drops_cache() {
        let mut ctl = controller();
        ctl.store_thumbnail(CaptureSourceId::new("w1"), thumbnail(1), 1_000_000);
        assert!(ctl.invalidate(&CaptureSourceId::new("w1")));
        assert_eq!(ctl.cached_count(), 0);
        assert!(!ctl.invalidate(&CaptureSourceId::new("w1")));
    }

    #[test]
    fn lru_eviction_bounds_cache() {
        let policy = PreviewPolicy {
            max_entries: 2,
            ..PreviewPolicy::preview_default()
        };
        let mut ctl = HoverPreviewController::new(DeviceId::generate(), policy);
        ctl.store_thumbnail(CaptureSourceId::new("a"), thumbnail(1), 100);
        ctl.store_thumbnail(CaptureSourceId::new("b"), thumbnail(2), 200);
        // "a" is least-recently-used; inserting "c" evicts it.
        ctl.store_thumbnail(CaptureSourceId::new("c"), thumbnail(3), 300);
        assert_eq!(ctl.cached_count(), 2);
        assert!(ctl.invalidate(&CaptureSourceId::new("b")));
        assert!(ctl.invalidate(&CaptureSourceId::new("c")));
        assert!(!ctl.invalidate(&CaptureSourceId::new("a")));
    }
}
