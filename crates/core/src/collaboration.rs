//! Collaborative session control plane.
//!
//! This module models shared cursor mode, pair programming, collaborative
//! control, remote teaching, and multi-user sessions. It is intentionally
//! platform-neutral: real cursor rendering, input injection, screen sharing, and
//! accessibility prompts stay in the `platform-*`, `input`, and `streaming`
//! crates behind safe async boundaries.
//!
//! Security posture: collaboration is only valid inside an authenticated,
//! encrypted session with trusted devices. Control is deny-by-default: users may
//! observe shared cursors without receiving input authority, and every control
//! lease is explicit, scoped, and revocable.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::identity::DeviceId;
use crate::workspace::{WorkspacePoint, WorkspaceRect};

/// Stable id for one collaborative session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CollaborationSessionId(pub Uuid);

impl CollaborationSessionId {
    /// Generate a fresh session id.
    #[must_use]
    pub fn generate() -> Self {
        Self(Uuid::new_v4())
    }
}

/// Stable id for a participant within a collaborative session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ParticipantId(pub Uuid);

impl ParticipantId {
    /// Generate a fresh participant id.
    #[must_use]
    pub fn generate() -> Self {
        Self(Uuid::new_v4())
    }
}

/// Errors from collaborative session planning and host backends.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CollaborationError {
    /// Session does not exist.
    #[error("collaboration session not found: {0}")]
    SessionNotFound(String),

    /// Participant does not exist.
    #[error("collaboration participant not found: {0}")]
    ParticipantNotFound(String),

    /// Requested action is not allowed by role/session policy.
    #[error("collaboration permission denied: {0}")]
    PermissionDenied(&'static str),

    /// Current control lease conflicts with the requested action.
    #[error("collaboration control conflict: {0}")]
    ControlConflict(&'static str),

    /// Invalid model input.
    #[error("invalid collaboration input: {0}")]
    InvalidInput(&'static str),

    /// Platform/runtime backend failure.
    #[error("collaboration backend error: {0}")]
    Backend(String),
}

/// Collaborative feature mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CollaborationMode {
    /// Participants see each other's cursors, but only the local owner controls input.
    SharedCursor,
    /// Driver/navigator workflow for pair programming.
    PairProgramming,
    /// Host may delegate control to trusted participants.
    CollaborativeControl,
    /// Teacher broadcasts cursor/annotations and can grant temporary student control.
    RemoteTeaching,
}

/// Participant role inside a collaborative session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ParticipantRole {
    /// Session owner and policy authority.
    Host,
    /// Active controller in pair-programming or delegated-control mode.
    Driver,
    /// Can observe and request/suggest control, but does not inject input by default.
    Navigator,
    /// Teaching presenter.
    Teacher,
    /// Teaching attendee.
    Student,
    /// Observe-only participant.
    Observer,
}

/// Fine-grained participant capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollaborationPermissions {
    /// May see session state and shared cursor updates.
    pub observe: bool,
    /// May publish a visible shared cursor.
    pub share_cursor: bool,
    /// May request an input control lease.
    pub request_control: bool,
    /// May receive delegated input control.
    pub receive_control: bool,
    /// May annotate/point during teaching.
    pub annotate: bool,
    /// May manage participants and grant/revoke control.
    pub administer: bool,
}

impl CollaborationPermissions {
    /// No permissions.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            observe: false,
            share_cursor: false,
            request_control: false,
            receive_control: false,
            annotate: false,
            administer: false,
        }
    }

    /// Permissions implied by a role.
    #[must_use]
    pub const fn for_role(role: ParticipantRole) -> Self {
        match role {
            ParticipantRole::Host => Self {
                observe: true,
                share_cursor: true,
                request_control: true,
                receive_control: true,
                annotate: true,
                administer: true,
            },
            ParticipantRole::Driver => Self {
                observe: true,
                share_cursor: true,
                request_control: true,
                receive_control: true,
                annotate: true,
                administer: false,
            },
            ParticipantRole::Navigator => Self {
                observe: true,
                share_cursor: true,
                request_control: true,
                receive_control: false,
                annotate: true,
                administer: false,
            },
            ParticipantRole::Teacher => Self {
                observe: true,
                share_cursor: true,
                request_control: true,
                receive_control: true,
                annotate: true,
                administer: true,
            },
            ParticipantRole::Student => Self {
                observe: true,
                share_cursor: true,
                request_control: true,
                receive_control: false,
                annotate: true,
                administer: false,
            },
            ParticipantRole::Observer => Self {
                observe: true,
                share_cursor: false,
                request_control: false,
                receive_control: false,
                annotate: false,
                administer: false,
            },
        }
    }
}

