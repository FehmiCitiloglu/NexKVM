//! Remote-session establishment handshake for internet-based device linking.
//!
//! [`InternetConnectivityPlanner`](crate::InternetConnectivityPlanner) decides
//! *how* to reach a peer (direct ICE vs relay); [`RemoteSessionPolicy`] decides
//! *whether* a remote link is allowed. This module is the sans-IO glue that ties
//! them together into an authenticated **offer/answer** exchange two devices run
//! over a signaling channel (the relay control lane or any out-of-band path)
//! before any media flows.
//!
//! Security boundary (mirrors [`docs/security.md`]): a remote session is only
//! established with an **already-trusted** device, and only when both sides
//! require application-layer encryption *and* replay protection. The answerer
//! validates the incoming offer against its local policy and **rejects**
//! anything weaker — an untrusted or downgraded offer never yields a plan. This
//! module performs no I/O and holds no keys; the actual DTLS/transport
//! encryption and replay protection are enforced by the `crypto` session layer.

use coklu_core::identity::DeviceId;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::internet::{
    ConnectivityPlan, InternetCandidate, InternetConnectivityPlanner, NatType, RemoteSessionPolicy,
};

/// Opaque, unguessable identifier correlating an offer with its answer.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RemoteSessionId(pub String);

impl RemoteSessionId {
    /// Wrap an externally-generated id (e.g. a random token from `crypto`).
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

/// Mandatory security guarantees advertised in an offer / required by a peer.
///
/// These are wire-exchanged so each side can confirm the other will not
/// downgrade the session below the local policy minimum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSecurityRequirements {
    /// Application-layer encryption is required in addition to transport DTLS.
    pub require_application_encryption: bool,
    /// Replay protection must be enforced by the crypto session.
    pub require_replay_protection: bool,
}

impl SessionSecurityRequirements {
    /// Derive the requirements implied by a [`RemoteSessionPolicy`].
    #[must_use]
    pub const fn from_policy(policy: &RemoteSessionPolicy) -> Self {
        Self {
            require_application_encryption: policy.require_application_encryption,
            require_replay_protection: policy.require_replay_protection,
        }
    }

    /// Whether `self` meets or exceeds every requirement in `minimum`.
    #[must_use]
    pub const fn satisfies(&self, minimum: &Self) -> bool {
        (self.require_application_encryption || !minimum.require_application_encryption)
            && (self.require_replay_protection || !minimum.require_replay_protection)
    }
}

/// Offer sent by the initiating device over the signaling channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteSessionOffer {
    /// Correlates this offer with its answer.
    pub session_id: RemoteSessionId,
    /// Device initiating the session.
    pub offerer: DeviceId,
    /// Intended recipient device.
    pub target: DeviceId,
    /// Locally gathered connectivity candidates.
    pub candidates: Vec<InternetCandidate>,
    /// Offerer's NAT estimate.
    pub nat: NatType,
    /// Security guarantees the offerer will enforce.
    pub security: SessionSecurityRequirements,
}

/// Answer returned by the recipient device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemoteSessionAnswer {
    /// Offer accepted; the answerer's candidates and NAT estimate are included.
    Accept {
        /// Correlated session id.
        session_id: RemoteSessionId,
        /// Device accepting the session.
        answerer: DeviceId,
        /// Answerer's connectivity candidates.
        candidates: Vec<InternetCandidate>,
        /// Answerer's NAT estimate.
        nat: NatType,
    },
    /// Offer rejected with a stable machine-readable reason.
    Reject {
        /// Correlated session id.
        session_id: RemoteSessionId,
        /// Why the offer was refused.
        reason: RejectReason,
    },
}

impl RemoteSessionAnswer {
    /// The session id this answer correlates with.
    #[must_use]
    pub fn session_id(&self) -> &RemoteSessionId {
        match self {
            Self::Accept { session_id, .. } | Self::Reject { session_id, .. } => session_id,
        }
    }
}

/// Reason an offer was rejected by the answerer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RejectReason {
    /// The offer was addressed to a different device.
    WrongTarget,
    /// The offerer is not a trusted/paired device.
    UntrustedPeer,
    /// The offer's security guarantees are weaker than local policy requires.
    InsufficientSecurity,
}

