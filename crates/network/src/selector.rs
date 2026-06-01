//! Transport selection with graceful fallback.

use std::net::SocketAddr;

use crate::error::NetworkError;
use crate::retry::Backoff;
use crate::transport::{Connection, Transport, TransportKind};

/// Picks a working transport by trying registered backends in priority order
/// ([`TransportKind::PRIORITY`]): QUIC, then TCP, then WebRTC.
///
/// Backends are registered explicitly (typically gated by Cargo features at the
/// call site), so this type carries no transport dependencies itself.
#[derive(Default)]
pub struct TransportSelector {
    backends: Vec<Box<dyn Transport>>,
}

impl std::fmt::Debug for TransportSelector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kinds: Vec<TransportKind> = self.backends.iter().map(|t| t.kind()).collect();
        f.debug_struct("TransportSelector")
            .field("backends", &kinds)
            .finish()
    }
}

impl TransportSelector {
    /// Create an empty selector. Register backends with [`Self::register`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a transport backend. Registration order does not matter;
    /// connection attempts follow [`TransportKind::PRIORITY`].
    pub fn register(&mut self, transport: Box<dyn Transport>) -> &mut Self {
        self.backends.push(transport);
        self
    }

    /// Connect to `addr`, trying each registered backend in priority order and
    /// falling back to the next on failure.
    ///
    /// # Errors
    /// Returns [`NetworkError::AllTransportsFailed`] if none succeed, or
    /// [`NetworkError::TransportDisabled`] if no backends are registered.
    pub async fn connect(&self, addr: SocketAddr) -> Result<Box<dyn Connection>, NetworkError> {
        if self.backends.is_empty() {
            return Err(NetworkError::TransportDisabled(TransportKind::Tcp));
        }

        for kind in TransportKind::PRIORITY {
            let Some(backend) = self.backends.iter().find(|t| t.kind() == kind) else {
                continue;
            };
            match backend.connect(addr).await {
                Ok(conn) => return Ok(conn),
                // Try the next priority transport on failure.
                Err(_) => continue,
            }
        }

        Err(NetworkError::AllTransportsFailed)
    }
}

impl TransportSelector {
    /// Connect with automatic retry/recovery, backing off between attempts.
    ///
    /// Tries the full priority fallback ([`Self::connect`]) on each attempt and
    /// sleeps for a jittered [`Backoff`] delay between failures, up to
    /// `max_attempts` (0 = unlimited). On success the backoff is reset so a
    /// later disconnect starts from the initial delay again.
    ///
    /// # Errors
    /// Returns the last [`NetworkError`] if all attempts are exhausted.
    pub async fn connect_with_retry(
        &self,
        addr: SocketAddr,
        backoff: &mut Backoff,
        max_attempts: u32,
    ) -> Result<Box<dyn Connection>, NetworkError> {
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            match self.connect(addr).await {
                Ok(conn) => {
                    backoff.reset();
                    return Ok(conn);
                }
                Err(e) => {
                    if max_attempts != 0 && attempt >= max_attempts {
                        return Err(e);
                    }
                    tokio::time::sleep(backoff.next_delay()).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_selector_reports_disabled() {
        let selector = TransportSelector::new();
        assert!(selector.backends.is_empty());
    }

    #[test]
    fn priority_order_is_quic_tcp_webrtc() {
        assert_eq!(
            TransportKind::PRIORITY,
            [
                TransportKind::Quic,
                TransportKind::Tcp,
                TransportKind::WebRtc
            ]
        );
    }
}
