//! End-to-end hover preview: a viewer hovers a window, the owner captures a
//! low-res thumbnail, negotiates a preview stream, caches it, and serves the
//! cache on the next hover — all through the public API.

use bytes::Bytes;
use coklu_core::identity::DeviceId;
use coklu_streaming::{
    CaptureSource, CaptureSourceId, EncodedScreenFrame, FrameDependency, HardwareEncoder,
    HoverPreviewController, PreviewDecision, PreviewPolicy, ScreenCodec, ScreenFrameType,
    ScreenResolution, ScreenStreamCapabilities, ScreenStreamIntent, WindowVisibility,
    negotiate_screen_stream,
};

fn window(id: &str) -> CaptureSource {
    CaptureSource::Window {
        id: CaptureSourceId::new(id),
        title: "Browser".into(),
        app_id: Some("com.example.browser".into()),
        visibility: WindowVisibility::Visible,
    }
}

#[test]
fn hover_captures_then_serves_cached_thumbnail() {
    let owner = DeviceId::generate();
    let viewer = DeviceId::generate();
    let mut controller = HoverPreviewController::new(owner, PreviewPolicy::preview_default());

    // First hover → the controller asks for a fresh capture.
    let PreviewDecision::Refresh(request) = controller.on_hover(window("w1"), viewer, 1_000_000)
    else {
        panic!("expected a refresh request on first hover");
    };
    assert_eq!(request.intent, ScreenStreamIntent::MiniRemotePreview);

    // The request feeds the existing negotiation path and yields a preview plan.
    let caps = ScreenStreamCapabilities::software_h264();
    let plan = negotiate_screen_stream(&caps, &caps, *request).expect("preview plan negotiates");
    assert_eq!(plan.resolution, ScreenResolution::preview());
    assert!(plan.requires_encrypted_transport);

    // The capture backend (mocked here) returns an encoded thumbnail; cache it.
    let thumb = EncodedScreenFrame {
        sequence: 0,
        capture_time_micros: 1_000_000,
        resolution: plan.resolution,
        codec: ScreenCodec::H264,
        encoder: HardwareEncoder::Software,
        dependency: FrameDependency::Key,
        frame_type: ScreenFrameType::I,
        payload: Bytes::from_static(b"jpeg-ish-thumb"),
    };
    controller.store_thumbnail(CaptureSourceId::new("w1"), thumb.clone(), 1_000_000);

    // A second hover shortly after is served from cache without re-capturing.
    let decision = controller.on_hover(window("w1"), viewer, 1_200_000);
    assert_eq!(decision, PreviewDecision::ServeCached(thumb));
    assert_eq!(controller.cached_count(), 1);
}