/// Policy knobs for a collaborative session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollaborationPolicy {
    /// Multiple participants may publish shared cursors at once.
    pub multi_cursor: bool,
    /// Participants may request a control lease.
    pub allow_control_requests: bool,
    /// Host/teacher may grant input control to another participant.
    pub allow_delegated_control: bool,
    /// Control returns to the host when a lease expires.
    pub revoke_on_timeout: bool,
    /// Maximum active participants.
    pub max_participants: usize,
    /// Default lease duration in milliseconds.
    pub default_lease_millis: u64,
}

impl CollaborationPolicy {
    /// Shared cursor mode: observation and multi-cursor only.
    #[must_use]
    pub const fn shared_cursor() -> Self {
        Self {
            multi_cursor: true,
            allow_control_requests: false,
            allow_delegated_control: false,
            revoke_on_timeout: true,
            max_participants: 8,
            default_lease_millis: 0,
        }
    }

    /// Pair programming: driver/navigator with explicit driver handoff.
    #[must_use]
    pub const fn pair_programming() -> Self {
        Self {
            multi_cursor: true,
            allow_control_requests: true,
            allow_delegated_control: true,
            revoke_on_timeout: true,
            max_participants: 2,
            default_lease_millis: 15 * 60 * 1000,
        }
    }

    /// Collaborative control for a trusted small group.
    #[must_use]
    pub const fn collaborative_control() -> Self {
        Self {
            multi_cursor: true,
            allow_control_requests: true,
            allow_delegated_control: true,
            revoke_on_timeout: true,
            max_participants: 6,
            default_lease_millis: 5 * 60 * 1000,
        }
    }

    /// Remote teaching: teacher manages attendees and grants short control turns.
    #[must_use]
    pub const fn remote_teaching() -> Self {
        Self {
            multi_cursor: true,
            allow_control_requests: true,
            allow_delegated_control: true,
            revoke_on_timeout: true,
            max_participants: 32,
            default_lease_millis: 2 * 60 * 1000,
        }
    }

    /// Default policy for a mode.
    #[must_use]
    pub const fn for_mode(mode: CollaborationMode) -> Self {
        match mode {
            CollaborationMode::SharedCursor => Self::shared_cursor(),
            CollaborationMode::PairProgramming => Self::pair_programming(),
            CollaborationMode::CollaborativeControl => Self::collaborative_control(),
            CollaborationMode::RemoteTeaching => Self::remote_teaching(),
        }
    }
}

/// One participant in a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollaborationParticipant {
    /// Participant id.
    pub id: ParticipantId,
    /// Backing device id.
    pub device: DeviceId,
    /// Display name.
    pub name: String,
    /// Role.
    pub role: ParticipantRole,
    /// Effective permissions.
    pub permissions: CollaborationPermissions,
    /// Whether this participant is currently connected.
    pub online: bool,
}

impl CollaborationParticipant {
    /// Construct a participant with permissions derived from role.
    #[must_use]
    pub fn new(device: DeviceId, name: impl Into<String>, role: ParticipantRole) -> Self {
        Self {
            id: ParticipantId::generate(),
            device,
            name: name.into(),
            role,
            permissions: CollaborationPermissions::for_role(role),
            online: true,
        }
    }
}

/// Shared cursor visibility state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SharedCursorVisibility {
    /// Cursor is visible to collaborators.
    Visible,
    /// Cursor is intentionally hidden.
    Hidden,
    /// Cursor is temporarily suppressed because the participant is controlling input.
    Controlling,
}

