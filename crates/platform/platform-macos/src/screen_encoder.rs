//! macOS screen encoder backend.
//!
//! This module defines a VideoToolbox-backed encoder boundary for the daemon.
//! The backend wraps system-memory BGRA/RGBA frames in CoreVideo pixel buffers,
//! submits them to a short-lived VideoToolbox compression session, and extracts
//! the encoded sample payload for the screen stream lane.

use async_trait::async_trait;
use bytes::Bytes;
use nexkvm_streaming::{
    EncodedScreenFrame, FrameDependency, GpuMemoryKind, HardwareEncoder, PixelFormat, ScreenCodec,
    ScreenEncoderBackend, ScreenError, ScreenFrame, ScreenFrameType, ScreenStreamPlan,
};
use std::ffi::c_void;
use std::ptr;
use std::sync::mpsc;
use std::time::Duration;

type OsStatus = i32;
type Boolean = u8;

const NO_ERR: OsStatus = 0;
const K_CM_TIME_FLAGS_VALID: u32 = 1;
const K_CM_BLOCK_BUFFER_NO_ERR: OsStatus = 0;
const K_CF_NUMBER_SINT32_TYPE: i32 = 3;
const K_CV_PIXEL_FORMAT_TYPE_32_BGRA: u32 = u32::from_be_bytes(*b"BGRA");
const K_CV_PIXEL_FORMAT_TYPE_32_RGBA: u32 = u32::from_be_bytes(*b"RGBA");
const K_CM_VIDEO_CODEC_TYPE_H264: u32 = u32::from_be_bytes(*b"avc1");
const K_CM_VIDEO_CODEC_TYPE_HEVC: u32 = u32::from_be_bytes(*b"hvc1");

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

impl CMTime {
    const INVALID: Self = Self {
        value: 0,
        timescale: 0,
        flags: 0,
        epoch: 0,
    };

    const fn new(value: i64, timescale: i32) -> Self {
        Self {
            value,
            timescale,
            flags: K_CM_TIME_FLAGS_VALID,
            epoch: 0,
        }
    }
}

#[derive(Debug)]
struct EncodedSample {
    payload: Bytes,
    dependency: FrameDependency,
    frame_type: ScreenFrameType,
}

struct EncodeCallbackState {
    sender: mpsc::Sender<Result<EncodedSample, ScreenError>>,
}

unsafe impl Send for EncodeCallbackState {}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFBooleanTrue: *const c_void;
    static kCFBooleanFalse: *const c_void;

    fn CFNumberCreate(
        allocator: *const c_void,
        the_type: i32,
        value_ptr: *const c_void,
    ) -> *const c_void;
    fn CFRelease(cf: *const c_void);
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    fn CVPixelBufferCreateWithBytes(
        allocator: *const c_void,
        width: usize,
        height: usize,
        pixel_format_type: u32,
        base_address: *mut c_void,
        bytes_per_row: usize,
        release_callback: Option<extern "C" fn(*mut c_void, *const c_void)>,
        release_ref_con: *mut c_void,
        pixel_buffer_attributes: *const c_void,
        pixel_buffer_out: *mut *mut c_void,
    ) -> OsStatus;
    fn CVPixelBufferRelease(texture: *const c_void);
}

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    fn CMSampleBufferGetDataBuffer(sample_buffer: *const c_void) -> *mut c_void;
    fn CMSampleBufferIsValid(sample_buffer: *const c_void) -> Boolean;
    fn CMSampleBufferDataIsReady(sample_buffer: *const c_void) -> Boolean;
    fn CMBlockBufferGetDataLength(the_buffer: *const c_void) -> usize;
    fn CMBlockBufferGetDataPointer(
        the_buffer: *mut c_void,
        offset: isize,
        length_at_offset_out: *mut isize,
        total_length_out: *mut isize,
        data_pointer_out: *mut *mut i8,
    ) -> OsStatus;
    fn CMBlockBufferCopyDataBytes(
        the_source_buffer: *const c_void,
        offset_to_data: usize,
        data_length: usize,
        destination: *mut c_void,
    ) -> OsStatus;
}

