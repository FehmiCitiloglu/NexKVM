//! nexkvm core domain.
//!
//! `core` is the hub that the feature crates (input, clipboard, discovery, …)
//! and the platform backends depend on. It owns foundational concerns:
//!
//! - [`collaboration`] — the multi-user session control plane for shared
//!   cursors, pair programming, collaborative control, and remote teaching.
//! - [`identity`] — stable device identification independent of crypto keys.
//! - [`event`] — the async [`EventBus`]: a typed,
//!   broadcast-based pub/sub backbone that decouples producers (input capture,
//!   network ingress) from consumers (input injection, UI, plugins).
//! - [`platform`] — the [`PlatformBackend`] trait
//!   that each `platform-*` crate implements, plus a capability descriptor so
//!   higher layers can degrade gracefully on limited platforms (e.g. Wayland).
//! - [`workspace`] — the shared workspace control plane for unified virtual
//!   desktops, cross-device window snapping, app launching, global search,
//!   shared memory, and spatial navigation.
//! - [`management`] — optional cloud sync, enterprise policy, and team
//!   collaboration management primitives.
//!
//! Architecturally, `core` carries no I/O or OS calls itself — those live in
//! `network` and the `platform-*` crates — keeping it portable and testable.

pub mod automation;
pub mod collaboration;
pub mod event;
pub mod identity;
pub mod management;
pub mod platform;
pub mod workspace;

mod error;

pub use automation::{
    AutomationAction, AutomationEngine, AutomationPlan, AutomationRule, AutomationTrigger,
    CommandError, CommandId, CommandPaletteIndex, CommandScope, CrossDeviceNotification,
    NotificationAction, NotificationId, NotificationUrgency, QuickCommand, QuickCommandExecutor,
    ScriptContext, ScriptEngine, ScriptError, ScriptLanguage, ScriptRef, ShortcutId,
};
pub use collaboration::{
    CollaborationBackend, CollaborationError, CollaborationMode, CollaborationParticipant,
    CollaborationPermissions, CollaborationPolicy, CollaborationSession, CollaborationSessionId,
    ControlLease, ControlRequest, ParticipantId, ParticipantRole, SharedCursorUpdate,
    SharedCursorVisibility,
};
pub use error::CoreError;
pub use event::{Event, EventBus, EventEnvelope};
pub use identity::{DeviceId, DeviceInfo, DeviceRole, OsKind};
pub use management::{
    CloudSyncConfig, CloudSyncMode, CloudSyncProvider, EnterprisePolicy, ManagedFeature,
    ManagementError, PolicyDecision, TeamCollaborationSpace, TeamId, TeamMember, TeamMemberRole,
};
pub use platform::{
    NativeIntegration, NativeIntegrationAvailability, NativeIntegrationReport,
    NativeIntegrationStatus, PlatformBackend, PlatformCapabilities,
};
pub use workspace::{
    AppId, AppLaunchOutcome, AppLaunchRequest, ApplicationDescriptor, FlickPlanner, FlickVector,
    MemoryVisibility, ScreenPoint, SearchKind, SearchQuery, SearchResult, SharedWorkspaceMemory,
    SnapDirection, SpatialNavigationTarget, SpatialNavigator, SpatialViewport, ThrowConfig,
    ThrowOutcome, ThrowPayload, UnifiedVirtualDesktop, ViewportSize, WindowId, WindowSnapPlan,
    WindowSnapshot, WorkspaceBackend, WorkspaceDevice, WorkspaceError, WorkspaceMemoryEntry,
    WorkspacePoint, WorkspaceRect, WorkspaceSearchProvider, plan_window_snap,
};
