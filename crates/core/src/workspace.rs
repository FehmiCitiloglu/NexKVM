//! Shared workspace control-plane model.
//!
//! This module owns the platform-neutral shape for coklu's shared workspace:
//! unified virtual desktops, cross-device window snapping, app launching,
//! global search, shared workspace memory, and spatial navigation. It performs
//! no OS calls. Window enumeration, app launch, and desktop search are provided
//! by `platform-*` crates through async traits so native APIs, permission
//! prompts, and any `unsafe` FFI stay behind safe boundaries.
//!
//! Security posture: remote app launch, search, and memory sharing must be
//! enabled by policy and backed by trusted, authenticated devices. Transport is
//! still owned by the network/session layer and must remain encrypted.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::identity::DeviceId;

/// Errors surfaced by shared workspace planning and platform backends.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum WorkspaceError {
    /// The requested target/source is not known in the virtual desktop.
    #[error("workspace target not found: {0}")]
    NotFound(String),

    /// Local policy or peer policy denied the request.
    #[error("workspace permission denied: {0}")]
    PermissionDenied(&'static str),

    /// Platform backend failed.
    #[error("workspace backend error: {0}")]
    Backend(String),

    /// No viable spatial route or snap target exists.
    #[error("workspace route unavailable: {0}")]
    RouteUnavailable(&'static str),

    /// Invalid workspace model input.
    #[error("invalid workspace input: {0}")]
    InvalidInput(&'static str),
}

/// Integer point in the shared virtual desktop coordinate space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspacePoint {
    /// Horizontal coordinate in virtual pixels.
    pub x: i32,
    /// Vertical coordinate in virtual pixels.
    pub y: i32,
}

impl WorkspacePoint {
    /// Construct a point.
    #[must_use]
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

/// Rectangle in the shared virtual desktop coordinate space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceRect {
    /// Left coordinate.
    pub x: i32,
    /// Top coordinate.
    pub y: i32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl WorkspaceRect {
    /// Construct a rectangle.
    #[must_use]
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Left edge.
    #[must_use]
    pub const fn left(self) -> i32 {
        self.x
    }

    /// Top edge.
    #[must_use]
    pub const fn top(self) -> i32 {
        self.y
    }

    /// Right edge.
    #[must_use]
    pub fn right(self) -> i32 {
        self.x.saturating_add(self.width as i32)
    }

    /// Bottom edge.
    #[must_use]
    pub fn bottom(self) -> i32 {
        self.y.saturating_add(self.height as i32)
    }

    /// Center point.
    #[must_use]
    pub fn center(self) -> WorkspacePoint {
        WorkspacePoint::new(
            self.x.saturating_add((self.width / 2) as i32),
            self.y.saturating_add((self.height / 2) as i32),
        )
    }

    /// Whether this rectangle contains `point`.
    #[must_use]
    pub fn contains(self, point: WorkspacePoint) -> bool {
        point.x >= self.left()
            && point.x < self.right()
            && point.y >= self.top()
            && point.y < self.bottom()
    }

    fn clamped_inside(self, bounds: Self) -> Self {
        let width = self.width.min(bounds.width);
        let height = self.height.min(bounds.height);
        let max_x = bounds.right().saturating_sub(width as i32);
        let max_y = bounds.bottom().saturating_sub(height as i32);
        Self::new(
            self.x.clamp(bounds.left(), max_x),
            self.y.clamp(bounds.top(), max_y),
            width,
            height,
        )
    }
}

/// Device rectangle inside the unified virtual desktop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceDevice {
    /// Device id.
    pub device: DeviceId,
    /// Human-readable label.
    pub label: String,
    /// Device desktop bounds in virtual coordinates.
    pub bounds: WorkspaceRect,
    /// Whether this device is currently connected.
    pub online: bool,
}

impl WorkspaceDevice {
    /// Construct a workspace device.
    #[must_use]
    pub fn new(device: DeviceId, label: impl Into<String>, bounds: WorkspaceRect) -> Self {
        Self {
            device,
            label: label.into(),
            bounds,
            online: false,
        }
    }

    /// Mark current connectivity.
    #[must_use]
    pub const fn with_online(mut self, online: bool) -> Self {
        self.online = online;
        self
    }
}

/// Unified virtual desktop containing all trusted devices.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnifiedVirtualDesktop {
    devices: Vec<WorkspaceDevice>,
}

