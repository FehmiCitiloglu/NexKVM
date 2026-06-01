//! End-to-end remote-session linking: two devices run the offer/answer handshake
//! over a simulated signaling channel (serialized round-trip), negotiate
//! connectivity, and a security downgrade is refused — all through the public
//! API.

use coklu_core::identity::DeviceId;
use coklu_network::internet::{CandidateKind, InternetCandidate, RelayConfig, WebRtcConfig};
use coklu_network::{
    ConnectivityPlan, InternetConnectivityPlanner, NatType, RemoteSessionAnswer,
    RemoteSessionEstablisher, RemoteSessionId, RemoteSessionPolicy, answer_offer,
};

fn candidate(port: u16, kind: CandidateKind) -> InternetCandidate {
    InternetCandidate::new(kind, ([198, 51, 100, 5], port).into(), 10)
}

fn relay_planner() -> InternetConnectivityPlanner {
    InternetConnectivityPlanner::new(WebRtcConfig {
        relay: Some(RelayConfig {
            endpoint: ([198, 51, 100, 1], 443).into(),
            require_tls: true,
        }),
        ..WebRtcConfig::default()
    })
}

/// Two trusted devices link over the internet: the offer and answer are
/// serialized across a simulated signaling hop, and the offerer ends up with a
/// direct WebRTC plan.
#[test]
fn trusted_devices_link_over_simulated_signaling() {
    let device_a = DeviceId::generate();
    let device_b = DeviceId::generate();

    // A creates an offer for B.
    let mut establisher = RemoteSessionEstablisher::offer(
        RemoteSessionId::new("link-1"),
        device_a,
        RemoteSessionPolicy::trusted_encrypted(device_b),
        relay_planner(),
        vec![candidate(4000, CandidateKind::ServerReflexive)],
        NatType::Cone,
    )
    .expect("secure policy yields an offer");

    // Offer crosses the signaling channel as JSON.
    let offer_wire = serde_json::to_vec(establisher.pending_offer()).unwrap();
    let offer_received: coklu_network::RemoteSessionOffer =
        serde_json::from_slice(&offer_wire).unwrap();

    // B validates against its own policy (A is trusted) and answers.
    let answer = answer_offer(
        &offer_received,
        device_b,
        &RemoteSessionPolicy::trusted_encrypted(device_a),
        true,
        vec![candidate(5000, CandidateKind::ServerReflexive)],
        NatType::Cone,
    );
    assert!(matches!(answer, RemoteSessionAnswer::Accept { .. }));

    // Answer crosses back over the wire.
    let answer_wire = serde_json::to_vec(&answer).unwrap();
    let answer_received: RemoteSessionAnswer = serde_json::from_slice(&answer_wire).unwrap();

    // A finalizes the link into a connectivity plan.
    let plan = establisher.accept_answer(answer_received).unwrap();
    assert!(matches!(plan, ConnectivityPlan::WebRtcDirect { .. }));
}

/// An untrusted offerer is refused at the answerer's security gate, so no
/// session is established.
#[test]
fn untrusted_peer_cannot_link() {
    let device_a = DeviceId::generate();
    let device_b = DeviceId::generate();

    let establisher = RemoteSessionEstablisher::offer(
        RemoteSessionId::new("link-2"),
        device_a,
        RemoteSessionPolicy::trusted_encrypted(device_b),
        relay_planner(),
        vec![candidate(4000, CandidateKind::ServerReflexive)],
        NatType::Cone,
    )
    .unwrap();

    // B does NOT trust A.
    let answer = answer_offer(
        establisher.pending_offer(),
        device_b,
        &RemoteSessionPolicy::trusted_encrypted(device_a),
        false,
        vec![candidate(5000, CandidateKind::ServerReflexive)],
        NatType::Cone,
    );
    assert!(matches!(answer, RemoteSessionAnswer::Reject { .. }));
}
