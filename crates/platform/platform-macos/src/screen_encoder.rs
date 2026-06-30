//! macOS screen encoder backend.
//!
//! This module defines a VideoToolbox-backed encoder boundary for the daemon.
//! The low-level VTCompressionSession sample-buffer bridge is staged separately;
//! until that lands, this backend validates plan compatibility and reports a
//! concrete codec error for H.264/H.265 encode requests.

use async_trait::async_trait;
use nexkvm_streaming::{
    EncodedScreenFrame, FrameDependency, HardwareEncoder, ScreenCodec, ScreenEncoderBackend,
    ScreenError, ScreenFrame, ScreenFrameType, ScreenStreamPlan,
};

/// macOS VideoToolbox encoder adapter.
#[derive(Debug, Default, Clone, Copy)]
pub struct MacosVideoToolboxEncoder;

impl MacosVideoToolboxEncoder {
    /// Construct a new VideoToolbox encoder adapter.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Whether VideoToolbox support is expected on this target.
    #[must_use]
    pub const fn is_available() -> bool {
        cfg!(target_os = "macos")
    }
}

#[async_trait]
impl ScreenEncoderBackend for MacosVideoToolboxEncoder {
    fn encoder(&self) -> HardwareEncoder {
        HardwareEncoder::VideoToolbox
    }

    fn codecs(&self) -> &[ScreenCodec] {
        &[ScreenCodec::H264, ScreenCodec::H265]
    }

    async fn encode_frame(
        &self,
        plan: &ScreenStreamPlan,
        frame: ScreenFrame,
    ) -> Result<EncodedScreenFrame, ScreenError> {
        if plan.encoder != HardwareEncoder::VideoToolbox {
            return Err(ScreenError::CapabilityMismatch(
                "plan does not target VideoToolbox",
            ));
        }

        match plan.codec {
            ScreenCodec::H264 | ScreenCodec::H265 => Err(ScreenError::Codec(
                "VideoToolbox compression session wiring is pending".into(),
            )),
            ScreenCodec::RawRgba => Ok(EncodedScreenFrame {
                sequence: frame.sequence,
                capture_time_micros: frame.capture_time_micros,
                resolution: frame.resolution,
                codec: ScreenCodec::RawRgba,
                encoder: HardwareEncoder::Software,
                dependency: FrameDependency::Key,
                frame_type: ScreenFrameType::I,
                payload: frame.payload,
            }),
        }
    }
}
