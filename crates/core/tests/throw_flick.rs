//! Integration coverage for the throw / flick interaction.
//!
//! Exercises a realistic three-device topology: a flick from the laptop toward
//! the tablet should "throw" the dragged file across the desktop and land it on
//! the tablet, while a flick into empty space resolves to no target.

use coklu_core::{
    DeviceId, FlickPlanner, FlickVector, ThrowPayload, UnifiedVirtualDesktop, WorkspaceDevice,
    WorkspacePoint, WorkspaceRect,
};

fn topology() -> (UnifiedVirtualDesktop, DeviceId, DeviceId, DeviceId) {
    let laptop = DeviceId::generate();
    let desktop_dev = DeviceId::generate();
    let tablet = DeviceId::generate();

    let mut desktop = UnifiedVirtualDesktop::new();
    desktop.upsert(
        WorkspaceDevice::new(laptop, "Laptop", WorkspaceRect::new(0, 0, 1440, 900))
            .with_online(true),
    );
    desktop.upsert(
        WorkspaceDevice::new(
            desktop_dev,
            "Desktop",
            WorkspaceRect::new(1440, 0, 2560, 1440),
        )
        .with_online(true),
    );
    desktop.upsert(
        WorkspaceDevice::new(tablet, "Tablet", WorkspaceRect::new(0, 900, 1080, 810))
            .with_online(true),
    );
    (desktop, laptop, desktop_dev, tablet)
}

#[test]
fn flick_down_throws_file_onto_the_tablet() {
    let (desktop, laptop, _, tablet) = topology();
    let planner = FlickPlanner::new(desktop);

    let outcome = planner
        .throw(
            laptop,
            WorkspacePoint::new(500, 850),
            FlickVector::new(0.0, 3500.0),
            ThrowPayload::File,
        )
        .expect("downward flick should land on the tablet");

    assert_eq!(outcome.source, laptop);
    assert_eq!(outcome.target, tablet);
    assert_eq!(outcome.payload, ThrowPayload::File);

    let tablet_bounds = WorkspaceRect::new(0, 900, 1080, 810);
    assert!(tablet_bounds.contains(outcome.landing));
}

#[test]
fn flick_into_empty_space_has_no_target() {
    let (desktop, laptop, _, _) = topology();
    let planner = FlickPlanner::new(desktop);

    // Up and to the left of the laptop is empty: no device to catch the throw.
    assert!(
        planner
            .throw(
                laptop,
                WorkspacePoint::new(100, 100),
                FlickVector::new(-2000.0, -2000.0),
                ThrowPayload::Cursor,
            )
            .is_none()
    );
}
