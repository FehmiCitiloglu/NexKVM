//! Notifications, universal commands, and workspace automation planning.
//!
//! This module owns the platform-neutral control plane for high-level "wow"
//! UX: cross-device notifications, a universal quick command palette, and smart
//! workspace automation. It performs no app launching, notification display, or
//! OS scripting directly. Platform backends and trusted remote peers execute the
//! returned plans through authenticated, encrypted sessions and existing
//! permission checks.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::identity::DeviceId;
use crate::workspace::{AppId, AppLaunchRequest, WorkspaceError};

/// Stable notification id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NotificationId(pub String);

impl NotificationId {
    /// Construct a notification id.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

/// Notification importance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NotificationUrgency {
    /// Ambient; may be batched or hidden.
    Low,
    /// Default visible notification.
    Normal,
    /// Time-sensitive notification.
    High,
}

/// Action attached to a cross-device notification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationAction {
    /// Stable action id within the notification.
    pub id: String,
    /// User-facing label.
    pub label: String,
}

/// Notification mirrored from one trusted device to another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrossDeviceNotification {
    /// Notification id.
    pub id: NotificationId,
    /// Source device.
    pub source: DeviceId,
    /// Target device, or `None` for every eligible trusted peer.
    pub target: Option<DeviceId>,
    /// Source app label or bundle id.
    pub app: Option<String>,
    /// Title.
    pub title: String,
    /// Body text.
    pub body: Option<String>,
    /// Urgency.
    pub urgency: NotificationUrgency,
    /// Optional actions.
    pub actions: Vec<NotificationAction>,
    /// Creation timestamp chosen by caller.
    pub at_millis: u64,
    /// Expiry timestamp; expired notifications should not be displayed.
    pub expires_at_millis: Option<u64>,
}

impl CrossDeviceNotification {
    /// Whether this notification is still useful at `now_millis`.
    #[must_use]
    pub fn is_fresh(&self, now_millis: u64) -> bool {
        self.expires_at_millis
            .is_none_or(|expires_at| now_millis <= expires_at)
    }
}

/// Stable command id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CommandId(pub String);

impl CommandId {
    /// Construct a command id.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

/// Where a quick command may execute.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandScope {
    /// Local device only.
    Local,
    /// Specific trusted device.
    Device(DeviceId),
    /// Any trusted device selected by the user.
    TrustedWorkspace,
}

impl CommandScope {
    fn allows_device(&self, device: Option<DeviceId>) -> bool {
        match (self, device) {
            (Self::Local | Self::TrustedWorkspace, _) => true,
            (Self::Device(expected), Some(actual)) => *expected == actual,
            (Self::Device(_), None) => true,
        }
    }
}

/// Command shown in the universal quick command palette.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuickCommand {
    /// Command id.
    pub id: CommandId,
    /// Primary label.
    pub title: String,
    /// Secondary context.
    pub subtitle: Option<String>,
    /// Execution scope.
    pub scope: CommandScope,
    /// Search aliases.
    pub keywords: Vec<String>,
    /// Whether policy currently allows execution.
    pub enabled: bool,
}

impl QuickCommand {
    /// Construct a local command.
    #[must_use]
    pub fn local(id: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            id: CommandId::new(id),
            title: title.into(),
            subtitle: None,
            scope: CommandScope::Local,
            keywords: Vec::new(),
            enabled: true,
        }
    }
}

/// In-memory command palette index.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandPaletteIndex {
    commands: Vec<QuickCommand>,
}

impl CommandPaletteIndex {
    /// Create an empty index.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Commands in insertion/update order.
    #[must_use]
    pub fn commands(&self) -> &[QuickCommand] {
        &self.commands
    }

    /// Add or replace a command by id.
    pub fn upsert(&mut self, command: QuickCommand) {
        if let Some(existing) = self
            .commands
            .iter_mut()
            .find(|existing| existing.id == command.id)
        {
            *existing = command;
        } else {
            self.commands.push(command);
        }
    }

    /// Remove a command by id.
    pub fn remove(&mut self, id: &CommandId) -> bool {
        if let Some(index) = self.commands.iter().position(|command| &command.id == id) {
            self.commands.remove(index);
            true
        } else {
            false
        }
    }

    /// Search enabled commands by title, subtitle, or keyword.
    #[must_use]
    pub fn search(&self, query: &str, device: Option<DeviceId>, limit: usize) -> Vec<QuickCommand> {
        let needle = query.to_ascii_lowercase();
        let mut matches: Vec<_> = self
            .commands
            .iter()
            .filter(|command| command.enabled && command.scope.allows_device(device))
            .filter_map(|command| {
                command_score(command, &needle).map(|score| (score, command.clone()))
            })
            .collect();
        matches.sort_by_key(|(score, command)| (std::cmp::Reverse(*score), command.title.clone()));
        matches
            .into_iter()
            .take(limit)
            .map(|(_, command)| command)
            .collect()
    }
}