impl UnifiedVirtualDesktop {
    /// Create an empty desktop.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// All devices in editor order.
    #[must_use]
    pub fn devices(&self) -> &[WorkspaceDevice] {
        &self.devices
    }

    /// Get one device.
    #[must_use]
    pub fn device(&self, device: DeviceId) -> Option<&WorkspaceDevice> {
        self.devices.iter().find(|entry| entry.device == device)
    }

    /// Add or replace a device.
    pub fn upsert(&mut self, device: WorkspaceDevice) {
        if let Some(existing) = self
            .devices
            .iter_mut()
            .find(|entry| entry.device == device.device)
        {
            *existing = device;
        } else {
            self.devices.push(device);
        }
    }

    /// Device containing a point.
    #[must_use]
    pub fn device_at(&self, point: WorkspacePoint) -> Option<&WorkspaceDevice> {
        self.devices
            .iter()
            .filter(|device| device.online)
            .find(|device| device.bounds.contains(point))
    }

    /// Bounding box containing all known devices.
    #[must_use]
    pub fn bounds(&self) -> Option<WorkspaceRect> {
        let mut devices = self.devices.iter();
        let first = devices.next()?;
        let mut min_x = first.bounds.left();
        let mut min_y = first.bounds.top();
        let mut max_x = first.bounds.right();
        let mut max_y = first.bounds.bottom();
        for device in devices {
            min_x = min_x.min(device.bounds.left());
            min_y = min_y.min(device.bounds.top());
            max_x = max_x.max(device.bounds.right());
            max_y = max_y.max(device.bounds.bottom());
        }
        Some(WorkspaceRect::new(
            min_x,
            min_y,
            (max_x - min_x) as u32,
            (max_y - min_y) as u32,
        ))
    }
}

/// Stable platform window id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WindowId(pub String);

impl WindowId {
    /// Construct a window id.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

/// Stable platform application id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AppId(pub String);

impl AppId {
    /// Construct an application id.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

/// Snapshot of a window participating in shared workspace UX.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowSnapshot {
    /// Window id on the owning device.
    pub id: WindowId,
    /// Owning device.
    pub device: DeviceId,
    /// Title reported by the platform.
    pub title: String,
    /// Owning app id when known.
    pub app_id: Option<AppId>,
    /// Bounds in the unified virtual desktop.
    pub bounds: WorkspaceRect,
    /// Whether the platform reports this window as visible.
    pub visible: bool,
}

/// Snap direction requested by the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SnapDirection {
    /// Snap to the left half of the current/neighbor desktop.
    Left,
    /// Snap to the right half of the current/neighbor desktop.
    Right,
    /// Snap to the top half of the current/neighbor desktop.
    Up,
    /// Snap to the bottom half of the current/neighbor desktop.
    Down,
}

/// Cross-device window snap decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowSnapPlan {
    /// Window being moved.
    pub window: WindowId,
    /// Source device.
    pub from: DeviceId,
    /// Destination device.
    pub to: DeviceId,
    /// Destination rectangle.
    pub target_bounds: WorkspaceRect,
    /// Whether crossing devices requires a platform handoff/reopen.
    pub cross_device: bool,
}

/// Compute a window snap target in the unified virtual desktop.
///
/// If the snap direction points at an adjacent online device, the target lands
/// on that device. Otherwise it snaps within the current device.
///
/// # Errors
/// Returns [`WorkspaceError`] if the window device is unknown/offline.
pub fn plan_window_snap(
    desktop: &UnifiedVirtualDesktop,
    window: &WindowSnapshot,
    direction: SnapDirection,
) -> Result<WindowSnapPlan, WorkspaceError> {
    let current = desktop
        .device(window.device)
        .filter(|device| device.online)
        .ok_or_else(|| WorkspaceError::NotFound(window.device.to_string()))?;
    let target = neighbor_for_direction(desktop, current, direction).unwrap_or(current);
    let target_bounds = snap_rect_for(target.bounds, direction).clamped_inside(target.bounds);
    Ok(WindowSnapPlan {
        window: window.id.clone(),
        from: current.device,
        to: target.device,
        target_bounds,
        cross_device: current.device != target.device,
    })
}

