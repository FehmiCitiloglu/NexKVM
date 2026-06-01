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

/// A point in viewport/screen space (logical UI pixels).
///
/// Distinct from [`WorkspacePoint`] (integer *world* coordinates): screen points
/// are floating point so pan/zoom stay smooth and reversible.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ScreenPoint {
    /// Horizontal screen coordinate.
    pub x: f64,
    /// Vertical screen coordinate.
    pub y: f64,
}

impl ScreenPoint {
    /// Construct a screen point.
    #[must_use]
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

/// Size of the viewport the spatial map is rendered into, in screen pixels.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ViewportSize {
    /// Viewport width in screen pixels.
    pub width: f64,
    /// Viewport height in screen pixels.
    pub height: f64,
}

impl ViewportSize {
    /// Construct a viewport size.
    #[must_use]
    pub const fn new(width: f64, height: f64) -> Self {
        Self { width, height }
    }
}

/// A pannable, zoomable camera over the (effectively infinite) virtual desktop.
///
/// The camera maps the unbounded world coordinate space onto a finite viewport
/// so a spatial-desktop UI can render a zoomable device topology. It is pure and
/// sans-IO: the UI layer owns rendering and input; this only computes the
/// world↔screen transform, panning, anchored zoom, and zoom-to-fit. `zoom` is
/// screen pixels per world pixel and is clamped to [`MIN_ZOOM`, `MAX_ZOOM`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SpatialViewport {
    /// World point currently centered in the viewport.
    center_x: f64,
    center_y: f64,
    /// Screen pixels per world pixel.
    zoom: f64,
    /// Viewport size in screen pixels.
    size: ViewportSize,
}

impl SpatialViewport {
    /// Smallest allowed zoom (most zoomed-out).
    pub const MIN_ZOOM: f64 = 0.02;
    /// Largest allowed zoom (most zoomed-in).
    pub const MAX_ZOOM: f64 = 8.0;

    /// Create a viewport centered on the world origin at unit zoom.
    #[must_use]
    pub fn new(size: ViewportSize) -> Self {
        Self {
            center_x: 0.0,
            center_y: 0.0,
            zoom: 1.0,
            size,
        }
    }

    /// Current zoom (screen pixels per world pixel).
    #[must_use]
    pub const fn zoom(self) -> f64 {
        self.zoom
    }

    /// World point at the center of the viewport.
    #[must_use]
    pub fn center(self) -> WorkspacePoint {
        WorkspacePoint::new(round_to_i32(self.center_x), round_to_i32(self.center_y))
    }

    /// Viewport size in screen pixels.
    #[must_use]
    pub const fn size(self) -> ViewportSize {
        self.size
    }

    /// Update the viewport size (e.g. on window resize), keeping center/zoom.
    pub fn set_size(&mut self, size: ViewportSize) {
        self.size = size;
    }

    /// Project a world point to screen space.
    #[must_use]
    pub fn world_to_screen(self, world: WorkspacePoint) -> ScreenPoint {
        ScreenPoint::new(
            (f64::from(world.x) - self.center_x) * self.zoom + self.size.width / 2.0,
            (f64::from(world.y) - self.center_y) * self.zoom + self.size.height / 2.0,
        )
    }

    /// Unproject a screen point back to world space.
    #[must_use]
    pub fn screen_to_world(self, screen: ScreenPoint) -> WorkspacePoint {
        let wx = (screen.x - self.size.width / 2.0) / self.zoom + self.center_x;
        let wy = (screen.y - self.size.height / 2.0) / self.zoom + self.center_y;
        WorkspacePoint::new(round_to_i32(wx), round_to_i32(wy))
    }

    /// Pan the camera by a screen-space delta (e.g. a drag gesture).
    pub fn pan_by_screen(&mut self, dx: f64, dy: f64) {
        self.center_x -= dx / self.zoom;
        self.center_y -= dy / self.zoom;
    }

    /// Zoom by `factor` while keeping the world point under `anchor` fixed on
    /// screen — the standard "scroll-to-zoom toward the cursor" behavior.
    ///
    /// `factor > 1.0` zooms in, `< 1.0` zooms out. The resulting zoom is clamped
    /// to [`MIN_ZOOM`, `MAX_ZOOM`].
    pub fn zoom_by(&mut self, factor: f64, anchor: ScreenPoint) {
        if !(factor.is_finite() && factor > 0.0) {
            return;
        }
        let before = self.screen_to_world_f64(anchor);
        self.zoom = (self.zoom * factor).clamp(Self::MIN_ZOOM, Self::MAX_ZOOM);
        let after = self.screen_to_world_f64(anchor);
        // Shift center so the anchor's world point stays put.
        self.center_x += before.0 - after.0;
        self.center_y += before.1 - after.1;
    }