/// Errors driving the offerer side of the handshake.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RemoteSessionError {
    /// Local policy is not secure enough to initiate a remote session.
    #[error("remote session policy is not secure (trust + encryption + replay required)")]
    InsecurePolicy,

    /// The answer's session id did not match the pending offer.
    #[error("session id mismatch between offer and answer")]
    SessionMismatch,

    /// The answer came from a device other than the offer's target.
    #[error("answer came from an unexpected device")]
    UnexpectedAnswerer,

    /// The peer rejected the offer.
    #[error("peer rejected remote session: {0:?}")]
    Rejected(RejectReason),

    /// No viable connectivity path after combining both sides' candidates.
    #[error("no viable connectivity path for remote session")]
    NoConnectivity,
}

/// State of an offerer-side remote session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteSessionState {
    /// Offer created, awaiting an answer.
    AwaitingAnswer,
    /// Established with a chosen connectivity plan.
    Established(ConnectivityPlan),
    /// Terminated (rejected or failed) with a reason.
    Failed(RemoteSessionError),
}

/// Offerer-side driver: creates the offer, then turns a peer's answer into a
/// validated [`ConnectivityPlan`].
#[derive(Debug, Clone)]
pub struct RemoteSessionEstablisher {
    policy: RemoteSessionPolicy,
    planner: InternetConnectivityPlanner,
    offer: RemoteSessionOffer,
    state: RemoteSessionState,
}

impl RemoteSessionEstablisher {
    /// Create an offer to link to the policy's peer.
    ///
    /// # Errors
    /// Returns [`RemoteSessionError::InsecurePolicy`] if `policy` does not
    /// require trust, encryption, and replay protection.
    pub fn offer(
        session_id: RemoteSessionId,
        local_device: DeviceId,
        policy: RemoteSessionPolicy,
        planner: InternetConnectivityPlanner,
        local_candidates: Vec<InternetCandidate>,
        local_nat: NatType,
    ) -> Result<Self, RemoteSessionError> {
        if !policy.is_secure() {
            return Err(RemoteSessionError::InsecurePolicy);
        }
        let offer = RemoteSessionOffer {
            session_id,
            offerer: local_device,
            target: policy.peer,
            candidates: local_candidates,
            nat: local_nat,
            security: SessionSecurityRequirements::from_policy(&policy),
        };
        Ok(Self {
            policy,
            planner,
            offer,
            state: RemoteSessionState::AwaitingAnswer,
        })
    }

    /// The offer to transmit over the signaling channel.
    #[must_use]
    pub fn pending_offer(&self) -> &RemoteSessionOffer {
        &self.offer
    }

    /// Current session state.
    #[must_use]
    pub fn state(&self) -> &RemoteSessionState {
        &self.state
    }

    /// Consume the peer's answer and, if accepted, compute the connectivity
    /// plan from both sides' candidates.
    ///
    /// # Errors
    /// Returns a [`RemoteSessionError`] if the answer is mismatched, from the
    /// wrong device, a rejection, or yields no viable path.
    pub fn accept_answer(
        &mut self,
        answer: RemoteSessionAnswer,
    ) -> Result<&ConnectivityPlan, RemoteSessionError> {
        let result = self.evaluate_answer(answer);
        match result {
            Ok(plan) => {
                self.state = RemoteSessionState::Established(plan);
                let RemoteSessionState::Established(plan) = &self.state else {
                    unreachable!("just assigned Established");
                };
                Ok(plan)
            }
            Err(err) => {
                self.state = RemoteSessionState::Failed(err.clone());
                Err(err)
            }
        }
    }

    fn evaluate_answer(
        &self,
        answer: RemoteSessionAnswer,
    ) -> Result<ConnectivityPlan, RemoteSessionError> {
        if answer.session_id() != &self.offer.session_id {
            return Err(RemoteSessionError::SessionMismatch);
        }
        let (answerer, candidates, nat) = match answer {
            RemoteSessionAnswer::Reject { reason, .. } => {
                return Err(RemoteSessionError::Rejected(reason));
            }
            RemoteSessionAnswer::Accept {
                answerer,
                candidates,
                nat,
                ..
            } => (answerer, candidates, nat),
        };
        if answerer != self.policy.peer {
            return Err(RemoteSessionError::UnexpectedAnswerer);
        }
        // Combine both sides' candidates; the worse of the two NAT estimates
        // drives the direct-vs-relay decision.
        let mut combined = self.offer.candidates.clone();
        combined.extend(candidates);
        let nat = worse_nat(self.offer.nat, nat);
        match self.planner.plan(nat, combined) {
            ConnectivityPlan::Unavailable => Err(RemoteSessionError::NoConnectivity),
            plan => Ok(plan),
        }
    }
}