#[link(name = "VideoToolbox", kind = "framework")]
unsafe extern "C" {
    static kVTCompressionPropertyKey_RealTime: *const c_void;
    static kVTCompressionPropertyKey_AllowFrameReordering: *const c_void;
    static kVTCompressionPropertyKey_AverageBitRate: *const c_void;
    static kVTCompressionPropertyKey_ProfileLevel: *const c_void;
    static kVTProfileLevel_H264_Baseline_AutoLevel: *const c_void;
    static kVTProfileLevel_HEVC_Main_AutoLevel: *const c_void;

    fn VTCompressionSessionCreate(
        allocator: *const c_void,
        width: i32,
        height: i32,
        codec_type: u32,
        encoder_specification: *const c_void,
        source_image_buffer_attributes: *const c_void,
        compressed_data_allocator: *const c_void,
        output_callback: Option<
            extern "C" fn(*mut c_void, *mut c_void, OsStatus, u32, *mut c_void),
        >,
        output_callback_ref_con: *mut c_void,
        compression_session_out: *mut *mut c_void,
    ) -> OsStatus;
    fn VTSessionSetProperty(
        session: *mut c_void,
        property_key: *const c_void,
        property_value: *const c_void,
    ) -> OsStatus;
    fn VTCompressionSessionEncodeFrame(
        session: *mut c_void,
        image_buffer: *mut c_void,
        presentation_time_stamp: CMTime,
        duration: CMTime,
        frame_properties: *const c_void,
        source_frame_ref_con: *mut c_void,
        info_flags_out: *mut u32,
    ) -> OsStatus;
    fn VTCompressionSessionCompleteFrames(
        session: *mut c_void,
        complete_until_presentation_time_stamp: CMTime,
    ) -> OsStatus;
    fn VTCompressionSessionInvalidate(session: *mut c_void);
}

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
            ScreenCodec::H264 | ScreenCodec::H265 => encode_videotoolbox_frame(plan, frame).await,
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

async fn encode_videotoolbox_frame(
    plan: &ScreenStreamPlan,
    frame: ScreenFrame,
) -> Result<EncodedScreenFrame, ScreenError> {
    let plan = plan.clone();
    tokio::task::spawn_blocking(move || encode_videotoolbox_frame_blocking(&plan, frame))
        .await
        .map_err(|error| ScreenError::Backend(format!("VideoToolbox task failed: {error}")))?
}

fn encode_videotoolbox_frame_blocking(
    plan: &ScreenStreamPlan,
    frame: ScreenFrame,
) -> Result<EncodedScreenFrame, ScreenError> {
    validate_videotoolbox_frame(plan, &frame)?;

    let payload = frame.payload;
    let width = frame.resolution.width;
    let height = frame.resolution.height;
    let pixel_format = cv_pixel_format(frame.pixel_format)?;
    let bytes_per_row = width as usize * 4;
    let expected_len = bytes_per_row * height as usize;
    if payload.len() < expected_len {
        return Err(ScreenError::Codec(format!(
            "frame payload is too small for {}x{} {:?}: {} < {}",
            width,
            height,
            frame.pixel_format,
            payload.len(),
            expected_len
        )));
    }

    let mut session = ptr::null_mut();
    let codec_type = vt_codec_type(plan.codec)?;
    // SAFETY: Creates an owned VTCompressionSession for this function. All
    // pointers passed here are null optional CoreFoundation parameters except
    // the callback and output pointer, which are valid for the call duration.
    let status = unsafe {
        VTCompressionSessionCreate(
            ptr::null(),
            width as i32,
            height as i32,
            codec_type,
            ptr::null(),
            ptr::null(),
            ptr::null(),
            Some(compression_output_callback),
            ptr::null_mut(),
            &mut session,
        )
    };
    if status != NO_ERR || session.is_null() {
        return Err(vt_error("create compression session", status));
    }
    let session = VtSession(session);

    configure_session(session.0, plan)?;

    let mut pixel_buffer = ptr::null_mut();
    let base_address = payload.as_ptr().cast_mut().cast::<c_void>();
    // SAFETY: `payload` is kept alive until after encode completion. The pixel
    // buffer does not own base_address because no release callback is supplied.
    let status = unsafe {
        CVPixelBufferCreateWithBytes(
            ptr::null(),
            width as usize,
            height as usize,
            pixel_format,
            base_address,
            bytes_per_row,
            None,
            ptr::null_mut(),
            ptr::null(),
            &mut pixel_buffer,
        )
    };
    if status != NO_ERR || pixel_buffer.is_null() {
        return Err(vt_error("create pixel buffer", status));
    }
    let pixel_buffer = CvPixelBuffer(pixel_buffer);

    let (sender, receiver) = mpsc::channel();
    let mut callback_state = Box::new(EncodeCallbackState { sender });
    let callback_ref = (&mut *callback_state) as *mut EncodeCallbackState as *mut c_void;
    // SAFETY: `callback_ref` remains valid until after
    // VTCompressionSessionCompleteFrames returns below.
    unsafe {
        let _ = VTSessionSetProperty(
            session.0,
            kVTCompressionPropertyKey_RealTime,
            kCFBooleanTrue,
        );
        let _ = VTSessionSetProperty(
            session.0,
            kVTCompressionPropertyKey_AllowFrameReordering,
            kCFBooleanFalse,
        );
    }

    let presentation_time = CMTime::new(frame.sequence as i64, i32::from(plan.fps.max(1)).max(1));
    let mut info_flags = 0_u32;
    // SAFETY: Session and pixel buffer are valid owned objects. Callback state
    // pointer is valid through frame completion.
    let status = unsafe {
        VTCompressionSessionEncodeFrame(
            session.0,
            pixel_buffer.0,
            presentation_time,
            CMTime::INVALID,
            ptr::null(),
            callback_ref,
            &mut info_flags,
        )
    };
    if status != NO_ERR {
        return Err(vt_error("encode frame", status));
    }

    // SAFETY: Drains all callbacks for this one-frame session before callback
    // state is dropped.
    let status = unsafe { VTCompressionSessionCompleteFrames(session.0, presentation_time) };
    if status != NO_ERR {
        return Err(vt_error("complete frames", status));
    }

    let sample = receiver
        .recv_timeout(Duration::from_secs(5))
        .map_err(|error| {
            ScreenError::Codec(format!(
                "VideoToolbox produced no frame before timeout: {error}"
            ))
        })??;

    Ok(EncodedScreenFrame {
        sequence: frame.sequence,
        capture_time_micros: frame.capture_time_micros,
        resolution: frame.resolution,
        codec: plan.codec,
        encoder: HardwareEncoder::VideoToolbox,
        dependency: sample.dependency,
        frame_type: sample.frame_type,
        payload: sample.payload,
    })
}