fn snap_rect_for(bounds: WorkspaceRect, direction: SnapDirection) -> WorkspaceRect {
    match direction {
        SnapDirection::Left => {
            WorkspaceRect::new(bounds.left(), bounds.top(), bounds.width / 2, bounds.height)
        }
        SnapDirection::Right => WorkspaceRect::new(
            bounds.left().saturating_add((bounds.width / 2) as i32),
            bounds.top(),
            bounds.width - bounds.width / 2,
            bounds.height,
        ),
        SnapDirection::Up => {
            WorkspaceRect::new(bounds.left(), bounds.top(), bounds.width, bounds.height / 2)
        }
        SnapDirection::Down => WorkspaceRect::new(
            bounds.left(),
            bounds.top().saturating_add((bounds.height / 2) as i32),
            bounds.width,
            bounds.height - bounds.height / 2,
        ),
    }
}

fn neighbor_for_direction<'a>(
    desktop: &'a UnifiedVirtualDesktop,
    current: &WorkspaceDevice,
    direction: SnapDirection,
) -> Option<&'a WorkspaceDevice> {
    desktop
        .devices()
        .iter()
        .filter(|device| device.online && device.device != current.device)
        .filter_map(|device| {
            directional_distance(current.bounds, device.bounds, direction)
                .map(|distance| (distance, device))
        })
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, device)| device)
}

fn directional_distance(
    from: WorkspaceRect,
    to: WorkspaceRect,
    direction: SnapDirection,
) -> Option<u32> {
    let from_center = from.center();
    let to_center = to.center();
    match direction {
        SnapDirection::Left if to.right() <= from.left() => Some((from.left() - to.right()) as u32),
        SnapDirection::Right if to.left() >= from.right() => {
            Some((to.left() - from.right()) as u32)
        }
        SnapDirection::Up if to.bottom() <= from.top() => Some((from.top() - to.bottom()) as u32),
        SnapDirection::Down if to.top() >= from.bottom() => Some((to.top() - from.bottom()) as u32),
        SnapDirection::Left | SnapDirection::Right
            if vertical_overlap(from, to)
                && horizontal_direction_matches(from_center, to_center, direction) =>
        {
            Some((from_center.x - to_center.x).unsigned_abs())
        }
        SnapDirection::Up | SnapDirection::Down
            if horizontal_overlap(from, to)
                && vertical_direction_matches(from_center, to_center, direction) =>
        {
            Some((from_center.y - to_center.y).unsigned_abs())
        }
        _ => None,
    }
}

fn horizontal_direction_matches(
    from: WorkspacePoint,
    to: WorkspacePoint,
    direction: SnapDirection,
) -> bool {
    matches!(direction, SnapDirection::Left) && to.x < from.x
        || matches!(direction, SnapDirection::Right) && to.x > from.x
}

fn vertical_direction_matches(
    from: WorkspacePoint,
    to: WorkspacePoint,
    direction: SnapDirection,
) -> bool {
    matches!(direction, SnapDirection::Up) && to.y < from.y
        || matches!(direction, SnapDirection::Down) && to.y > from.y
}

fn horizontal_overlap(a: WorkspaceRect, b: WorkspaceRect) -> bool {
    a.left().max(b.left()) < a.right().min(b.right())
}

fn vertical_overlap(a: WorkspaceRect, b: WorkspaceRect) -> bool {
    a.top().max(b.top()) < a.bottom().min(b.bottom())
}

/// App descriptor from a platform backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplicationDescriptor {
    /// Stable app id.
    pub id: AppId,
    /// Owning device.
    pub device: DeviceId,
    /// User-facing name.
    pub name: String,
    /// Whether remote launch is allowed by local policy/platform.
    pub launchable: bool,
}

