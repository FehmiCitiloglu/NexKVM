//! End-to-end pair-programming driver handoff: a navigator requests the shared
//! cursor, the host grants it (clearing the pending queue), the lease expires,
//! and a fresh request can be denied — all through the public API.

use coklu_core::{
    CollaborationMode, CollaborationParticipant, CollaborationSession, DeviceId, ParticipantRole,
};

fn participant(role: ParticipantRole) -> CollaborationParticipant {
    CollaborationParticipant::new(DeviceId::generate(), format!("{role:?}"), role)
}

#[test]
fn driver_requests_and_receives_then_loses_control() {
    let host = participant(ParticipantRole::Host);
    let host_id = host.id;
    let target = host.device;
    let driver = participant(ParticipantRole::Driver);
    let driver_id = driver.id;

    let mut session = CollaborationSession::new(host, CollaborationMode::PairProgramming);
    session.join(driver).expect("driver joins");

    // Driver asks for the shared cursor; nobody controls yet.
    let request = session
        .request_control(driver_id, target, 100)
        .expect("request allowed");
    assert_eq!(request.requester, driver_id);
    assert_eq!(session.pending_requests().len(), 1);
    assert!(!session.can_control(driver_id, target, 100));

    // Host grants control, which consumes the pending request.
    let lease = session
        .grant_control(host_id, driver_id, target, 110, Some(50))
        .expect("host grants control");
    assert_eq!(lease.holder, driver_id);
    assert!(session.pending_requests().is_empty());
    assert!(session.can_control(driver_id, target, 120));

    // The lease times out and control returns to the host.
    assert!(session.expire_control(200));
    assert!(session.control().is_none());
    assert!(!session.can_control(driver_id, target, 200));

    // A new request can be explicitly denied by the host.
    session
        .request_control(driver_id, target, 210)
        .expect("re-request allowed");
    assert!(
        session
            .deny_control_request(host_id, driver_id)
            .expect("host may deny")
    );
    assert!(session.pending_requests().is_empty());
}