/// Shared cursor update in unified workspace coordinates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedCursorUpdate {
    /// Session id.
    pub session: CollaborationSessionId,
    /// Participant publishing the cursor.
    pub participant: ParticipantId,
    /// Device whose workspace contains the cursor.
    pub device: DeviceId,
    /// Cursor point in workspace coordinates.
    pub position: WorkspacePoint,
    /// Optional area being pointed at/highlighted.
    pub focus_rect: Option<WorkspaceRect>,
    /// Cursor visibility.
    pub visibility: SharedCursorVisibility,
    /// Monotonic timestamp supplied by caller.
    pub at_millis: u64,
}

/// An active delegated-control lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlLease {
    /// Participant currently allowed to control input.
    pub holder: ParticipantId,
    /// Device receiving injected control.
    pub target_device: DeviceId,
    /// Grant timestamp.
    pub granted_at_millis: u64,
    /// Expiration timestamp.
    pub expires_at_millis: u64,
}

impl ControlLease {
    /// Whether this lease is still valid at `now_millis`.
    #[must_use]
    pub const fn is_active(self, now_millis: u64) -> bool {
        now_millis < self.expires_at_millis
    }
}

/// A collaborative session snapshot and policy engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollaborationSession {
    /// Session id.
    pub id: CollaborationSessionId,
    /// Host device.
    pub host: DeviceId,
    /// Active mode.
    pub mode: CollaborationMode,
    /// Session policy.
    pub policy: CollaborationPolicy,
    participants: Vec<CollaborationParticipant>,
    cursors: Vec<SharedCursorUpdate>,
    control: Option<ControlLease>,
}

impl CollaborationSession {
    /// Create a new session with the host participant already present.
    #[must_use]
    pub fn new(host: CollaborationParticipant, mode: CollaborationMode) -> Self {
        Self {
            id: CollaborationSessionId::generate(),
            host: host.device,
            mode,
            policy: CollaborationPolicy::for_mode(mode),
            participants: vec![host],
            cursors: Vec::new(),
            control: None,
        }
    }

    /// Participants in stable join order.
    #[must_use]
    pub fn participants(&self) -> &[CollaborationParticipant] {
        &self.participants
    }

    /// Latest shared cursor updates.
    #[must_use]
    pub fn cursors(&self) -> &[SharedCursorUpdate] {
        &self.cursors
    }

    /// Current control lease.
    #[must_use]
    pub const fn control(&self) -> Option<ControlLease> {
        self.control
    }

    /// Find a participant.
    #[must_use]
    pub fn participant(&self, id: ParticipantId) -> Option<&CollaborationParticipant> {
        self.participants
            .iter()
            .find(|participant| participant.id == id)
    }

    /// Add a participant if capacity allows.
    ///
    /// # Errors
    /// Returns [`CollaborationError::PermissionDenied`] if the session is full.
    pub fn join(
        &mut self,
        participant: CollaborationParticipant,
    ) -> Result<(), CollaborationError> {
        if self.participants.len() >= self.policy.max_participants {
            return Err(CollaborationError::PermissionDenied("session is full"));
        }
        if self
            .participants
            .iter()
            .any(|existing| existing.id == participant.id || existing.device == participant.device)
        {
            return Err(CollaborationError::InvalidInput(
                "participant already joined session",
            ));
        }
        self.participants.push(participant);
        Ok(())
    }

    /// Remove a participant and revoke their cursor/control state.
    pub fn leave(&mut self, participant: ParticipantId) -> bool {
        let Some(index) = self
            .participants
            .iter()
            .position(|existing| existing.id == participant)
        else {
            return false;
        };
        self.participants.remove(index);
        self.cursors
            .retain(|cursor| cursor.participant != participant);
        if self
            .control
            .is_some_and(|lease| lease.holder == participant)
        {
            self.control = None;
        }
        true
    }