/// Remote/local app launch request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppLaunchRequest {
    /// Device that should launch the app.
    pub target: DeviceId,
    /// App id to launch.
    pub app: AppId,
    /// Optional arguments. Platform backends may reject arguments by policy.
    pub args: Vec<String>,
    /// Whether this request came from a remote trusted device.
    pub remote: bool,
}

/// App launch result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppLaunchOutcome {
    /// Target device.
    pub device: DeviceId,
    /// Launched app.
    pub app: AppId,
    /// Window id if the platform can report it immediately.
    pub window: Option<WindowId>,
}

/// Search item category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SearchKind {
    /// Application result.
    Application,
    /// Open window result.
    Window,
    /// File or folder result.
    File,
    /// Clipboard/history result.
    Clipboard,
    /// Shared workspace memory result.
    WorkspaceMemory,
    /// Setting/action result.
    Setting,
}

/// Global search query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchQuery {
    /// Query text.
    pub text: String,
    /// Devices to include; empty means all trusted devices allowed by policy.
    pub devices: Vec<DeviceId>,
    /// Result kinds to include; empty means all kinds allowed by policy.
    pub kinds: Vec<SearchKind>,
    /// Maximum result count.
    pub limit: usize,
}

impl SearchQuery {
    /// Construct a query with a conservative result limit.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            devices: Vec::new(),
            kinds: Vec::new(),
            limit: 20,
        }
    }

    fn allows_device(&self, device: DeviceId) -> bool {
        self.devices.is_empty() || self.devices.contains(&device)
    }

    fn allows_kind(&self, kind: SearchKind) -> bool {
        self.kinds.is_empty() || self.kinds.contains(&kind)
    }
}

/// One global search result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchResult {
    /// Device that owns the result.
    pub device: DeviceId,
    /// Result kind.
    pub kind: SearchKind,
    /// Stable local id for activation.
    pub id: String,
    /// Primary label.
    pub title: String,
    /// Secondary context.
    pub subtitle: Option<String>,
    /// Higher scores sort first.
    pub score: u16,
}

/// Shared memory visibility policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemoryVisibility {
    /// Visible to all trusted devices in the workspace.
    TrustedWorkspace,
    /// Visible only to specific devices.
    Devices(Vec<DeviceId>),
    /// Local-only memory; useful before sync is enabled.
    LocalOnly,
}

impl MemoryVisibility {
    fn permits(&self, viewer: DeviceId, owner: DeviceId) -> bool {
        match self {
            Self::TrustedWorkspace => true,
            Self::Devices(devices) => viewer == owner || devices.contains(&viewer),
            Self::LocalOnly => viewer == owner,
        }
    }
}

/// A durable shared memory entry for the workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceMemoryEntry {
    /// Stable key.
    pub key: String,
    /// Owning/source device.
    pub device: DeviceId,
    /// Title.
    pub title: String,
    /// Body text.
    pub body: String,
    /// Tags used by global search.
    pub tags: Vec<String>,
    /// Visibility policy.
    pub visibility: MemoryVisibility,
    /// Monotonic or wall-clock update timestamp chosen by caller.
    pub updated_at_millis: u64,
}

/// In-memory index for workspace memory entries.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedWorkspaceMemory {
    entries: Vec<WorkspaceMemoryEntry>,
}

impl SharedWorkspaceMemory {
    /// Create an empty memory index.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// All entries in insertion/update order.
    #[must_use]
    pub fn entries(&self) -> &[WorkspaceMemoryEntry] {
        &self.entries
    }

    /// Add or replace an entry by key.
    pub fn upsert(&mut self, entry: WorkspaceMemoryEntry) -> Result<(), WorkspaceError> {
        if entry.key.is_empty() {
            return Err(WorkspaceError::InvalidInput("memory key cannot be empty"));
        }
        if let Some(existing) = self
            .entries
            .iter_mut()
            .find(|existing| existing.key == entry.key)
        {
            *existing = entry;
        } else {
            self.entries.push(entry);
        }
        Ok(())
    }