/// Answerer-side validation: decide whether to accept an incoming offer.
///
/// This is the security gate. The caller supplies whether the offerer is a
/// locally trusted/paired device; an untrusted peer, a misdirected offer, or a
/// security downgrade is rejected without producing any plan.
#[must_use]
pub fn answer_offer(
    offer: &RemoteSessionOffer,
    local_device: DeviceId,
    local_policy: &RemoteSessionPolicy,
    offerer_is_trusted: bool,
    local_candidates: Vec<InternetCandidate>,
    local_nat: NatType,
) -> RemoteSessionAnswer {
    let reject = |reason| RemoteSessionAnswer::Reject {
        session_id: offer.session_id.clone(),
        reason,
    };

    if offer.target != local_device {
        return reject(RejectReason::WrongTarget);
    }
    if local_policy.require_trusted_device && !offerer_is_trusted {
        return reject(RejectReason::UntrustedPeer);
    }
    let minimum = SessionSecurityRequirements::from_policy(local_policy);
    if !offer.security.satisfies(&minimum) {
        return reject(RejectReason::InsufficientSecurity);
    }
    RemoteSessionAnswer::Accept {
        session_id: offer.session_id.clone(),
        answerer: local_device,
        candidates: local_candidates,
        nat: local_nat,
    }
}