    /// Publish or replace a participant's shared cursor update.
    ///
    /// # Errors
    /// Returns [`CollaborationError`] if the participant cannot share cursors or
    /// another cursor is already active in a single-cursor session.
    pub fn update_cursor(&mut self, update: SharedCursorUpdate) -> Result<(), CollaborationError> {
        if update.session != self.id {
            return Err(CollaborationError::SessionNotFound(
                update.session.0.to_string(),
            ));
        }
        let participant = self.require_participant(update.participant)?;
        if !participant.permissions.share_cursor {
            return Err(CollaborationError::PermissionDenied(
                "participant cannot share cursor",
            ));
        }
        if !self.policy.multi_cursor
            && self
                .cursors
                .iter()
                .any(|cursor| cursor.participant != update.participant)
        {
            return Err(CollaborationError::ControlConflict(
                "session allows only one shared cursor",
            ));
        }
        if let Some(existing) = self
            .cursors
            .iter_mut()
            .find(|cursor| cursor.participant == update.participant)
        {
            *existing = update;
        } else {
            self.cursors.push(update);
        }
        Ok(())
    }

    /// Grant input control to a participant.
    ///
    /// # Errors
    /// Returns [`CollaborationError`] if delegated control is disabled, the
    /// grantor cannot administer the session, or the holder cannot receive
    /// control.
    pub fn grant_control(
        &mut self,
        grantor: ParticipantId,
        holder: ParticipantId,
        target_device: DeviceId,
        now_millis: u64,
        duration_millis: Option<u64>,
    ) -> Result<ControlLease, CollaborationError> {
        if !self.policy.allow_delegated_control {
            return Err(CollaborationError::PermissionDenied(
                "delegated control disabled",
            ));
        }
        if !self.require_participant(grantor)?.permissions.administer {
            return Err(CollaborationError::PermissionDenied(
                "participant cannot grant control",
            ));
        }
        if !self
            .require_participant(holder)?
            .permissions
            .receive_control
        {
            return Err(CollaborationError::PermissionDenied(
                "participant cannot receive control",
            ));
        }
        let duration = duration_millis.unwrap_or(self.policy.default_lease_millis);
        if duration == 0 {
            return Err(CollaborationError::InvalidInput(
                "control lease duration must be non-zero",
            ));
        }
        let lease = ControlLease {
            holder,
            target_device,
            granted_at_millis: now_millis,
            expires_at_millis: now_millis.saturating_add(duration),
        };
        self.control = Some(lease);
        Ok(lease)
    }

    /// Revoke the current control lease.
    ///
    /// # Errors
    /// Returns [`CollaborationError`] if the requester is not an administrator
    /// and not the current lease holder.
    pub fn revoke_control(&mut self, requester: ParticipantId) -> Result<(), CollaborationError> {
        let can_revoke = self.require_participant(requester)?.permissions.administer
            || self.control.is_some_and(|lease| lease.holder == requester);
        if !can_revoke {
            return Err(CollaborationError::PermissionDenied(
                "participant cannot revoke control",
            ));
        }
        self.control = None;
        Ok(())
    }

    /// Whether a participant may currently inject collaborative control.
    #[must_use]
    pub fn can_control(
        &self,
        participant: ParticipantId,
        target_device: DeviceId,
        now_millis: u64,
    ) -> bool {
        self.control.is_some_and(|lease| {
            lease.holder == participant
                && lease.target_device == target_device
                && lease.is_active(now_millis)
        })
    }

    /// Drop expired control lease, returning whether one was revoked.
    pub fn expire_control(&mut self, now_millis: u64) -> bool {
        if self.policy.revoke_on_timeout
            && self
                .control
                .is_some_and(|lease| !lease.is_active(now_millis))
        {
            self.control = None;
            true
        } else {
            false
        }
    }

    fn require_participant(
        &self,
        participant: ParticipantId,
    ) -> Result<&CollaborationParticipant, CollaborationError> {
        self.participant(participant)
            .ok_or_else(|| CollaborationError::ParticipantNotFound(participant.0.to_string()))
    }
}

/// Backend for rendering cursors/annotations and applying delegated control.
#[async_trait]
pub trait CollaborationBackend: Send + Sync {
    /// Render or hide a shared cursor update.
    ///
    /// # Errors
    /// Returns [`CollaborationError`] on platform/render failure.
    async fn publish_cursor(&self, update: SharedCursorUpdate) -> Result<(), CollaborationError>;

    /// Apply an already-authorized control lease to the local platform.
    ///
    /// # Errors
    /// Returns [`CollaborationError`] on backend failure or missing OS permission.
    async fn activate_control(&self, lease: ControlLease) -> Result<(), CollaborationError>;