    /// Remove an entry by key.
    pub fn remove(&mut self, key: &str) -> bool {
        if let Some(index) = self.entries.iter().position(|entry| entry.key == key) {
            self.entries.remove(index);
            true
        } else {
            false
        }
    }

    /// Search visible memory entries.
    #[must_use]
    pub fn search(&self, query: &SearchQuery, viewer: DeviceId) -> Vec<SearchResult> {
        let needle = query.text.to_ascii_lowercase();
        let mut results: Vec<_> = self
            .entries
            .iter()
            .filter(|entry| query.allows_device(entry.device))
            .filter(|_| query.allows_kind(SearchKind::WorkspaceMemory))
            .filter(|entry| entry.visibility.permits(viewer, entry.device))
            .filter_map(|entry| memory_match(entry, &needle))
            .collect();
        results.sort_by_key(|result| std::cmp::Reverse(result.score));
        results.truncate(query.limit);
        results
    }
}

fn memory_match(entry: &WorkspaceMemoryEntry, needle: &str) -> Option<SearchResult> {
    if needle.is_empty() {
        return Some(memory_result(entry, 1));
    }
    let title = entry.title.to_ascii_lowercase();
    let body = entry.body.to_ascii_lowercase();
    let tag_hit = entry
        .tags
        .iter()
        .any(|tag| tag.to_ascii_lowercase().contains(needle));
    let score = if title.contains(needle) {
        100
    } else if tag_hit {
        75
    } else if body.contains(needle) {
        50
    } else {
        return None;
    };
    Some(memory_result(entry, score))
}

fn memory_result(entry: &WorkspaceMemoryEntry, score: u16) -> SearchResult {
    SearchResult {
        device: entry.device,
        kind: SearchKind::WorkspaceMemory,
        id: entry.key.clone(),
        title: entry.title.clone(),
        subtitle: Some(entry.body.clone()),
        score,
    }
}

/// Spatial navigation target selected from the unified desktop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpatialNavigationTarget {
    /// Destination device.
    pub device: DeviceId,
    /// Suggested landing point.
    pub point: WorkspacePoint,
}

/// Spatial navigation planner for keyboard/gesture/focus movement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpatialNavigator {
    desktop: UnifiedVirtualDesktop,
}

impl SpatialNavigator {
    /// Construct a navigator from a virtual desktop snapshot.
    #[must_use]
    pub fn new(desktop: UnifiedVirtualDesktop) -> Self {
        Self { desktop }
    }

    /// Navigate from a point toward the next online device in `direction`.
    #[must_use]
    pub fn navigate(
        &self,
        from: WorkspacePoint,
        direction: SnapDirection,
    ) -> Option<SpatialNavigationTarget> {
        let current = self.desktop.device_at(from)?;
        let next = neighbor_for_direction(&self.desktop, current, direction)?;
        Some(SpatialNavigationTarget {
            device: next.device,
            point: next.bounds.center(),
        })
    }
}

/// Platform app/window operations for shared workspace features.
#[async_trait]
pub trait WorkspaceBackend: Send + Sync {
    /// List applications that may participate in shared launch/search.
    ///
    /// # Errors
    /// Returns [`WorkspaceError`] on platform enumeration failure.
    async fn list_applications(&self) -> Result<Vec<ApplicationDescriptor>, WorkspaceError>;

    /// List windows that may participate in snapping/search.
    ///
    /// # Errors
    /// Returns [`WorkspaceError`] on platform enumeration failure.
    async fn list_windows(&self) -> Result<Vec<WindowSnapshot>, WorkspaceError>;

    /// Launch an app locally or on behalf of a trusted peer.
    ///
    /// # Errors
    /// Returns [`WorkspaceError::PermissionDenied`] when remote launch is not
    /// allowed by policy, or [`WorkspaceError::Backend`] for OS failures.
    async fn launch_app(
        &self,
        request: AppLaunchRequest,
    ) -> Result<AppLaunchOutcome, WorkspaceError>;

    /// Apply a local window snap/move.
    ///
    /// # Errors
    /// Returns [`WorkspaceError`] if the platform cannot move the target window.
    async fn apply_window_snap(&self, plan: WindowSnapPlan) -> Result<(), WorkspaceError>;
}

