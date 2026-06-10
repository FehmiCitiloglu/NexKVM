//! End-to-end mouse-sharing hand-off: a cursor leaves device A's right edge,
//! drives device B as an absolute pointer, and slides back to re-home on A —
//! exercising boundary detection, multi-monitor mapping, and the focus state
//! machine through the public API.

use nexkvm_core::identity::DeviceId;
use nexkvm_input::{
    CursorFocus, DisplayRect, Edge, EdgeLink, InputEvent, MonitorId, MonitorLayout,
    MouseShareController, ShareOutput,
};
// `BoundaryDetector` is re-exported for constructing the controller.
use nexkvm_input::BoundaryDetector;

fn layout_1080p() -> MonitorLayout {
    MonitorLayout::new(vec![(MonitorId(0), DisplayRect::new(0, 0, 1920, 1080))])
}

#[test]
fn handoff_drives_peer_then_returns_local() {
    let peer_b = DeviceId::generate();

    // Device A: right edge links to device B.
    let boundary = BoundaryDetector::new(
        DisplayRect::new(0, 0, 1920, 1080),
        vec![EdgeLink {
            edge: Edge::Right,
            peer: peer_b,
        }],
    );
    let mut a = MouseShareController::new(boundary, layout_1080p());

    // 1. Cursor inside A's desktop: nothing shared.
    assert_eq!(a.on_local_cursor(800, 540), ShareOutput::Idle);
    assert_eq!(a.focus(), CursorFocus::Local);

    // 2. Cursor crosses A's right edge → focus moves to B, entering B's left edge.
    let entry = match a.on_local_cursor(1920, 270) {
        ShareOutput::EnterRemote(e) => e,
        other => panic!("expected EnterRemote, got {other:?}"),
    };
    assert_eq!(entry.peer, peer_b);
    assert_eq!(a.active_peer(), Some(peer_b));
    // B drops its cursor in at its left edge, quarter-height.
    assert_eq!(
        entry.entry_event(),
        InputEvent::PointerMove { x: 0.0, y: 0.25 }
    );

    // 3. Motion while on B is forwarded to B as absolute positions.
    match a.on_remote_motion(0.4, 0.2) {
        ShareOutput::Forward {
            peer,
            event: InputEvent::PointerMove { x, y },
        } => {
            assert_eq!(peer, peer_b);
            assert!((x - 0.4).abs() < 1e-9);
            assert!((y - 0.45).abs() < 1e-9);
        }
        other => panic!("expected Forward, got {other:?}"),
    }

    // 4. Cursor slides back past B's left edge → focus returns to A and the local
    //    cursor re-homes at A's right edge at the current height.
    match a.on_remote_motion(-0.6, 0.0) {
        ShareOutput::ReturnLocal { x, y } => {
            assert_eq!(x, 1920, "re-home at A's right edge");
            // y ≈ 0.45 * 1080 ≈ 486
            assert!((y - 486).abs() <= 1, "re-home near current height, got {y}");
        }
        other => panic!("expected ReturnLocal, got {other:?}"),
    }
    assert_eq!(a.focus(), CursorFocus::Local);
    assert_eq!(a.active_peer(), None);

    // 5. Back to local: motion is used locally again.
    assert_eq!(a.on_local_cursor(960, 540), ShareOutput::Idle);
}