    /// Revoke active collaborative control on the local platform.
    ///
    /// # Errors
    /// Returns [`CollaborationError`] on backend failure.
    async fn revoke_control(
        &self,
        session: CollaborationSessionId,
    ) -> Result<(), CollaborationError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn participant(role: ParticipantRole) -> CollaborationParticipant {
        CollaborationParticipant::new(DeviceId::generate(), format!("{role:?}"), role)
    }

    #[test]
    fn shared_cursor_mode_rejects_control_grants() {
        let host = participant(ParticipantRole::Host);
        let host_id = host.id;
        let driver = participant(ParticipantRole::Driver);
        let driver_id = driver.id;
        let target = driver.device;
        let mut session = CollaborationSession::new(host, CollaborationMode::SharedCursor);
        session.join(driver).unwrap();

        let err = session
            .grant_control(host_id, driver_id, target, 100, Some(1_000))
            .unwrap_err();
        assert!(matches!(err, CollaborationError::PermissionDenied(_)));
    }

    #[test]
    fn pair_programming_grants_and_expires_driver_control() {
        let host = participant(ParticipantRole::Host);
        let host_id = host.id;
        let driver = participant(ParticipantRole::Driver);
        let driver_id = driver.id;
        let target = host.device;
        let mut session = CollaborationSession::new(host, CollaborationMode::PairProgramming);
        session.join(driver).unwrap();

        let lease = session
            .grant_control(host_id, driver_id, target, 10, Some(50))
            .unwrap();
        assert_eq!(lease.holder, driver_id);
        assert!(session.can_control(driver_id, target, 20));
        assert!(!session.can_control(driver_id, target, 60));
        assert!(session.expire_control(60));
        assert!(session.control().is_none());
    }

    #[test]
    fn observer_cannot_publish_shared_cursor() {
        let host = participant(ParticipantRole::Host);
        let observer = participant(ParticipantRole::Observer);
        let observer_id = observer.id;
        let observer_device = observer.device;
        let mut session = CollaborationSession::new(host, CollaborationMode::RemoteTeaching);
        session.join(observer).unwrap();

        let err = session
            .update_cursor(SharedCursorUpdate {
                session: session.id,
                participant: observer_id,
                device: observer_device,
                position: WorkspacePoint::new(10, 20),
                focus_rect: None,
                visibility: SharedCursorVisibility::Visible,
                at_millis: 1,
            })
            .unwrap_err();
        assert!(matches!(err, CollaborationError::PermissionDenied(_)));
    }

    #[test]
    fn shared_cursors_are_replaced_per_participant() {
        let host = participant(ParticipantRole::Host);
        let host_id = host.id;
        let host_device = host.device;
        let mut session = CollaborationSession::new(host, CollaborationMode::SharedCursor);

        session
            .update_cursor(SharedCursorUpdate {
                session: session.id,
                participant: host_id,
                device: host_device,
                position: WorkspacePoint::new(1, 1),
                focus_rect: None,
                visibility: SharedCursorVisibility::Visible,
                at_millis: 1,
            })
            .unwrap();
        session
            .update_cursor(SharedCursorUpdate {
                session: session.id,
                participant: host_id,
                device: host_device,
                position: WorkspacePoint::new(2, 3),
                focus_rect: Some(WorkspaceRect::new(0, 0, 10, 10)),
                visibility: SharedCursorVisibility::Visible,
                at_millis: 2,
            })
            .unwrap();

        assert_eq!(session.cursors().len(), 1);
        assert_eq!(session.cursors()[0].position, WorkspacePoint::new(2, 3));
        assert_eq!(
            session.cursors()[0].focus_rect,
            Some(WorkspaceRect::new(0, 0, 10, 10))
        );
    }

    #[test]
    fn session_capacity_is_enforced() {
        let host = participant(ParticipantRole::Teacher);
        let mut session = CollaborationSession::new(host, CollaborationMode::PairProgramming);
        session
            .join(participant(ParticipantRole::Navigator))
            .unwrap();
        let err = session
            .join(participant(ParticipantRole::Observer))
            .unwrap_err();
        assert!(matches!(err, CollaborationError::PermissionDenied(_)));
    }
}