/// Platform or remote provider for global search.
#[async_trait]
pub trait WorkspaceSearchProvider: Send + Sync {
    /// Search this provider.
    ///
    /// # Errors
    /// Returns [`WorkspaceError`] on backend or policy failure.
    async fn search(&self, query: SearchQuery) -> Result<Vec<SearchResult>, WorkspaceError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desktop_pair() -> (UnifiedVirtualDesktop, DeviceId, DeviceId) {
        let left = DeviceId::generate();
        let right = DeviceId::generate();
        let mut desktop = UnifiedVirtualDesktop::new();
        desktop.upsert(
            WorkspaceDevice::new(left, "Left", WorkspaceRect::new(0, 0, 1000, 800))
                .with_online(true),
        );
        desktop.upsert(
            WorkspaceDevice::new(right, "Right", WorkspaceRect::new(1000, 0, 1200, 900))
                .with_online(true),
        );
        (desktop, left, right)
    }

    #[test]
    fn unified_desktop_finds_device_at_point() {
        let (desktop, left, right) = desktop_pair();
        assert_eq!(
            desktop
                .device_at(WorkspacePoint::new(10, 10))
                .unwrap()
                .device,
            left
        );
        assert_eq!(
            desktop
                .device_at(WorkspacePoint::new(1100, 10))
                .unwrap()
                .device,
            right
        );
        assert_eq!(
            desktop.bounds().unwrap(),
            WorkspaceRect::new(0, 0, 2200, 900)
        );
    }

    #[test]
    fn snap_moves_window_to_neighbor_device() {
        let (desktop, left, right) = desktop_pair();
        let window = WindowSnapshot {
            id: WindowId::new("w1"),
            device: left,
            title: "Editor".into(),
            app_id: Some(AppId::new("dev.editor")),
            bounds: WorkspaceRect::new(100, 100, 600, 400),
            visible: true,
        };

        let plan = plan_window_snap(&desktop, &window, SnapDirection::Right).unwrap();
        assert_eq!(plan.to, right);
        assert!(plan.cross_device);
        assert_eq!(plan.target_bounds, WorkspaceRect::new(1600, 0, 600, 900));
    }

    #[test]
    fn snap_without_neighbor_stays_on_current_device() {
        let (desktop, left, _) = desktop_pair();
        let window = WindowSnapshot {
            id: WindowId::new("w1"),
            device: left,
            title: "Editor".into(),
            app_id: None,
            bounds: WorkspaceRect::new(100, 100, 600, 400),
            visible: true,
        };

        let plan = plan_window_snap(&desktop, &window, SnapDirection::Left).unwrap();
        assert_eq!(plan.to, left);
        assert!(!plan.cross_device);
        assert_eq!(plan.target_bounds, WorkspaceRect::new(0, 0, 500, 800));
    }

    #[test]
    fn shared_memory_search_respects_visibility() {
        let owner = DeviceId::generate();
        let viewer = DeviceId::generate();
        let other = DeviceId::generate();
        let mut memory = SharedWorkspaceMemory::new();
        memory
            .upsert(WorkspaceMemoryEntry {
                key: "project".into(),
                device: owner,
                title: "Project Plan".into(),
                body: "Launch checklist".into(),
                tags: vec!["workspace".into()],
                visibility: MemoryVisibility::Devices(vec![viewer]),
                updated_at_millis: 1,
            })
            .unwrap();

        let query = SearchQuery::new("project");
        assert_eq!(memory.search(&query, viewer).len(), 1);
        assert!(memory.search(&query, other).is_empty());
    }

    #[test]
    fn spatial_navigation_picks_neighbor_center() {
        let (desktop, _, right) = desktop_pair();
        let navigator = SpatialNavigator::new(desktop);
        let target = navigator
            .navigate(WorkspacePoint::new(500, 400), SnapDirection::Right)
            .unwrap();
        assert_eq!(target.device, right);
        assert_eq!(target.point, WorkspacePoint::new(1600, 450));
    }
}
