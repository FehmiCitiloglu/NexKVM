//! Cloud sync, enterprise policy, and team collaboration management.
//!
//! Long-term deployment modes need a control plane without turning nexkvm into a
//! centralized service. This module keeps that layer policy-only: cloud sync is
//! optional, enterprise policy is explicit and auditable, and team membership
//! gates collaboration capabilities before platform/network backends execute
//! anything.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::collaboration::CollaborationMode;
use crate::identity::DeviceId;

/// Errors from management and sync policy.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ManagementError {
    /// Policy denied the operation.
    #[error("management policy denied: {0}")]
    PolicyDenied(&'static str),
    /// Invalid management model input.
    #[error("invalid management input: {0}")]
    InvalidInput(&'static str),
    /// Backend/provider failure.
    #[error("management backend error: {0}")]
    Backend(String),
}

/// Optional cloud sync mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CloudSyncMode {
    /// Cloud sync disabled; local-first only.
    Disabled,
    /// User-provided/self-hosted sync endpoint.
    SelfHosted,
    /// Managed cloud sync endpoint.
    ManagedCloud,
}

/// Cloud sync configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudSyncConfig {
    /// Sync mode.
    pub mode: CloudSyncMode,
    /// Optional endpoint URL. Kept opaque here to avoid pulling URL parsing into core.
    pub endpoint: Option<String>,
    /// Sync trusted-device metadata.
    pub sync_trust_metadata: bool,
    /// Sync workspace memory.
    pub sync_workspace_memory: bool,
    /// Sync clipboard timeline metadata; payload bytes still require clipboard policy.
    pub sync_clipboard_timeline: bool,
    /// Require end-to-end encryption before uploading any user data.
    pub require_end_to_end_encryption: bool,
}

impl CloudSyncConfig {
    /// Local-first default.
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            mode: CloudSyncMode::Disabled,
            endpoint: None,
            sync_trust_metadata: false,
            sync_workspace_memory: false,
            sync_clipboard_timeline: false,
            require_end_to_end_encryption: true,
        }
    }

    /// Whether this configuration may upload user data.
    #[must_use]
    pub fn can_upload_user_data(&self) -> bool {
        !matches!(self.mode, CloudSyncMode::Disabled)
            && self
                .endpoint
                .as_ref()
                .is_some_and(|endpoint| endpoint.starts_with("https://"))
            && self.require_end_to_end_encryption
    }
}

impl Default for CloudSyncConfig {
    fn default() -> Self {
        Self::disabled()
    }
}

/// Async cloud sync provider boundary.
#[async_trait]
pub trait CloudSyncProvider: Send + Sync {
    /// Push encrypted sync payload bytes to the configured provider.
    ///
    /// # Errors
    /// Returns [`ManagementError`] on policy or backend failure.
    async fn push_encrypted(
        &self,
        namespace: &str,
        ciphertext: &[u8],
    ) -> Result<(), ManagementError>;
}

/// Features controllable by enterprise/team policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ManagedFeature {
    /// Remote/browser sessions.
    RemoteSessions,
    /// Cloud sync.
    CloudSync,
    /// Plugin marketplace/install.
    PluginMarketplace,
    /// Clipboard timeline sync.
    ClipboardTimeline,
    /// Team collaboration sessions.
    TeamCollaboration,
    /// Mesh forwarding through this device.
    MeshRouting,
}

/// Policy decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PolicyDecision {
    /// Allowed by policy.
    Allow,
    /// Denied by policy.
    Deny,
}

/// Enterprise policy snapshot distributed by a management panel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnterprisePolicy {
    /// Organization label/id.
    pub organization: String,
    /// Whether enrolled devices must be paired/trusted before any feature use.
    pub require_trusted_devices: bool,
    /// Whether remote/browser sessions are allowed.
    pub allow_remote_sessions: bool,
    /// Whether cloud sync is allowed.
    pub allow_cloud_sync: bool,
    /// Whether plugin marketplace installs are allowed.
    pub allow_plugin_marketplace: bool,
    /// Whether this device may forward mesh traffic.
    pub allow_mesh_routing: bool,
    /// Whether clipboard timeline sync is allowed.
    pub allow_clipboard_timeline: bool,
    /// Whether team collaboration is allowed.
    pub allow_team_collaboration: bool,
}

impl EnterprisePolicy {
    /// Permissive policy for unmanaged local-first installs.
    #[must_use]
    pub fn unmanaged() -> Self {
        Self {
            organization: "unmanaged".into(),
            require_trusted_devices: true,
            allow_remote_sessions: true,
            allow_cloud_sync: false,
            allow_plugin_marketplace: true,
            allow_mesh_routing: false,
            allow_clipboard_timeline: true,
            allow_team_collaboration: true,
        }
    }