    /// Recenter (and rezoom) so `rect` fits in the viewport with `padding`
    /// fraction of margin on each side. Used for "zoom to fit all devices".
    ///
    /// A zero-area rect or non-positive viewport leaves the camera unchanged.
    pub fn fit_rect(&mut self, rect: WorkspaceRect, padding: f64) {
        if self.size.width <= 0.0 || self.size.height <= 0.0 {
            return;
        }
        let pad = padding.clamp(0.0, 0.9);
        let usable_w = self.size.width * (1.0 - pad);
        let usable_h = self.size.height * (1.0 - pad);
        let world_w = f64::from(rect.width).max(1.0);
        let world_h = f64::from(rect.height).max(1.0);
        let zoom = (usable_w / world_w).min(usable_h / world_h);
        self.zoom = zoom.clamp(Self::MIN_ZOOM, Self::MAX_ZOOM);
        let center = rect.center();
        self.center_x = f64::from(center.x);
        self.center_y = f64::from(center.y);
    }

    fn screen_to_world_f64(self, screen: ScreenPoint) -> (f64, f64) {
        (
            (screen.x - self.size.width / 2.0) / self.zoom + self.center_x,
            (screen.y - self.size.height / 2.0) / self.zoom + self.center_y,
        )
    }
}

fn round_to_i32(value: f64) -> i32 {
    let clamped = value
        .round()
        .clamp(f64::from(i32::MIN), f64::from(i32::MAX));
    clamped as i32
}

/// 2D gesture release velocity in world pixels per second.
///
/// Produced by the UI/input layer when the user "flicks" the pointer (or a
/// dragged file) toward another device. This is the raw momentum of the
/// gesture; [`FlickPlanner`] turns it into a destination.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FlickVector {
    /// Horizontal velocity in world pixels per second.
    pub vx: f64,
    /// Vertical velocity in world pixels per second.
    pub vy: f64,
}

impl FlickVector {
    /// Construct a flick velocity.
    #[must_use]
    pub const fn new(vx: f64, vy: f64) -> Self {
        Self { vx, vy }
    }

    /// Speed magnitude in world pixels per second.
    #[must_use]
    pub fn speed(self) -> f64 {
        self.vx.hypot(self.vy)
    }
}

/// What a throw gesture is carrying to the destination device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ThrowPayload {
    /// Hand the pointer/control to the target device ("throw the mouse").
    Cursor,
    /// Deliver a pending file transfer to the target device.
    File,
    /// Push the current clipboard to the target device.
    Clipboard,
}

/// Inertial tuning for throw/flick resolution.
///
/// `friction` models how quickly the projectile decelerates: a harder flick
/// travels farther, so a weak flick may not reach any neighbor at all.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ThrowConfig {
    /// Inertial deceleration in world pixels per second squared. Must be > 0.
    pub friction: f64,
    /// Minimum release speed (world px/s) for a gesture to count as a throw.
    pub min_speed: f64,
}

impl Default for ThrowConfig {
    fn default() -> Self {
        Self {
            friction: 1800.0,
            min_speed: 600.0,
        }
    }
}

/// Resolved destination of a throw/flick gesture.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThrowOutcome {
    /// Device the gesture originated on.
    pub source: DeviceId,
    /// Device the projectile lands on.
    pub target: DeviceId,
    /// What is being delivered.
    pub payload: ThrowPayload,
    /// Landing point inside the target device's bounds.
    pub landing: WorkspacePoint,
    /// Total inertial travel distance in world pixels.
    pub travel_distance: f64,
}

/// Physics-based "throw / flick" planner over the unified virtual desktop.
///
/// Given a release point and gesture velocity, it projects an inertial
/// trajectory across the shared desktop and resolves which online device the
/// projectile reaches (the "throw the mouse to another screen" / "throw a file
/// like a physics object" interaction). It is pure and sans-IO: the input layer
/// supplies the gesture velocity; the daemon performs the resulting handoff.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FlickPlanner {
    desktop: UnifiedVirtualDesktop,
    config: ThrowConfig,
}

impl FlickPlanner {
    /// Construct a planner with default inertial tuning.
    #[must_use]
    pub fn new(desktop: UnifiedVirtualDesktop) -> Self {
        Self {
            desktop,
            config: ThrowConfig::default(),
        }
    }

    /// Construct a planner with explicit inertial tuning.
    #[must_use]
    pub fn with_config(desktop: UnifiedVirtualDesktop, config: ThrowConfig) -> Self {
        Self { desktop, config }
    }

