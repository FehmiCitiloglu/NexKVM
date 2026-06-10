//! Self-hosted relay server policy and route planning.
//!
//! Relay servers are fallback infrastructure for hostile NATs, browser remote
//! sessions, and decentralized meshes. The relay is not a trust root: payloads
//! remain end-to-end encrypted and replay-protected by paired device sessions.

use std::net::SocketAddr;

use nexkvm_core::identity::DeviceId;

/// Relay deployment kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayServerKind {
    /// User/team operated relay.
    SelfHosted,
    /// Managed cloud relay.
    ManagedCloud,
}

/// Admission policy for a relay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayPolicy {
    /// Relay kind.
    pub kind: RelayServerKind,
    /// Public relay endpoint.
    pub endpoint: SocketAddr,
    /// Require TLS to the relay endpoint.
    pub require_tls: bool,
    /// Require end-to-end app encryption between devices.
    pub require_end_to_end_encryption: bool,
    /// Maximum connected devices admitted by this policy.
    pub max_devices: usize,
    /// Whether browser/WebRTC clients may use this relay.
    pub allow_browser_sessions: bool,
}

impl RelayPolicy {
    /// Secure default for self-hosted relays.
    #[must_use]
    pub const fn self_hosted(endpoint: SocketAddr) -> Self {
        Self {
            kind: RelayServerKind::SelfHosted,
            endpoint,
            require_tls: true,
            require_end_to_end_encryption: true,
            max_devices: 64,
            allow_browser_sessions: true,
        }
    }

    /// Whether the relay policy keeps the relay out of the trust boundary.
    #[must_use]
    pub const fn is_secure(&self) -> bool {
        self.require_tls && self.require_end_to_end_encryption
    }
}

/// Device registration request for a relay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayRegistration {
    /// Device requesting relay admission.
    pub device: DeviceId,
    /// Whether the device is trusted/pairing-authenticated locally.
    pub trusted: bool,
    /// Whether the session has app-layer encryption.
    pub end_to_end_encrypted: bool,
}

/// Admission decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayAdmission {
    /// Registration accepted.
    Accepted,
    /// Registration denied with reason.
    Denied(&'static str),
}

/// Relay route between two trusted devices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayRoutePlan {
    /// Relay endpoint.
    pub relay: SocketAddr,
    /// Source device.
    pub from: DeviceId,
    /// Destination device.
    pub to: DeviceId,
    /// Whether the relay may inspect payload plaintext. Must stay false.
    pub relay_can_decrypt: bool,
}

impl RelayPolicy {
    /// Evaluate a relay registration.
    #[must_use]
    pub fn admit(
        &self,
        current_devices: usize,
        registration: &RelayRegistration,
    ) -> RelayAdmission {
        if !self.is_secure() {
            return RelayAdmission::Denied(
                "relay policy must require TLS and end-to-end encryption",
            );
        }
        if current_devices >= self.max_devices {
            return RelayAdmission::Denied("relay capacity reached");
        }
        if !registration.trusted {
            return RelayAdmission::Denied("device is not trusted");
        }
        if !registration.end_to_end_encrypted {
            return RelayAdmission::Denied("session is not end-to-end encrypted");
        }
        RelayAdmission::Accepted
    }

    /// Plan a relay route for already-admitted trusted devices.
    #[must_use]
    pub fn route(&self, from: DeviceId, to: DeviceId) -> Option<RelayRoutePlan> {
        self.is_secure().then_some(RelayRoutePlan {
            relay: self.endpoint,
            from,
            to,
            relay_can_decrypt: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint() -> SocketAddr {
        ([203, 0, 113, 30], 443).into()
    }

    #[test]
    fn self_hosted_policy_requires_trusted_encrypted_devices() {
        let policy = RelayPolicy::self_hosted(endpoint());
        let registration = RelayRegistration {
            device: DeviceId::generate(),
            trusted: true,
            end_to_end_encrypted: true,
        };
        assert_eq!(policy.admit(0, &registration), RelayAdmission::Accepted);
    }

    #[test]
    fn relay_never_decrypts_route_payload() {
        let policy = RelayPolicy::self_hosted(endpoint());
        let route = policy
            .route(DeviceId::generate(), DeviceId::generate())
            .unwrap();
        assert!(!route.relay_can_decrypt);
    }
}