fn validate_videotoolbox_frame(
    plan: &ScreenStreamPlan,
    frame: &ScreenFrame,
) -> Result<(), ScreenError> {
    if frame.memory != GpuMemoryKind::System {
        return Err(ScreenError::CapabilityMismatch(
            "VideoToolbox system-memory encode currently requires CPU pixel bytes",
        ));
    }
    if frame.resolution != plan.resolution {
        return Err(ScreenError::CapabilityMismatch(
            "frame resolution does not match stream plan",
        ));
    }
    Ok(())
}

fn cv_pixel_format(pixel_format: PixelFormat) -> Result<u32, ScreenError> {
    match pixel_format {
        PixelFormat::Bgra8 => Ok(K_CV_PIXEL_FORMAT_TYPE_32_BGRA),
        PixelFormat::Rgba8 => Ok(K_CV_PIXEL_FORMAT_TYPE_32_RGBA),
        PixelFormat::Nv12 => Err(ScreenError::Codec(
            "VideoToolbox NV12 pixel-buffer wrapping is not wired yet".into(),
        )),
    }
}

fn vt_codec_type(codec: ScreenCodec) -> Result<u32, ScreenError> {
    match codec {
        ScreenCodec::H264 => Ok(K_CM_VIDEO_CODEC_TYPE_H264),
        ScreenCodec::H265 => Ok(K_CM_VIDEO_CODEC_TYPE_HEVC),
        ScreenCodec::RawRgba => Err(ScreenError::Codec(
            "RawRgba is not a VideoToolbox compressed codec".into(),
        )),
    }
}

fn configure_session(session: *mut c_void, plan: &ScreenStreamPlan) -> Result<(), ScreenError> {
    let bitrate = (plan.bitrate_kbps.saturating_mul(1000)) as i32;
    let bitrate_number = cf_number_i32(bitrate)?;
    // SAFETY: Session and CoreFoundation objects are valid. Property failures
    // are returned as codec errors so unsupported hardware profiles surface
    // clearly to the caller.
    let status = unsafe {
        VTSessionSetProperty(
            session,
            kVTCompressionPropertyKey_AverageBitRate,
            bitrate_number.0,
        )
    };
    if status != NO_ERR {
        return Err(vt_error("set average bitrate", status));
    }

    let profile = match plan.codec {
        ScreenCodec::H264 => unsafe { kVTProfileLevel_H264_Baseline_AutoLevel },
        ScreenCodec::H265 => unsafe { kVTProfileLevel_HEVC_Main_AutoLevel },
        ScreenCodec::RawRgba => ptr::null(),
    };
    if !profile.is_null() {
        // SAFETY: Session and profile constant are valid CoreFoundation values.
        let status = unsafe {
            VTSessionSetProperty(session, kVTCompressionPropertyKey_ProfileLevel, profile)
        };
        if status != NO_ERR {
            return Err(vt_error("set profile level", status));
        }
    }

    Ok(())
}