    /// Resolve a flick/throw gesture to a destination device.
    ///
    /// Returns `None` when the gesture is too weak to count as a throw, the
    /// configuration is degenerate, or no online device (other than `source`)
    /// lies along the trajectory within inertial reach. The landing point is
    /// always clamped inside the target device's bounds.
    #[must_use]
    pub fn throw(
        &self,
        source: DeviceId,
        release: WorkspacePoint,
        velocity: FlickVector,
        payload: ThrowPayload,
    ) -> Option<ThrowOutcome> {
        let speed = velocity.speed();
        if !speed.is_finite() || speed < self.config.min_speed {
            return None;
        }
        let friction = self.config.friction;
        if !(friction.is_finite() && friction > 0.0) {
            return None;
        }
        // Inertial throw distance under constant deceleration: d = v² / (2a).
        let max_distance = (speed * speed) / (2.0 * friction);
        let dir = (velocity.vx / speed, velocity.vy / speed);
        let origin = (f64::from(release.x), f64::from(release.y));

        let mut best: Option<(f64, &WorkspaceDevice, f64)> = None;
        for device in self.desktop.devices() {
            if !device.online || device.device == source {
                continue;
            }
            let Some((t_near, t_far)) = ray_rect_intersection(origin, dir, device.bounds) else {
                continue;
            };
            // Device must be ahead of the release point and within reach.
            if t_far < 0.0 || t_near > max_distance {
                continue;
            }
            let entry = t_near.max(0.0);
            if best.is_none_or(|(best_near, _, _)| entry < best_near) {
                best = Some((entry, device, t_far));
            }
        }

        let (_, device, t_far) = best?;
        // Stop point along the trajectory, clamped to the device bounds so the
        // payload always lands inside the destination.
        let stop = max_distance.min(t_far);
        let land_x = origin.0 + dir.0 * stop;
        let land_y = origin.1 + dir.1 * stop;
        Some(ThrowOutcome {
            source,
            target: device.device,
            payload,
            landing: clamp_point_to_rect(land_x, land_y, device.bounds),
            travel_distance: max_distance,
        })
    }
}

/// Ray vs. axis-aligned rectangle intersection (slab method).
///
/// Returns the near/far distances along the unit-length `dir` at which the ray
/// from `origin` enters and exits `rect`, or `None` if it never intersects.
fn ray_rect_intersection(
    origin: (f64, f64),
    dir: (f64, f64),
    rect: WorkspaceRect,
) -> Option<(f64, f64)> {
    let bounds = [
        (
            origin.0,
            dir.0,
            f64::from(rect.left()),
            f64::from(rect.right()),
        ),
        (
            origin.1,
            dir.1,
            f64::from(rect.top()),
            f64::from(rect.bottom()),
        ),
    ];
    let mut t_near = f64::NEG_INFINITY;
    let mut t_far = f64::INFINITY;
    for (o, d, lo, hi) in bounds {
        if d.abs() < f64::EPSILON {
            // Ray parallel to this slab: it must already lie within it.
            if o < lo || o > hi {
                return None;
            }
        } else {
            let inv = 1.0 / d;
            let mut t1 = (lo - o) * inv;
            let mut t2 = (hi - o) * inv;
            if t1 > t2 {
                core::mem::swap(&mut t1, &mut t2);
            }
            t_near = t_near.max(t1);
            t_far = t_far.min(t2);
            if t_near > t_far {
                return None;
            }
        }
    }
    Some((t_near, t_far))
}

