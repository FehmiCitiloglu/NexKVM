//! Browser-based remote session planning.
//!
//! Browser clients use WebRTC data channels/media and often require relay/TURN
//! fallback. They must still join through a paired, authenticated device or an
//! explicit short-lived invite. This module models the ticket/policy layer only;
//! no HTTP server, WebRTC agent, or JavaScript runtime lives here.

use coklu_core::identity::DeviceId;

/// Policy for browser remote sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrowserSessionPolicy {
    /// Require an already trusted target device.
    pub require_trusted_target: bool,
    /// Require app-layer encryption in addition to WebRTC DTLS.
    pub require_application_encryption: bool,
    /// Require replay protection on command/input messages.
    pub require_replay_protection: bool,
    /// Maximum ticket lifetime.
    pub max_ticket_ttl_millis: u64,
    /// Whether relay fallback is allowed for browser sessions.
    pub allow_relay: bool,
}

impl BrowserSessionPolicy {
    /// Secure browser remote default.
    #[must_use]
    pub const fn secure_default() -> Self {
        Self {
            require_trusted_target: true,
            require_application_encryption: true,
            require_replay_protection: true,
            max_ticket_ttl_millis: 5 * 60 * 1000,
            allow_relay: true,
        }
    }
}

impl Default for BrowserSessionPolicy {
    fn default() -> Self {
        Self::secure_default()
    }
}

/// Short-lived browser invite/ticket metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserSessionTicket {
    /// Opaque invite id shown as a QR/link token by higher layers.
    pub id: String,
    /// Target device to control/view.
    pub target: DeviceId,
    /// Issuing device.
    pub issuer: DeviceId,
    /// Creation timestamp.
    pub issued_at_millis: u64,
    /// Expiration timestamp.
    pub expires_at_millis: u64,
    /// Whether the ticket has been bound to an authenticated browser session.
    pub claimed: bool,
}

impl BrowserSessionTicket {
    /// Whether the ticket can still be claimed.
    #[must_use]
    pub const fn is_claimable(&self, now_millis: u64) -> bool {
        !self.claimed && now_millis <= self.expires_at_millis
    }
}

/// Planned browser remote session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserRemoteSession {
    /// Browser ticket id.
    pub ticket_id: String,
    /// Target device.
    pub target: DeviceId,
    /// Use relay/TURN fallback.
    pub relay_allowed: bool,
    /// Whether app-layer encryption is mandatory.
    pub application_encryption_required: bool,
    /// Whether replay protection is mandatory.
    pub replay_protection_required: bool,
}

impl BrowserSessionPolicy {
    /// Plan a browser session if the ticket and security posture are valid.
    #[must_use]
    pub fn plan(
        &self,
        ticket: &BrowserSessionTicket,
        now_millis: u64,
        target_trusted: bool,
    ) -> Option<BrowserRemoteSession> {
        if !ticket.is_claimable(now_millis) {
            return None;
        }
        if self.require_trusted_target && !target_trusted {
            return None;
        }
        if ticket
            .expires_at_millis
            .saturating_sub(ticket.issued_at_millis)
            > self.max_ticket_ttl_millis
        {
            return None;
        }
        Some(BrowserRemoteSession {
            ticket_id: ticket.id.clone(),
            target: ticket.target,
            relay_allowed: self.allow_relay,
            application_encryption_required: self.require_application_encryption,
            replay_protection_required: self.require_replay_protection,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticket() -> BrowserSessionTicket {
        BrowserSessionTicket {
            id: "invite".into(),
            target: DeviceId::generate(),
            issuer: DeviceId::generate(),
            issued_at_millis: 100,
            expires_at_millis: 200,
            claimed: false,
        }
    }

    #[test]
    fn plans_secure_browser_session_for_trusted_target() {
        let session = BrowserSessionPolicy::secure_default()
            .plan(&ticket(), 150, true)
            .unwrap();
        assert!(session.application_encryption_required);
        assert!(session.replay_protection_required);
    }

    #[test]
    fn rejects_untrusted_or_expired_ticket() {
        let policy = BrowserSessionPolicy::secure_default();
        assert!(policy.plan(&ticket(), 150, false).is_none());
        assert!(policy.plan(&ticket(), 201, true).is_none());
    }
}