fn cf_number_i32(value: i32) -> Result<CfObject, ScreenError> {
    // SAFETY: Creates an owned CFNumber from a valid pointer to an i32 value.
    let object = unsafe {
        CFNumberCreate(
            ptr::null(),
            K_CF_NUMBER_SINT32_TYPE,
            (&value as *const i32).cast::<c_void>(),
        )
    };
    if object.is_null() {
        return Err(ScreenError::Codec("failed to create CFNumber".into()));
    }
    Ok(CfObject(object))
}

extern "C" fn compression_output_callback(
    output_callback_ref_con: *mut c_void,
    source_frame_ref_con: *mut c_void,
    status: OsStatus,
    _info_flags: u32,
    sample_buffer: *mut c_void,
) {
    let state_ref_con = if output_callback_ref_con.is_null() {
        source_frame_ref_con
    } else {
        output_callback_ref_con
    };
    if state_ref_con.is_null() {
        return;
    }
    // SAFETY: VideoToolbox calls this with the callback state pointer supplied
    // to EncodeFrame; the caller keeps it alive until frames are completed.
    let state = unsafe { &*(state_ref_con as *const EncodeCallbackState) };
    let result = if status == NO_ERR {
        encoded_sample_from_sample_buffer(sample_buffer)
    } else {
        Err(vt_error("compression callback", status))
    };
    let _ = state.sender.send(result);
}

fn encoded_sample_from_sample_buffer(
    sample_buffer: *mut c_void,
) -> Result<EncodedSample, ScreenError> {
    if sample_buffer.is_null() {
        return Err(ScreenError::Codec(
            "VideoToolbox returned a null sample buffer".into(),
        ));
    }
    // SAFETY: Sample buffer is owned by VideoToolbox for the callback duration.
    let valid = unsafe { CMSampleBufferIsValid(sample_buffer) != 0 };
    let ready = unsafe { CMSampleBufferDataIsReady(sample_buffer) != 0 };
    if !valid || !ready {
        return Err(ScreenError::Codec(
            "VideoToolbox sample buffer is not ready".into(),
        ));
    }

    // SAFETY: Valid sample buffer is queried for its encoded block buffer.
    let block_buffer = unsafe { CMSampleBufferGetDataBuffer(sample_buffer) };
    if block_buffer.is_null() {
        return Err(ScreenError::Codec(
            "VideoToolbox sample buffer has no data block".into(),
        ));
    }
    let payload = block_buffer_payload(block_buffer)?;
    Ok(EncodedSample {
        payload,
        dependency: FrameDependency::Key,
        frame_type: ScreenFrameType::I,
    })
}

fn block_buffer_payload(block_buffer: *mut c_void) -> Result<Bytes, ScreenError> {
    // SAFETY: Block buffer is valid for callback duration.
    let total_len = unsafe { CMBlockBufferGetDataLength(block_buffer) };
    if total_len == 0 {
        return Err(ScreenError::Codec(
            "VideoToolbox produced an empty encoded frame".into(),
        ));
    }

    let mut length_at_offset = 0_isize;
    let mut contiguous_total = 0_isize;
    let mut data_ptr = ptr::null_mut();
    // SAFETY: Output pointers are valid locals. The function either returns a
    // contiguous pointer or an error, in which case we copy below.
    let status = unsafe {
        CMBlockBufferGetDataPointer(
            block_buffer,
            0,
            &mut length_at_offset,
            &mut contiguous_total,
            &mut data_ptr,
        )
    };
    if status == K_CM_BLOCK_BUFFER_NO_ERR
        && !data_ptr.is_null()
        && contiguous_total as usize == total_len
    {
        // SAFETY: Pointer is valid for `total_len` bytes during callback; Bytes
        // owns the copied Vec after this statement.
        let bytes = unsafe { std::slice::from_raw_parts(data_ptr.cast::<u8>(), total_len) };
        return Ok(Bytes::copy_from_slice(bytes));
    }

    let mut bytes = vec![0_u8; total_len];
    // SAFETY: Destination buffer is valid for `total_len` bytes.
    let status = unsafe {
        CMBlockBufferCopyDataBytes(block_buffer, 0, total_len, bytes.as_mut_ptr().cast())
    };
    if status != NO_ERR {
        return Err(vt_error("copy block buffer", status));
    }
    Ok(Bytes::from(bytes))
}