fn clamp_point_to_rect(x: f64, y: f64, rect: WorkspaceRect) -> WorkspacePoint {
    // `WorkspaceRect::contains` is right/bottom-exclusive, so clamp to the last
    // interior pixel to guarantee the landing point is inside the device.
    let max_x = f64::from(rect.right().saturating_sub(1)).max(f64::from(rect.left()));
    let max_y = f64::from(rect.bottom().saturating_sub(1)).max(f64::from(rect.top()));
    let cx = x.clamp(f64::from(rect.left()), max_x);
    let cy = y.clamp(f64::from(rect.top()), max_y);
    WorkspacePoint::new(round_to_i32(cx), round_to_i32(cy))
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

    #[test]
    fn viewport_world_screen_roundtrip_is_stable() {
        let mut viewport = SpatialViewport::new(ViewportSize::new(800.0, 600.0));
        viewport.zoom_by(2.0, ScreenPoint::new(400.0, 300.0));
        let world = WorkspacePoint::new(1234, -567);
        let screen = viewport.world_to_screen(world);
        assert_eq!(viewport.screen_to_world(screen), world);
    }

    #[test]
    fn anchored_zoom_keeps_point_under_cursor_fixed() {
        let mut viewport = SpatialViewport::new(ViewportSize::new(1000.0, 800.0));
        let cursor = ScreenPoint::new(720.0, 240.0);
        let world_under_cursor = viewport.screen_to_world(cursor);
        viewport.zoom_by(3.0, cursor);
        // The world point under the cursor must stay under the cursor.
        assert_eq!(viewport.screen_to_world(cursor), world_under_cursor);
        assert!((viewport.zoom() - 3.0).abs() < f64::EPSILON);
    }

    #[test]
    fn zoom_is_clamped_to_limits() {
        let mut viewport = SpatialViewport::new(ViewportSize::new(800.0, 600.0));
        let anchor = ScreenPoint::new(400.0, 300.0);
        viewport.zoom_by(1_000.0, anchor);
        assert!((viewport.zoom() - SpatialViewport::MAX_ZOOM).abs() < f64::EPSILON);
        viewport.zoom_by(0.000_001, anchor);
        assert!((viewport.zoom() - SpatialViewport::MIN_ZOOM).abs() < f64::EPSILON);
    }

    #[test]
    fn pan_shifts_center_in_world_units() {
        let mut viewport = SpatialViewport::new(ViewportSize::new(800.0, 600.0));
        viewport.zoom_by(2.0, ScreenPoint::new(400.0, 300.0));
        let before = viewport.center();
        // Dragging content right by 200 screen px moves the camera left 100 world px.
        viewport.pan_by_screen(200.0, 0.0);
        assert_eq!(viewport.center().x, before.x - 100);
    }

    #[test]
    fn fit_rect_centers_and_frames_all_devices() {
        let (desktop, _, _) = desktop_pair();
        let bounds = desktop.bounds().unwrap();
        let mut viewport = SpatialViewport::new(ViewportSize::new(800.0, 600.0));
        viewport.fit_rect(bounds, 0.1);

        // Camera centers on the combined device bounds.
        assert_eq!(viewport.center(), bounds.center());
        // The whole topology lands within the viewport.
        let top_left = viewport.world_to_screen(WorkspacePoint::new(bounds.left(), bounds.top()));
        let bottom_right =
            viewport.world_to_screen(WorkspacePoint::new(bounds.right(), bounds.bottom()));
        assert!(top_left.x >= 0.0 && top_left.y >= 0.0);
        assert!(bottom_right.x <= 800.0 && bottom_right.y <= 600.0);
    }

    #[test]
    fn flick_throws_cursor_to_device_in_gesture_direction() {
        let (desktop, left, right) = desktop_pair();
        let planner = FlickPlanner::new(desktop);
        // Hard flick to the right from inside the left device.
        let outcome = planner
            .throw(
                left,
                WorkspacePoint::new(500, 400),
                FlickVector::new(3000.0, 0.0),
                ThrowPayload::Cursor,
            )
            .expect("throw should reach the right device");
        assert_eq!(outcome.source, left);
        assert_eq!(outcome.target, right);
        assert_eq!(outcome.payload, ThrowPayload::Cursor);
        // Landing point must be inside the right device's bounds.
        let target_bounds = WorkspaceRect::new(1000, 0, 1200, 900);
        assert!(target_bounds.contains(outcome.landing));
    }

    #[test]
    fn weak_flick_does_not_reach_any_device() {
        let (desktop, left, _) = desktop_pair();
        let planner = FlickPlanner::new(desktop);
        // Below the min_speed threshold: not a throw at all.
        assert!(
            planner
                .throw(
                    left,
                    WorkspacePoint::new(500, 400),
                    FlickVector::new(100.0, 0.0),
                    ThrowPayload::Cursor,
                )
                .is_none()
        );
    }

    #[test]
    fn flick_away_from_neighbors_finds_no_target() {
        let (desktop, left, _) = desktop_pair();
        let planner = FlickPlanner::new(desktop);
        // Strong flick to the left, away from the only neighbor on the right.
        assert!(
            planner
                .throw(
                    left,
                    WorkspacePoint::new(500, 400),
                    FlickVector::new(-4000.0, 0.0),
                    ThrowPayload::File,
                )
                .is_none()
        );
    }

    #[test]
    fn harder_flick_travels_farther() {
        let (desktop, left, _) = desktop_pair();
        let planner = FlickPlanner::new(desktop);
        let soft = planner
            .throw(
                left,
                WorkspacePoint::new(500, 400),
                FlickVector::new(2000.0, 0.0),
                ThrowPayload::File,
            )
            .map(|outcome| outcome.travel_distance);
        let hard = planner
            .throw(
                left,
                WorkspacePoint::new(500, 400),
                FlickVector::new(4000.0, 0.0),
                ThrowPayload::File,
            )
            .map(|outcome| outcome.travel_distance)
            .expect("hard flick should reach the right device");
        // A harder flick reaches farther; a soft flick may not reach at all.
        assert!(soft.is_none_or(|soft_distance| hard > soft_distance));
    }
}