/// Pick the more constrained of two NAT estimates (relay-favoring).
fn worse_nat(a: NatType, b: NatType) -> NatType {
    fn rank(nat: NatType) -> u8 {
        match nat {
            NatType::OpenInternet => 0,
            NatType::Cone => 1,
            NatType::Restricted => 2,
            NatType::Unknown => 3,
            NatType::Symmetric => 4,
            NatType::UdpBlocked => 5,
        }
    }
    if rank(a) >= rank(b) { a } else { b }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::internet::{CandidateKind, RelayConfig, WebRtcConfig};
    use std::net::SocketAddr;

    fn addr(port: u16) -> SocketAddr {
        ([203, 0, 113, 7], port).into()
    }

    fn planner_with_relay() -> InternetConnectivityPlanner {
        InternetConnectivityPlanner::new(WebRtcConfig {
            relay: Some(RelayConfig {
                endpoint: addr(443),
                require_tls: true,
            }),
            ..WebRtcConfig::default()
        })
    }

    fn host(port: u16) -> InternetCandidate {
        InternetCandidate::new(CandidateKind::ServerReflexive, addr(port), 10)
    }

    #[test]
    fn rejects_insecure_local_policy() {
        let local = DeviceId::generate();
        let peer = DeviceId::generate();
        let mut policy = RemoteSessionPolicy::trusted_encrypted(peer);
        policy.require_replay_protection = false;
        let err = RemoteSessionEstablisher::offer(
            RemoteSessionId::new("s1"),
            local,
            policy,
            planner_with_relay(),
            vec![host(4000)],
            NatType::Cone,
        )
        .unwrap_err();
        assert_eq!(err, RemoteSessionError::InsecurePolicy);
    }

    #[test]
    fn answerer_rejects_untrusted_peer() {
        let offerer = DeviceId::generate();
        let answerer = DeviceId::generate();
        let policy = RemoteSessionPolicy::trusted_encrypted(offerer);
        let offer = RemoteSessionOffer {
            session_id: RemoteSessionId::new("s2"),
            offerer,
            target: answerer,
            candidates: vec![host(4000)],
            nat: NatType::Cone,
            security: SessionSecurityRequirements::from_policy(&policy),
        };
        let answer = answer_offer(
            &offer,
            answerer,
            &RemoteSessionPolicy::trusted_encrypted(offerer),
            false, // not trusted
            vec![host(5000)],
            NatType::Cone,
        );
        assert!(matches!(
            answer,
            RemoteSessionAnswer::Reject {
                reason: RejectReason::UntrustedPeer,
                ..
            }
        ));
    }

    #[test]
    fn answerer_rejects_security_downgrade() {
        let offerer = DeviceId::generate();
        let answerer = DeviceId::generate();
        let offer = RemoteSessionOffer {
            session_id: RemoteSessionId::new("s3"),
            offerer,
            target: answerer,
            candidates: vec![host(4000)],
            nat: NatType::Cone,
            // Offerer advertises no replay protection.
            security: SessionSecurityRequirements {
                require_application_encryption: true,
                require_replay_protection: false,
            },
        };
        let answer = answer_offer(
            &offer,
            answerer,
            &RemoteSessionPolicy::trusted_encrypted(offerer),
            true,
            vec![host(5000)],
            NatType::Cone,
        );
        assert!(matches!(
            answer,
            RemoteSessionAnswer::Reject {
                reason: RejectReason::InsufficientSecurity,
                ..
            }
        ));
    }

    #[test]
    fn full_handshake_yields_direct_plan() {
        let offerer = DeviceId::generate();
        let answerer = DeviceId::generate();
        let mut establisher = RemoteSessionEstablisher::offer(
            RemoteSessionId::new("s4"),
            offerer,
            RemoteSessionPolicy::trusted_encrypted(answerer),
            planner_with_relay(),
            vec![host(4000)],
            NatType::Cone,
        )
        .unwrap();
        assert_eq!(establisher.state(), &RemoteSessionState::AwaitingAnswer);

        let answer = answer_offer(
            establisher.pending_offer(),
            answerer,
            &RemoteSessionPolicy::trusted_encrypted(offerer),
            true,
            vec![host(5000)],
            NatType::Cone,
        );
        let plan = establisher.accept_answer(answer).unwrap().clone();
        assert!(matches!(plan, ConnectivityPlan::WebRtcDirect { .. }));
        assert!(matches!(
            establisher.state(),
            RemoteSessionState::Established(_)
        ));
    }

    #[test]
    fn symmetric_nat_handshake_falls_back_to_relay() {
        let offerer = DeviceId::generate();
        let answerer = DeviceId::generate();
        let mut establisher = RemoteSessionEstablisher::offer(
            RemoteSessionId::new("s5"),
            offerer,
            RemoteSessionPolicy::trusted_encrypted(answerer),
            planner_with_relay(),
            vec![host(4000)],
            NatType::Symmetric,
        )
        .unwrap();
        let answer = answer_offer(
            establisher.pending_offer(),
            answerer,
            &RemoteSessionPolicy::trusted_encrypted(offerer),
            true,
            vec![host(5000)],
            NatType::Cone,
        );
        let plan = establisher.accept_answer(answer).unwrap().clone();
        assert!(matches!(plan, ConnectivityPlan::Relay { .. }));
    }

    #[test]
    fn rejection_answer_fails_establisher() {
        let offerer = DeviceId::generate();
        let answerer = DeviceId::generate();
        let mut establisher = RemoteSessionEstablisher::offer(
            RemoteSessionId::new("s6"),
            offerer,
            RemoteSessionPolicy::trusted_encrypted(answerer),
            planner_with_relay(),
            vec![host(4000)],
            NatType::Cone,
        )
        .unwrap();
        let answer = RemoteSessionAnswer::Reject {
            session_id: RemoteSessionId::new("s6"),
            reason: RejectReason::UntrustedPeer,
        };
        let err = establisher.accept_answer(answer).unwrap_err();
        assert_eq!(
            err,
            RemoteSessionError::Rejected(RejectReason::UntrustedPeer)
        );
        assert!(matches!(establisher.state(), RemoteSessionState::Failed(_)));
    }

    #[test]
    fn mismatched_session_id_is_rejected() {
        let offerer = DeviceId::generate();
        let answerer = DeviceId::generate();
        let mut establisher = RemoteSessionEstablisher::offer(
            RemoteSessionId::new("s7"),
            offerer,
            RemoteSessionPolicy::trusted_encrypted(answerer),
            planner_with_relay(),
            vec![host(4000)],
            NatType::Cone,
        )
        .unwrap();
        let answer = RemoteSessionAnswer::Accept {
            session_id: RemoteSessionId::new("WRONG"),
            answerer,
            candidates: vec![host(5000)],
            nat: NatType::Cone,
        };
        assert_eq!(
            establisher.accept_answer(answer).unwrap_err(),
            RemoteSessionError::SessionMismatch
        );
    }
}