fn vt_error(operation: &str, status: OsStatus) -> ScreenError {
    ScreenError::Codec(format!(
        "VideoToolbox {operation} failed with OSStatus {status}"
    ))
}

#[derive(Debug)]
struct VtSession(*mut c_void);

impl Drop for VtSession {
    fn drop(&mut self) {
        // SAFETY: `self.0` is an owned VTCompressionSession created by
        // VTCompressionSessionCreate.
        unsafe {
            VTCompressionSessionInvalidate(self.0);
            CFRelease(self.0);
        }
    }
}

#[derive(Debug)]
struct CvPixelBuffer(*mut c_void);

impl Drop for CvPixelBuffer {
    fn drop(&mut self) {
        // SAFETY: `self.0` is an owned CVPixelBuffer created by
        // CVPixelBufferCreateWithBytes.
        unsafe {
            CVPixelBufferRelease(self.0);
        }
    }
}

#[derive(Debug)]
struct CfObject(*const c_void);

impl Drop for CfObject {
    fn drop(&mut self) {
        // SAFETY: `self.0` is an owned CoreFoundation object from a Create rule.
        unsafe {
            CFRelease(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use nexkvm_core::identity::DeviceId;
    use nexkvm_streaming::{
        CaptureSource, CaptureSourceId, GpuMemoryKind, PixelFormat, ScreenResolution,
        ScreenStreamIntent,
    };

    fn test_plan(codec: ScreenCodec) -> ScreenStreamPlan {
        ScreenStreamPlan {
            from: DeviceId::generate(),
            to: DeviceId::generate(),
            source: CaptureSource::Display {
                id: CaptureSourceId::new("display-main"),
                label: "Main Display".into(),
            },
            intent: ScreenStreamIntent::InteractiveRemote,
            codec,
            encoder: HardwareEncoder::VideoToolbox,
            memory: GpuMemoryKind::System,
            resolution: ScreenResolution::new(16, 16),
            fps: 30,
            bitrate_kbps: 300,
            zero_copy: false,
            requires_encrypted_transport: true,
        }
    }

    fn test_frame() -> ScreenFrame {
        let resolution = ScreenResolution::new(16, 16);
        let mut pixels = vec![0_u8; resolution.pixels() as usize * 4];
        for pixel in pixels.chunks_exact_mut(4) {
            pixel.copy_from_slice(&[0x20, 0x80, 0xe0, 0xff]);
        }

        ScreenFrame {
            sequence: 7,
            capture_time_micros: 42,
            resolution,
            pixel_format: PixelFormat::Bgra8,
            memory: GpuMemoryKind::System,
            payload: Bytes::from(pixels),
        }
    }

    #[tokio::test]
    async fn h264_encode_uses_videotoolbox_session() {
        let encoded = MacosVideoToolboxEncoder::new()
            .encode_frame(&test_plan(ScreenCodec::H264), test_frame())
            .await
            .unwrap();

        assert_eq!(encoded.sequence, 7);
        assert_eq!(encoded.codec, ScreenCodec::H264);
        assert_eq!(encoded.encoder, HardwareEncoder::VideoToolbox);
        assert_eq!(encoded.dependency, FrameDependency::Key);
        assert_eq!(encoded.frame_type, ScreenFrameType::I);
        assert!(!encoded.payload.is_empty());
    }

    #[tokio::test]
    async fn h265_encode_uses_videotoolbox_session() {
        let encoded = MacosVideoToolboxEncoder::new()
            .encode_frame(&test_plan(ScreenCodec::H265), test_frame())
            .await
            .unwrap();

        assert_eq!(encoded.codec, ScreenCodec::H265);
        assert_eq!(encoded.encoder, HardwareEncoder::VideoToolbox);
        assert!(!encoded.payload.is_empty());
    }
}
