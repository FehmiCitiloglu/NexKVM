//! Wayland portal-mediated input capture and injection boundary.
//!
//! Native Wayland sessions do not allow global hooks or synthetic input through
//! raw process APIs. This module keeps Linux input integration behind a
//! compositor-mediated portal client: RemoteDesktop grants injection and
//! InputCapture grants captured events. The concrete D-Bus/libei client can
//! implement [`WaylandPortalInputClient`] without changing the daemon-facing
//! [`InputCapture`] and [`InputInjector`] contracts.

use async_trait::async_trait;
use nexkvm_input::{InjectionCommand, InputCapture, InputError, InputEvent, InputInjector};

use crate::PortalAvailability;

/// Input permissions granted by the Wayland portal session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PortalInputGrant {
    /// RemoteDesktop portal permission for compositor-mediated input injection.
    pub remote_desktop: bool,
    /// InputCapture portal permission for compositor-mediated capture.
    pub input_capture: bool,
}

impl PortalInputGrant {
    const REQUIRED: Self = Self {
        remote_desktop: true,
        input_capture: true,
    };

    const fn satisfies(self, required: Self) -> bool {
        (!required.remote_desktop || self.remote_desktop)
            && (!required.input_capture || self.input_capture)
    }
}

/// Client for the Linux Wayland input portals.
#[async_trait]
pub trait WaylandPortalInputClient: Send + Sync {
    /// Request a portal input session with the required capabilities.
    ///
    /// # Errors
    /// Returns [`InputError`] when the compositor denies or cannot provide the
    /// requested portal grants.
    async fn request_input_session(
        &self,
        required: PortalInputGrant,
    ) -> Result<PortalInputGrant, InputError>;

    /// Send one compositor-mediated injection command through the portal.
    ///
    /// # Errors
    /// Returns [`InputError`] if the portal session rejects the command.
    async fn inject(&self, command: InjectionCommand) -> Result<(), InputError>;

    /// Read the next event emitted by the InputCapture portal.
    ///
    /// # Errors
    /// Returns [`InputError`] if the capture stream stops or is unavailable.
    async fn next_event(&self) -> Result<InputEvent, InputError>;
}

/// Daemon-facing Wayland input adapter backed by a portal client.
#[derive(Debug)]
pub struct LinuxWaylandPortalInput<C> {
    client: C,
    grant: PortalInputGrant,
}

impl<C> LinuxWaylandPortalInput<C>
where
    C: WaylandPortalInputClient,
{
    /// Open a Wayland portal input session.
    ///
    /// # Errors
    /// Returns [`InputError::PermissionDenied`] when the session lacks the
    /// required portal interfaces or the compositor grant is incomplete.
    pub async fn connect(portals: PortalAvailability, client: C) -> Result<Self, InputError> {
        if !portals.desktop || !portals.remote_desktop || !portals.input_capture {
            return Err(InputError::PermissionDenied);
        }

        let required = PortalInputGrant::REQUIRED;
        let grant = client.request_input_session(required).await?;
        if !grant.satisfies(required) {
            return Err(InputError::PermissionDenied);
        }

        Ok(Self { client, grant })
    }

    /// Borrow the portal client for observability/testing.
    #[must_use]
    pub const fn client(&self) -> &C {
        &self.client
    }

    /// Portal grants for this session.
    #[must_use]
    pub const fn grant(&self) -> PortalInputGrant {
        self.grant
    }
}

#[async_trait]
impl<C> InputInjector for LinuxWaylandPortalInput<C>
where
    C: WaylandPortalInputClient,
{
    async fn inject(&self, event: InputEvent) -> Result<(), InputError> {
        if !self.grant.remote_desktop {
            return Err(InputError::PermissionDenied);
        }
        self.client.inject(event.to_injection_command()).await
    }
}

#[async_trait]
impl<C> InputCapture for LinuxWaylandPortalInput<C>
where
    C: WaylandPortalInputClient,
{
    async fn next_event(&self) -> Result<InputEvent, InputError> {
        if !self.grant.input_capture {
            return Err(InputError::PermissionDenied);
        }
        self.client.next_event().await
    }
}