    /// Evaluate a managed feature.
    #[must_use]
    pub const fn decide(&self, feature: ManagedFeature) -> PolicyDecision {
        let allowed = match feature {
            ManagedFeature::RemoteSessions => self.allow_remote_sessions,
            ManagedFeature::CloudSync => self.allow_cloud_sync,
            ManagedFeature::PluginMarketplace => self.allow_plugin_marketplace,
            ManagedFeature::ClipboardTimeline => self.allow_clipboard_timeline,
            ManagedFeature::TeamCollaboration => self.allow_team_collaboration,
            ManagedFeature::MeshRouting => self.allow_mesh_routing,
        };
        if allowed {
            PolicyDecision::Allow
        } else {
            PolicyDecision::Deny
        }
    }
}

/// Stable team id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TeamId(pub Uuid);

impl TeamId {
    /// Generate a team id.
    #[must_use]
    pub fn generate() -> Self {
        Self(Uuid::new_v4())
    }
}

/// Team role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TeamMemberRole {
    /// Team administrator.
    Admin,
    /// Can host sessions.
    Host,
    /// Can join sessions and receive delegated control when allowed.
    Member,
    /// Observe-only participant.
    Guest,
}

impl TeamMemberRole {
    fn can_host(self) -> bool {
        matches!(self, Self::Admin | Self::Host)
    }

    fn can_control(self) -> bool {
        matches!(self, Self::Admin | Self::Host | Self::Member)
    }
}

/// One team member/device binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamMember {
    /// Device id.
    pub device: DeviceId,
    /// Display name.
    pub name: String,
    /// Role.
    pub role: TeamMemberRole,
    /// Whether this device is currently active in the team.
    pub active: bool,
}

/// Team collaboration policy and membership.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamCollaborationSpace {
    /// Team id.
    pub id: TeamId,
    /// Team label.
    pub name: String,
    /// Max active participants in a team session.
    pub max_session_participants: usize,
    /// Allowed collaboration modes.
    pub allowed_modes: Vec<CollaborationMode>,
    members: Vec<TeamMember>,
}

impl TeamCollaborationSpace {
    /// Create an empty team space.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: TeamId::generate(),
            name: name.into(),
            max_session_participants: 16,
            allowed_modes: vec![
                CollaborationMode::SharedCursor,
                CollaborationMode::PairProgramming,
                CollaborationMode::CollaborativeControl,
            ],
            members: Vec::new(),
        }
    }

    /// Members in stable order.
    #[must_use]
    pub fn members(&self) -> &[TeamMember] {
        &self.members
    }

    /// Add or replace a member by device id.
    pub fn upsert_member(&mut self, member: TeamMember) {
        if let Some(existing) = self
            .members
            .iter_mut()
            .find(|existing| existing.device == member.device)
        {
            *existing = member;
        } else {
            self.members.push(member);
        }
    }

    /// Whether `host` can start a session in `mode` with `participants`.
    #[must_use]
    pub fn can_start_session(
        &self,
        host: DeviceId,
        mode: CollaborationMode,
        participants: &[DeviceId],
    ) -> bool {
        self.allowed_modes.contains(&mode)
            && participants.len() <= self.max_session_participants
            && self
                .member(host)
                .is_some_and(|member| member.active && member.role.can_host())
            && participants.iter().all(|device| {
                self.member(*device)
                    .is_some_and(|member| member.active && member.role.can_control())
            })
    }

    fn member(&self, device: DeviceId) -> Option<&TeamMember> {
        self.members.iter().find(|member| member.device == device)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_sync_requires_https_and_e2e() {
        let config = CloudSyncConfig {
            mode: CloudSyncMode::SelfHosted,
            endpoint: Some("https://sync.example".into()),
            sync_trust_metadata: true,
            sync_workspace_memory: true,
            sync_clipboard_timeline: false,
            require_end_to_end_encryption: true,
        };
        assert!(config.can_upload_user_data());
    }

    #[test]
    fn enterprise_policy_denies_disabled_features() {
        let policy = EnterprisePolicy::unmanaged();
        assert_eq!(
            policy.decide(ManagedFeature::CloudSync),
            PolicyDecision::Deny
        );
        assert_eq!(
            policy.decide(ManagedFeature::RemoteSessions),
            PolicyDecision::Allow
        );
    }

    #[test]
    fn team_space_requires_host_role_and_active_members() {
        let host = DeviceId::generate();
        let member = DeviceId::generate();
        let guest = DeviceId::generate();
        let mut team = TeamCollaborationSpace::new("Core Team");
        team.upsert_member(TeamMember {
            device: host,
            name: "Host".into(),
            role: TeamMemberRole::Host,
            active: true,
        });
        team.upsert_member(TeamMember {
            device: member,
            name: "Member".into(),
            role: TeamMemberRole::Member,
            active: true,
        });
        team.upsert_member(TeamMember {
            device: guest,
            name: "Guest".into(),
            role: TeamMemberRole::Guest,
            active: true,
        });

        assert!(team.can_start_session(host, CollaborationMode::PairProgramming, &[member]));
        assert!(!team.can_start_session(host, CollaborationMode::PairProgramming, &[guest]));
    }
}