/// Errors from command execution.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CommandError {
    /// Command was unknown.
    #[error("command not found: {0}")]
    NotFound(String),
    /// Policy denied execution.
    #[error("command permission denied: {0}")]
    PermissionDenied(&'static str),
    /// Backend failed.
    #[error("command backend error: {0}")]
    Backend(String),
}

/// Executes quick commands after policy/trust validation.
#[async_trait]
pub trait QuickCommandExecutor: Send + Sync {
    /// Execute a command.
    ///
    /// # Errors
    /// Returns [`CommandError`] when the command is unknown, denied, or failed.
    async fn execute(&self, command: CommandId) -> Result<(), CommandError>;
}

/// Trigger that can activate an automation rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AutomationTrigger {
    /// A device became active/nearby enough according to discovery presence.
    DeviceBecameActive(DeviceId),
    /// A quick command was invoked.
    CommandInvoked(CommandId),
    /// A specific app became foreground on a device.
    AppFocused { device: DeviceId, app: AppId },
}

/// Action emitted by a smart workspace automation rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AutomationAction {
    /// Route input/focus to a device.
    SwitchToDevice(DeviceId),
    /// Launch an app through the workspace backend.
    LaunchApp(AppLaunchRequest),
    /// Show or forward a notification.
    SendNotification(CrossDeviceNotification),
    /// Invoke another quick command.
    InvokeCommand(CommandId),
}

/// One policy-controlled automation rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutomationRule {
    /// Stable rule id.
    pub id: String,
    /// User-facing label.
    pub name: String,
    /// Trigger.
    pub trigger: AutomationTrigger,
    /// Planned action.
    pub action: AutomationAction,
    /// Whether the rule can fire.
    pub enabled: bool,
}

/// Planned action produced by an automation rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutomationPlan {
    /// Rule that fired.
    pub rule_id: String,
    /// Action to execute through platform/network backends.
    pub action: AutomationAction,
}

/// Sans-IO automation planner.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutomationEngine {
    rules: Vec<AutomationRule>,
}

impl AutomationEngine {
    /// Create an empty automation engine.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add or replace a rule by id.
    pub fn upsert(&mut self, rule: AutomationRule) -> Result<(), WorkspaceError> {
        if rule.id.is_empty() {
            return Err(WorkspaceError::InvalidInput(
                "automation rule id cannot be empty",
            ));
        }
        if let Some(existing) = self
            .rules
            .iter_mut()
            .find(|existing| existing.id == rule.id)
        {
            *existing = rule;
        } else {
            self.rules.push(rule);
        }
        Ok(())
    }

    /// Plan actions for a trigger.
    #[must_use]
    pub fn plan(&self, trigger: &AutomationTrigger) -> Vec<AutomationPlan> {
        self.rules
            .iter()
            .filter(|rule| rule.enabled && &rule.trigger == trigger)
            .map(|rule| AutomationPlan {
                rule_id: rule.id.clone(),
                action: rule.action.clone(),
            })
            .collect()
    }
}

fn command_score(command: &QuickCommand, needle: &str) -> Option<u16> {
    if needle.is_empty() {
        return Some(1);
    }
    let title = command.title.to_ascii_lowercase();
    let subtitle_hit = command
        .subtitle
        .as_ref()
        .is_some_and(|subtitle| subtitle.to_ascii_lowercase().contains(needle));
    let keyword_hit = command
        .keywords
        .iter()
        .any(|keyword| keyword.to_ascii_lowercase().contains(needle));
    if title == needle {
        Some(100)
    } else if title.contains(needle) {
        Some(80)
    } else if keyword_hit {
        Some(60)
    } else if subtitle_hit {
        Some(40)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_palette_search_ranks_enabled_matches() {
        let mut index = CommandPaletteIndex::new();
        let mut command = QuickCommand::local("open-settings", "Open Settings");
        command.keywords.push("preferences".into());
        index.upsert(command);
        index.upsert(QuickCommand {
            enabled: false,
            ..QuickCommand::local("hidden", "Open Hidden")
        });

        let results = index.search("pref", None, 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, CommandId::new("open-settings"));
    }

    #[test]
    fn automation_plans_matching_enabled_rules() {
        let device = DeviceId::generate();
        let mut engine = AutomationEngine::new();
        engine
            .upsert(AutomationRule {
                id: "focus-nearby".into(),
                name: "Focus nearby device".into(),
                trigger: AutomationTrigger::DeviceBecameActive(device),
                action: AutomationAction::SwitchToDevice(device),
                enabled: true,
            })
            .unwrap();

        let plans = engine.plan(&AutomationTrigger::DeviceBecameActive(device));
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].rule_id, "focus-nearby");
    }

    #[test]
    fn expired_notifications_are_not_fresh() {
        let notification = CrossDeviceNotification {
            id: NotificationId::new("n1"),
            source: DeviceId::generate(),
            target: None,
            app: None,
            title: "Done".into(),
            body: None,
            urgency: NotificationUrgency::Normal,
            actions: Vec::new(),
            at_millis: 10,
            expires_at_millis: Some(20),
        };
        assert!(notification.is_fresh(20));
        assert!(!notification.is_fresh(21));
    }
}
