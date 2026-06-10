//! End-to-end spatial desktop camera: build a multi-device topology, zoom to fit
//! it, scroll-zoom toward a device, pan, and pick the device under the cursor —
//! the control-plane flow a zoomable spatial-navigation UI drives, all through
//! the public API.

use nexkvm_core::{
    DeviceId, ScreenPoint, SpatialViewport, UnifiedVirtualDesktop, ViewportSize, WorkspaceDevice,
    WorkspacePoint, WorkspaceRect,
};

#[test]
fn fit_then_zoom_and_pan_locates_device_under_cursor() {
    let laptop = DeviceId::generate();
    let desktop_dev = DeviceId::generate();
    let tablet = DeviceId::generate();

    let mut topology = UnifiedVirtualDesktop::new();
    topology.upsert(
        WorkspaceDevice::new(laptop, "Laptop", WorkspaceRect::new(0, 0, 1440, 900))
            .with_online(true),
    );
    topology.upsert(
        WorkspaceDevice::new(
            desktop_dev,
            "Desktop",
            WorkspaceRect::new(1440, 0, 2560, 1440),
        )
        .with_online(true),
    );
    topology.upsert(
        WorkspaceDevice::new(tablet, "Tablet", WorkspaceRect::new(4000, 200, 1180, 820))
            .with_online(true),
    );

    let bounds = topology.bounds().expect("topology has devices");
    let mut camera = SpatialViewport::new(ViewportSize::new(1280.0, 720.0));

    // Zoom to fit the entire topology with a 10% margin.
    camera.fit_rect(bounds, 0.1);
    assert_eq!(camera.center(), bounds.center());

    // The desktop's center projects somewhere inside the viewport.
    let desktop_center = topology.device(desktop_dev).unwrap().bounds.center();
    let desktop_screen = camera.world_to_screen(desktop_center);
    assert!(desktop_screen.x >= 0.0 && desktop_screen.x <= 1280.0);
    assert!(desktop_screen.y >= 0.0 && desktop_screen.y <= 720.0);

    // Scroll-zoom in toward the desktop; the world point under that screen spot
    // must remain anchored.
    let anchor = camera.world_to_screen(desktop_center);
    camera.zoom_by(3.0, anchor);
    assert_eq!(camera.screen_to_world(anchor), desktop_center);

    // Pan so the tablet's center lands at the viewport middle, then confirm a
    // hit-test at the viewport center resolves to the tablet.
    let tablet_center = topology.device(tablet).unwrap().bounds.center();
    let mid = ScreenPoint::new(640.0, 360.0);
    let tablet_screen = camera.world_to_screen(tablet_center);
    camera.pan_by_screen(mid.x - tablet_screen.x, mid.y - tablet_screen.y);

    let world_at_mid = camera.screen_to_world(mid);
    let hit = topology
        .device_at(WorkspacePoint::new(world_at_mid.x, world_at_mid.y))
        .expect("a device sits under the viewport center");
    assert_eq!(hit.device, tablet);
}
