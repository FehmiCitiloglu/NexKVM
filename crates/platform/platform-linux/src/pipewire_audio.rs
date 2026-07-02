//! Linux PipeWire audio routing backend scaffolding.
//!
//! PipeWire exposes audio devices as graph nodes. This module maps the stable
//! node metadata into NexKVM's platform-neutral [`AudioBackend`] boundary before
//! native graph mutation is wired in later slices.

use async_trait::async_trait;
use bytes::Bytes;
use nexkvm_streaming::{
    AudioBackend, AudioCodec, AudioDevice, AudioDeviceId, AudioDeviceRole, AudioError, AudioFormat,
    AudioFrame, AudioStreamBackend,
};
use std::collections::VecDeque;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

/// PipeWire node interface type reported by registry global events.
pub const PIPEWIRE_INTERFACE_NODE: &str = "PipeWire:Interface:Node";

/// PipeWire graph node metadata used for audio routing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipeWireAudioNode {
    /// PipeWire object id.
    pub id: u32,
    /// String properties reported by the PipeWire registry.
    pub properties: HashMap<String, String>,
    /// Whether this node is the current default endpoint for its media class.
    pub is_default: bool,
}

impl PipeWireAudioNode {
    /// Create an audio graph node record.
    #[must_use]
    pub fn new(id: u32) -> Self {
        Self {
            id,
            properties: HashMap::new(),
            is_default: false,
        }
    }

    /// Add a PipeWire property.
    #[must_use]
    pub fn with_property(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.properties.insert(key.into(), value.into());
        self
    }

    /// Mark this node as the default endpoint.
    #[must_use]
    pub const fn with_default(mut self, is_default: bool) -> Self {
        self.is_default = is_default;
        self
    }

    fn property(&self, key: &str) -> Option<&str> {
        self.properties.get(key).map(String::as_str)
    }
}

/// Snapshot of PipeWire audio graph state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PipeWireAudioGraphSnapshot {
    /// Nodes visible to the backend.
    pub nodes: Vec<PipeWireAudioNode>,
}

/// One PipeWire registry global event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipeWireRegistryGlobal {
    /// PipeWire global object id.
    pub id: u32,
    /// PipeWire interface type.
    pub type_: String,
    /// Global properties.
    pub properties: HashMap<String, String>,
}

/// Incremental collector for PipeWire registry globals.
#[derive(Debug, Default, Clone)]
pub struct PipeWireRegistryCollector {
    globals: BTreeMap<u32, PipeWireRegistryGlobal>,
}

impl PipeWireRegistryCollector {
    /// Record or replace a registry global.
    pub fn global(&mut self, global: PipeWireRegistryGlobal) {
        self.globals.insert(global.id, global);
    }

    /// Remove a registry global by id.
    pub fn global_remove(&mut self, id: u32) {
        self.globals.remove(&id);
    }

    /// Convert collected globals into an audio graph snapshot.
    #[must_use]
    pub fn snapshot(&self) -> PipeWireAudioGraphSnapshot {
        PipeWireAudioGraphSnapshot {
            nodes: self
                .globals
                .values()
                .filter(|global| global.type_ == PIPEWIRE_INTERFACE_NODE)
                .map(|global| PipeWireAudioNode {
                    id: global.id,
                    properties: global.properties.clone(),
                    is_default: parse_bool_property(&global.properties, "node.default"),
                })
                .collect(),
        }
    }
}

/// PipeWire audio graph access boundary.
#[async_trait]
pub trait PipeWireAudioGraph: Send + Sync {
    /// Return a snapshot of known PipeWire audio nodes.
    async fn snapshot(&self) -> Result<PipeWireAudioGraphSnapshot, AudioError>;

    /// Switch default playback to the given PipeWire node id.
    async fn set_default_playback(&self, node_id: u32) -> Result<(), AudioError>;
}

/// PipeWire audio frame stream access boundary.
#[async_trait]
pub trait PipeWireAudioStream: Send + Sync {
    /// Capture one frame from a PipeWire audio node.
    async fn capture_frame(
        &self,
        node_id: u32,
        format: AudioFormat,
    ) -> Result<AudioFrame, AudioError>;

    /// Play one frame to a PipeWire audio node.
    async fn play_frame(
        &self,
        node_id: u32,
        format: AudioFormat,
        frame: AudioFrame,
    ) -> Result<(), AudioError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PipeWireAudioStreamDirection {
    Capture,
    Playback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PipeWireAudioStreamRequest {
    node_id: u32,
    direction: PipeWireAudioStreamDirection,
    format: AudioFormat,
}

#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
struct PipeWireAudioMappedBuffer<'a> {
    bytes: &'a [u8],
    chunk_offset: usize,
    chunk_size: usize,
    capture_time_micros: u64,
}

/// Static graph used by tests and diagnostics.
#[derive(Debug, Clone)]
pub struct StaticPipeWireAudioGraph {
    snapshot: PipeWireAudioGraphSnapshot,
}

/// Static stream used by tests and diagnostics.
#[derive(Debug, Clone, Default)]
pub struct StaticPipeWireAudioStream {
    capture_frames: Arc<Mutex<BTreeMap<u32, VecDeque<AudioFrame>>>>,
    played_frames: Arc<Mutex<Vec<(u32, AudioFrame)>>>,
}

/// Placeholder stream for graph-only PipeWire backends.
#[derive(Debug, Clone, Copy, Default)]
pub struct UnsupportedPipeWireAudioStream;

/// Native PipeWire stream accessor for the current Linux user session.
#[derive(Debug, Clone, Copy, Default)]
pub struct NativePipeWireAudioStream;

/// Native PipeWire graph accessor for the current Linux user session.
#[derive(Debug, Clone, Copy, Default)]
pub struct NativePipeWireAudioGraph;

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
struct CommandSpec {
    program: &'static str,
    args: Vec<String>,
}

impl StaticPipeWireAudioGraph {
    /// Create a static graph from a snapshot.
    #[must_use]
    pub const fn new(snapshot: PipeWireAudioGraphSnapshot) -> Self {
        Self { snapshot }
    }
}

impl StaticPipeWireAudioStream {
    /// Create a static stream from queued capture frames keyed by node id.
    #[must_use]
    pub fn new(frames: Vec<(u32, AudioFrame)>) -> Self {
        let mut capture_frames: BTreeMap<u32, VecDeque<AudioFrame>> = BTreeMap::new();
        for (node_id, frame) in frames {
            capture_frames.entry(node_id).or_default().push_back(frame);
        }
        Self {
            capture_frames: Arc::new(Mutex::new(capture_frames)),
            played_frames: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Return frames played by node id, in playback order.
    #[must_use]
    pub fn played_frames(&self) -> Vec<(u32, AudioFrame)> {
        self.played_frames
            .lock()
            .expect("poisoned static PipeWire playback frames")
            .clone()
    }
}

#[async_trait]
impl PipeWireAudioGraph for StaticPipeWireAudioGraph {
    async fn snapshot(&self) -> Result<PipeWireAudioGraphSnapshot, AudioError> {
        Ok(self.snapshot.clone())
    }

    async fn set_default_playback(&self, node_id: u32) -> Result<(), AudioError> {
        if self.snapshot.nodes.iter().any(|node| node.id == node_id) {
            Ok(())
        } else {
            Err(AudioError::DeviceUnavailable(format!(
                "pipewire-node:{node_id}"
            )))
        }
    }
}

#[async_trait]
impl PipeWireAudioStream for StaticPipeWireAudioStream {
    async fn capture_frame(
        &self,
        node_id: u32,
        _format: AudioFormat,
    ) -> Result<AudioFrame, AudioError> {
        self.capture_frames
            .lock()
            .expect("poisoned static PipeWire capture frames")
            .get_mut(&node_id)
            .and_then(VecDeque::pop_front)
            .ok_or_else(|| AudioError::DeviceUnavailable(format!("pipewire-node:{node_id}")))
    }

    async fn play_frame(
        &self,
        node_id: u32,
        _format: AudioFormat,
        frame: AudioFrame,
    ) -> Result<(), AudioError> {
        self.played_frames
            .lock()
            .expect("poisoned static PipeWire playback frames")
            .push((node_id, frame));
        Ok(())
    }
}

#[async_trait]
impl PipeWireAudioStream for UnsupportedPipeWireAudioStream {
    async fn capture_frame(
        &self,
        _node_id: u32,
        _format: AudioFormat,
    ) -> Result<AudioFrame, AudioError> {
        Err(AudioError::Unsupported(
            "PipeWire audio stream capture is not wired",
        ))
    }

    async fn play_frame(
        &self,
        _node_id: u32,
        _format: AudioFormat,
        _frame: AudioFrame,
    ) -> Result<(), AudioError> {
        Err(AudioError::Unsupported(
            "PipeWire audio stream playback is not wired",
        ))
    }
}

#[async_trait]
impl PipeWireAudioStream for NativePipeWireAudioStream {
    async fn capture_frame(
        &self,
        node_id: u32,
        format: AudioFormat,
    ) -> Result<AudioFrame, AudioError> {
        let request = PipeWireAudioStreamRequest {
            node_id,
            direction: PipeWireAudioStreamDirection::Capture,
            format,
        };
        native_pipewire_capture_audio_frame(request).await
    }

    async fn play_frame(
        &self,
        node_id: u32,
        format: AudioFormat,
        frame: AudioFrame,
    ) -> Result<(), AudioError> {
        let request = PipeWireAudioStreamRequest {
            node_id,
            direction: PipeWireAudioStreamDirection::Playback,
            format,
        };
        native_pipewire_play_audio_frame(request, frame).await
    }
}

#[async_trait]
impl PipeWireAudioGraph for NativePipeWireAudioGraph {
    async fn snapshot(&self) -> Result<PipeWireAudioGraphSnapshot, AudioError> {
        native_pipewire_audio_snapshot().await
    }

    async fn set_default_playback(&self, node_id: u32) -> Result<(), AudioError> {
        native_pipewire_set_default_playback(node_id).await
    }
}

/// PipeWire-backed Linux audio backend.
#[derive(Debug, Clone)]
pub struct PipeWireAudioBackend<G, S = UnsupportedPipeWireAudioStream> {
    graph: G,
    stream: S,
    preferred_format: AudioFormat,
}

impl<G> PipeWireAudioBackend<G>
where
    G: PipeWireAudioGraph,
{
    /// Create a PipeWire audio backend over a graph accessor.
    #[must_use]
    pub fn new(graph: G) -> Self {
        Self {
            graph,
            stream: UnsupportedPipeWireAudioStream,
            preferred_format: AudioFormat::opus_stereo_48k(),
        }
    }
}

impl<G, S> PipeWireAudioBackend<G, S>
where
    G: PipeWireAudioGraph,
    S: PipeWireAudioStream,
{
    /// Create a PipeWire audio backend with explicit graph and stream accessors.
    #[must_use]
    pub fn with_stream(graph: G, stream: S) -> Self {
        Self {
            graph,
            stream,
            preferred_format: AudioFormat::opus_stereo_48k(),
        }
    }
}

#[async_trait]
impl<G, S> AudioBackend for PipeWireAudioBackend<G, S>
where
    G: PipeWireAudioGraph,
    S: PipeWireAudioStream,
{
    async fn devices(&self) -> Result<Vec<AudioDevice>, AudioError> {
        Ok(self
            .graph
            .snapshot()
            .await?
            .nodes
            .iter()
            .filter_map(audio_device_from_node)
            .collect())
    }

    async fn switch_playback_device(&self, device: &AudioDeviceId) -> Result<(), AudioError> {
        let node_id = pipewire_node_id(device)?;
        self.graph.set_default_playback(node_id).await
    }

    fn preferred_format(&self) -> AudioFormat {
        self.preferred_format
    }
}

#[async_trait]
impl<G, S> AudioStreamBackend for PipeWireAudioBackend<G, S>
where
    G: PipeWireAudioGraph,
    S: PipeWireAudioStream,
{
    async fn capture_audio_frame(&self, device: &AudioDeviceId) -> Result<AudioFrame, AudioError> {
        let node_id = pipewire_node_id(device)?;
        self.stream
            .capture_frame(node_id, self.preferred_format)
            .await
    }

    async fn play_audio_frame(
        &self,
        device: &AudioDeviceId,
        frame: AudioFrame,
    ) -> Result<(), AudioError> {
        let node_id = pipewire_node_id(device)?;
        self.stream
            .play_frame(node_id, self.preferred_format, frame)
            .await
    }
}

fn audio_device_from_node(node: &PipeWireAudioNode) -> Option<AudioDevice> {
    let role = match node.property("media.class")? {
        "Audio/Sink" => AudioDeviceRole::Playback,
        "Audio/Source" => AudioDeviceRole::Capture,
        "Audio/Duplex" => AudioDeviceRole::Duplex,
        _ => return None,
    };
    Some(AudioDevice {
        id: AudioDeviceId::new(format!("pipewire-node:{}", node.id)),
        label: node
            .property("node.description")
            .or_else(|| node.property("node.nick"))
            .or_else(|| node.property("node.name"))
            .map_or_else(|| format!("PipeWire node {}", node.id), str::to_string),
        role,
        is_default: node.is_default,
    })
}

fn pipewire_node_id(device: &AudioDeviceId) -> Result<u32, AudioError> {
    device
        .0
        .strip_prefix("pipewire-node:")
        .ok_or_else(|| AudioError::DeviceUnavailable(device.0.clone()))?
        .parse()
        .map_err(|_| AudioError::DeviceUnavailable(device.0.clone()))
}

fn parse_bool_property(properties: &HashMap<String, String>, key: &str) -> bool {
    matches!(
        properties.get(key).map(String::as_str),
        Some("true" | "1" | "yes")
    )
}

#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
fn audio_stream_properties(request: &PipeWireAudioStreamRequest) -> Vec<(String, String)> {
    vec![
        ("application.name".into(), "NexKVM".into()),
        ("media.type".into(), "Audio".into()),
        (
            "media.category".into(),
            match request.direction {
                PipeWireAudioStreamDirection::Capture => "Capture",
                PipeWireAudioStreamDirection::Playback => "Playback",
            }
            .into(),
        ),
        ("media.role".into(), "Music".into()),
        ("target.object".into(), request.node_id.to_string()),
    ]
}

#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
fn audio_frame_from_mapped_buffer(
    buffer: PipeWireAudioMappedBuffer<'_>,
    format: AudioFormat,
    sequence: u64,
) -> Result<AudioFrame, AudioError> {
    if format.codec != AudioCodec::Pcm {
        return Err(AudioError::Unsupported(
            "native PipeWire audio stream currently expects PCM frames",
        ));
    }
    let end = buffer
        .chunk_offset
        .checked_add(buffer.chunk_size)
        .ok_or_else(|| AudioError::Codec("PipeWire audio chunk range overflow".into()))?;
    let bytes = buffer
        .bytes
        .get(buffer.chunk_offset..end)
        .ok_or_else(|| AudioError::Codec("PipeWire audio chunk range out of bounds".into()))?;
    Ok(AudioFrame {
        sequence,
        capture_time_micros: buffer.capture_time_micros,
        samples_per_channel: format.samples_per_frame(),
        codec: AudioCodec::Pcm,
        payload: Bytes::copy_from_slice(bytes),
    })
}

#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
fn wpctl_set_default_command(node_id: u32) -> CommandSpec {
    CommandSpec {
        program: "wpctl",
        args: vec!["set-default".to_string(), node_id.to_string()],
    }
}

#[cfg(target_os = "linux")]
async fn native_pipewire_audio_snapshot() -> Result<PipeWireAudioGraphSnapshot, AudioError> {
    tokio::task::spawn_blocking(native_pipewire::snapshot)
        .await
        .map_err(|error| AudioError::Backend(format!("PipeWire audio graph task join: {error}")))?
}

#[cfg(not(target_os = "linux"))]
async fn native_pipewire_audio_snapshot() -> Result<PipeWireAudioGraphSnapshot, AudioError> {
    Err(AudioError::Unsupported(
        "native PipeWire audio graph requires Linux",
    ))
}

#[cfg(target_os = "linux")]
async fn native_pipewire_set_default_playback(node_id: u32) -> Result<(), AudioError> {
    tokio::task::spawn_blocking(move || run_wpctl_set_default(node_id))
        .await
        .map_err(|error| AudioError::Backend(format!("wpctl task join: {error}")))?
}

#[cfg(not(target_os = "linux"))]
async fn native_pipewire_set_default_playback(_node_id: u32) -> Result<(), AudioError> {
    Err(AudioError::Unsupported(
        "native PipeWire default playback mutation requires Linux",
    ))
}

#[cfg(target_os = "linux")]
async fn native_pipewire_capture_audio_frame(
    request: PipeWireAudioStreamRequest,
) -> Result<AudioFrame, AudioError> {
    tokio::task::spawn_blocking(move || native_pipewire::capture_audio_frame(request))
        .await
        .map_err(|error| {
            AudioError::Backend(format!("PipeWire audio capture task join: {error}"))
        })?
}

#[cfg(not(target_os = "linux"))]
async fn native_pipewire_capture_audio_frame(
    _request: PipeWireAudioStreamRequest,
) -> Result<AudioFrame, AudioError> {
    Err(AudioError::Unsupported(
        "native PipeWire audio capture requires Linux",
    ))
}

#[cfg(target_os = "linux")]
async fn native_pipewire_play_audio_frame(
    request: PipeWireAudioStreamRequest,
    frame: AudioFrame,
) -> Result<(), AudioError> {
    tokio::task::spawn_blocking(move || native_pipewire::play_audio_frame(request, frame))
        .await
        .map_err(|error| {
            AudioError::Backend(format!("PipeWire audio playback task join: {error}"))
        })?
}

#[cfg(not(target_os = "linux"))]
async fn native_pipewire_play_audio_frame(
    _request: PipeWireAudioStreamRequest,
    _frame: AudioFrame,
) -> Result<(), AudioError> {
    Err(AudioError::Unsupported(
        "native PipeWire audio playback requires Linux",
    ))
}

#[cfg(target_os = "linux")]
fn run_wpctl_set_default(node_id: u32) -> Result<(), AudioError> {
    let command = wpctl_set_default_command(node_id);
    let output = std::process::Command::new(command.program)
        .args(&command.args)
        .output()
        .map_err(|error| AudioError::Backend(format!("run wpctl set-default: {error}")))?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    Err(AudioError::Backend(format!(
        "wpctl set-default {node_id} failed: {detail}"
    )))
}

#[cfg(target_os = "linux")]
#[allow(clashing_extern_declarations)]
mod native_pipewire {
    use super::{
        AudioError, AudioFrame, PipeWireAudioGraphSnapshot, PipeWireAudioMappedBuffer,
        PipeWireAudioStreamDirection, PipeWireAudioStreamRequest, PipeWireRegistryCollector,
        PipeWireRegistryGlobal, audio_frame_from_mapped_buffer, audio_stream_properties,
    };
    use std::collections::HashMap;
    use std::ffi::{CStr, CString, c_char, c_int, c_void};
    use std::ptr;
    use std::sync::mpsc::{SyncSender, TryRecvError, sync_channel};
    use std::time::{Duration, Instant};

    const REGISTRY_ENUMERATION_TIMEOUT: Duration = Duration::from_millis(500);
    const REGISTRY_ITERATE_TIMEOUT_MS: c_int = 25;
    const PW_VERSION_REGISTRY: u32 = 3;
    const PW_DIRECTION_INPUT: u32 = 0;
    const PW_DIRECTION_OUTPUT: u32 = 1;
    const PW_ID_ANY: u32 = u32::MAX;
    const PW_STREAM_FLAG_AUTOCONNECT: u32 = 1 << 0;
    const PW_STREAM_FLAG_MAP_BUFFERS: u32 = 1 << 2;
    const STREAM_WAIT_TIMEOUT: Duration = Duration::from_secs(5);

    #[repr(C)]
    struct SpaDict {
        flags: u32,
        n_items: u32,
        items: *const SpaDictItem,
    }

    #[repr(C)]
    struct SpaDictItem {
        key: *const c_char,
        value: *const c_char,
    }

    #[repr(C)]
    struct SpaList {
        next: *mut SpaList,
        prev: *mut SpaList,
    }

    #[repr(C)]
    struct SpaCallbacks {
        funcs: *const c_void,
        data: *mut c_void,
    }

    #[repr(C)]
    struct SpaHook {
        link: SpaList,
        cb: SpaCallbacks,
        removed: Option<extern "C" fn(*mut SpaHook)>,
        priv_: *mut c_void,
    }

    impl Default for SpaHook {
        fn default() -> Self {
            Self {
                link: SpaList {
                    next: ptr::null_mut(),
                    prev: ptr::null_mut(),
                },
                cb: SpaCallbacks {
                    funcs: ptr::null(),
                    data: ptr::null_mut(),
                },
                removed: None,
                priv_: ptr::null_mut(),
            }
        }
    }

    #[repr(C)]
    struct PwMainLoop {
        _private: [u8; 0],
    }

    #[repr(C)]
    struct PwLoop {
        _private: [u8; 0],
    }

    #[repr(C)]
    struct PwContext {
        _private: [u8; 0],
    }

    #[repr(C)]
    struct PwCore {
        _private: [u8; 0],
    }

    #[repr(C)]
    struct PwRegistry {
        _private: [u8; 0],
    }

    #[repr(C)]
    struct PwProxy {
        _private: [u8; 0],
    }

    #[repr(C)]
    struct PwProperties {
        _private: [u8; 0],
    }

    #[repr(C)]
    struct PwStream {
        _private: [u8; 0],
    }

    #[repr(C)]
    struct SpaPod {
        size: u32,
        type_: u32,
    }

    #[repr(C)]
    struct PwStreamEvents {
        version: u32,
        destroy: Option<extern "C" fn(*mut c_void)>,
        state_changed: Option<extern "C" fn(*mut c_void, u32, u32, *const c_char)>,
        control_info: Option<extern "C" fn(*mut c_void, u32, *const c_void)>,
        io_changed: Option<extern "C" fn(*mut c_void, u32, *mut c_void, u32)>,
        param_changed: Option<extern "C" fn(*mut c_void, u32, *const SpaPod)>,
        add_buffer: Option<extern "C" fn(*mut c_void, *mut PwBuffer)>,
        remove_buffer: Option<extern "C" fn(*mut c_void, *mut PwBuffer)>,
        process: Option<extern "C" fn(*mut c_void)>,
        drained: Option<extern "C" fn(*mut c_void)>,
        command: Option<extern "C" fn(*mut c_void, *const c_void)>,
        trigger_done: Option<extern "C" fn(*mut c_void)>,
    }

    #[repr(C)]
    struct PwBuffer {
        buffer: *mut SpaBuffer,
        user_data: *mut c_void,
        size: u64,
        requested: u64,
        time: u64,
    }

    #[repr(C)]
    struct SpaBuffer {
        n_metas: u32,
        metas: *mut c_void,
        n_datas: u32,
        datas: *mut SpaData,
    }

    #[repr(C)]
    struct SpaData {
        type_: u32,
        flags: u32,
        fd: i64,
        mapoffset: u32,
        maxsize: u32,
        data: *mut c_void,
        chunk: *mut SpaChunk,
    }

    #[repr(C)]
    struct SpaChunk {
        offset: u32,
        size: u32,
        stride: i32,
        flags: i32,
    }

    #[repr(C)]
    struct PwRegistryEvents {
        version: u32,
        global: Option<
            extern "C" fn(
                data: *mut c_void,
                id: u32,
                permissions: u32,
                type_: *const c_char,
                version: u32,
                props: *const SpaDict,
            ),
        >,
        global_remove: Option<extern "C" fn(data: *mut c_void, id: u32)>,
    }

    struct RegistryData {
        collector: PipeWireRegistryCollector,
    }

    struct AudioStreamData {
        stream: *mut PwStream,
        request: PipeWireAudioStreamRequest,
        sequence: u64,
        capture_sender: Option<SyncSender<Result<AudioFrame, String>>>,
        playback_sender: Option<SyncSender<Result<(), String>>>,
        playback_frame: Option<AudioFrame>,
        playback_done: bool,
    }

    unsafe extern "C" {
        fn pw_init(argc: *mut c_int, argv: *mut *mut *mut c_char);
    }

    #[link(name = "pipewire-0.3")]
    unsafe extern "C" {
        fn pw_main_loop_new(props: *const SpaDict) -> *mut PwMainLoop;
        fn pw_main_loop_destroy(loop_: *mut PwMainLoop);
        fn pw_main_loop_get_loop(loop_: *mut PwMainLoop) -> *mut PwLoop;
        fn pw_context_new(
            main_loop: *mut PwLoop,
            props: *mut c_void,
            user_data_size: usize,
        ) -> *mut PwContext;
        fn pw_context_destroy(context: *mut PwContext);
        fn pw_context_connect(
            context: *mut PwContext,
            properties: *mut c_void,
            user_data_size: usize,
        ) -> *mut PwCore;
        fn pw_core_disconnect(core: *mut PwCore);
        fn pw_core_get_registry(
            core: *mut PwCore,
            version: u32,
            user_data_size: usize,
        ) -> *mut PwRegistry;
        fn pw_registry_add_listener(
            registry: *mut PwRegistry,
            listener: *mut SpaHook,
            events: *const PwRegistryEvents,
            data: *mut c_void,
        ) -> c_int;
        fn pw_loop_iterate(loop_: *mut PwLoop, timeout: c_int) -> c_int;
        fn pw_properties_new_string(args: *const c_char) -> *mut PwProperties;
        fn pw_properties_set(
            properties: *mut PwProperties,
            key: *const c_char,
            value: *const c_char,
        ) -> c_int;
        fn pw_properties_free(properties: *mut PwProperties);
        fn pw_stream_new(
            core: *mut PwCore,
            name: *const c_char,
            props: *mut PwProperties,
        ) -> *mut PwStream;
        fn pw_stream_destroy(stream: *mut PwStream);
        fn pw_stream_add_listener(
            stream: *mut PwStream,
            listener: *mut SpaHook,
            events: *const PwStreamEvents,
            data: *mut c_void,
        );
        fn pw_stream_connect(
            stream: *mut PwStream,
            direction: u32,
            target_id: u32,
            flags: u32,
            params: *mut *const SpaPod,
            n_params: u32,
        ) -> c_int;
        fn pw_stream_dequeue_buffer(stream: *mut PwStream) -> *mut PwBuffer;
        fn pw_stream_queue_buffer(stream: *mut PwStream, buffer: *mut PwBuffer) -> c_int;
        fn pw_proxy_destroy(proxy: *mut PwProxy);
    }

    pub(super) fn snapshot() -> Result<PipeWireAudioGraphSnapshot, AudioError> {
        let mut connection = PipeWireConnection::connect()?;
        connection.enumerate()
    }

    pub(super) fn capture_audio_frame(
        request: PipeWireAudioStreamRequest,
    ) -> Result<AudioFrame, AudioError> {
        let connection = PipeWireConnection::connect()?;
        let (sender, receiver) = sync_channel(1);
        let stream = PipeWireAudioNativeStream::connect_capture(&connection, request, sender)?;
        let started = Instant::now();
        loop {
            match receiver.try_recv() {
                Ok(Ok(frame)) => return Ok(frame),
                Ok(Err(error)) => return Err(AudioError::Backend(error)),
                Err(TryRecvError::Disconnected) => {
                    return Err(AudioError::Backend(
                        "PipeWire audio capture stream disconnected".into(),
                    ));
                }
                Err(TryRecvError::Empty) => {}
            }
            if started.elapsed() >= STREAM_WAIT_TIMEOUT {
                return Err(AudioError::Backend(
                    "timed out waiting for PipeWire audio frame".into(),
                ));
            }
            stream.iterate(100)?;
        }
    }

    pub(super) fn play_audio_frame(
        request: PipeWireAudioStreamRequest,
        frame: AudioFrame,
    ) -> Result<(), AudioError> {
        let connection = PipeWireConnection::connect()?;
        let (sender, receiver) = sync_channel(1);
        let stream =
            PipeWireAudioNativeStream::connect_playback(&connection, request, frame, sender)?;
        let started = Instant::now();
        loop {
            match receiver.try_recv() {
                Ok(Ok(())) => return Ok(()),
                Ok(Err(error)) => return Err(AudioError::Backend(error)),
                Err(TryRecvError::Disconnected) => {
                    return Err(AudioError::Backend(
                        "PipeWire audio playback stream disconnected".into(),
                    ));
                }
                Err(TryRecvError::Empty) => {}
            }
            if started.elapsed() >= STREAM_WAIT_TIMEOUT {
                return Err(AudioError::Backend(
                    "timed out waiting for PipeWire audio playback buffer".into(),
                ));
            }
            stream.iterate(100)?;
        }
    }

    struct PipeWireConnection {
        core: *mut PwCore,
        context: *mut PwContext,
        main_loop: *mut PwMainLoop,
        loop_: *mut PwLoop,
    }

    impl PipeWireConnection {
        fn connect() -> Result<Self, AudioError> {
            unsafe {
                pw_init(ptr::null_mut(), ptr::null_mut());
                let main_loop = pw_main_loop_new(ptr::null());
                if main_loop.is_null() {
                    return Err(AudioError::Backend("create PipeWire main loop".into()));
                }
                let loop_ = pw_main_loop_get_loop(main_loop);
                let context = pw_context_new(loop_, ptr::null_mut(), 0);
                if context.is_null() {
                    pw_main_loop_destroy(main_loop);
                    return Err(AudioError::Backend("create PipeWire context".into()));
                }
                let core = pw_context_connect(context, ptr::null_mut(), 0);
                if core.is_null() {
                    pw_context_destroy(context);
                    pw_main_loop_destroy(main_loop);
                    return Err(AudioError::Backend("connect PipeWire core".into()));
                }
                Ok(Self {
                    core,
                    context,
                    main_loop,
                    loop_,
                })
            }
        }

        fn enumerate(&mut self) -> Result<PipeWireAudioGraphSnapshot, AudioError> {
            let registry = unsafe { pw_core_get_registry(self.core, PW_VERSION_REGISTRY, 0) };
            if registry.is_null() {
                return Err(AudioError::Backend("get PipeWire registry".into()));
            }

            let mut data = RegistryData {
                collector: PipeWireRegistryCollector::default(),
            };
            let mut listener = SpaHook::default();
            let events = PwRegistryEvents {
                version: 0,
                global: Some(on_registry_global),
                global_remove: Some(on_registry_global_remove),
            };
            let res = unsafe {
                pw_registry_add_listener(
                    registry,
                    &mut listener,
                    &events,
                    (&mut data as *mut RegistryData).cast(),
                )
            };
            if res < 0 {
                unsafe { pw_proxy_destroy(registry.cast::<PwProxy>()) };
                return Err(AudioError::Backend(format!(
                    "add PipeWire registry listener: {res}"
                )));
            }

            let started = Instant::now();
            while started.elapsed() < REGISTRY_ENUMERATION_TIMEOUT {
                let res = unsafe { pw_loop_iterate(self.loop_, REGISTRY_ITERATE_TIMEOUT_MS) };
                if res < 0 {
                    unsafe { pw_proxy_destroy(registry.cast::<PwProxy>()) };
                    return Err(AudioError::Backend(format!(
                        "iterate PipeWire registry: {res}"
                    )));
                }
            }

            let _ = &listener;
            let _ = &events;
            unsafe { pw_proxy_destroy(registry.cast::<PwProxy>()) };
            Ok(data.collector.snapshot())
        }
    }

    impl Drop for PipeWireConnection {
        fn drop(&mut self) {
            unsafe {
                pw_core_disconnect(self.core);
                pw_context_destroy(self.context);
                pw_main_loop_destroy(self.main_loop);
            }
        }
    }

    struct PipeWireAudioNativeStream {
        stream: *mut PwStream,
        listener: SpaHook,
        events: PwStreamEvents,
        data: Box<AudioStreamData>,
        loop_: *mut PwLoop,
    }

    impl PipeWireAudioNativeStream {
        fn connect_capture(
            connection: &PipeWireConnection,
            request: PipeWireAudioStreamRequest,
            sender: SyncSender<Result<AudioFrame, String>>,
        ) -> Result<Self, AudioError> {
            let data = AudioStreamData {
                stream: ptr::null_mut(),
                request,
                sequence: 1,
                capture_sender: Some(sender),
                playback_sender: None,
                playback_frame: None,
                playback_done: false,
            };
            Self::connect(connection, data)
        }

        fn connect_playback(
            connection: &PipeWireConnection,
            request: PipeWireAudioStreamRequest,
            frame: AudioFrame,
            sender: SyncSender<Result<(), String>>,
        ) -> Result<Self, AudioError> {
            let data = AudioStreamData {
                stream: ptr::null_mut(),
                request,
                sequence: frame.sequence,
                capture_sender: None,
                playback_sender: Some(sender),
                playback_frame: Some(frame),
                playback_done: false,
            };
            Self::connect(connection, data)
        }

        fn connect(
            connection: &PipeWireConnection,
            mut data: AudioStreamData,
        ) -> Result<Self, AudioError> {
            let props = stream_properties(&data.request)?;
            let name = CString::new("nexkvm-audio").map_err(|error| {
                AudioError::Backend(format!("PipeWire audio stream name: {error}"))
            })?;
            let stream = unsafe { pw_stream_new(connection.core, name.as_ptr(), props) };
            if stream.is_null() {
                unsafe {
                    pw_properties_free(props);
                }
                return Err(AudioError::Backend("create PipeWire audio stream".into()));
            }

            data.stream = stream;
            let mut data = Box::new(data);
            let mut listener = SpaHook::default();
            let events = PwStreamEvents {
                version: 0,
                destroy: None,
                state_changed: None,
                control_info: None,
                io_changed: None,
                param_changed: None,
                add_buffer: None,
                remove_buffer: None,
                process: Some(on_audio_stream_process),
                drained: None,
                command: None,
                trigger_done: None,
            };
            unsafe {
                pw_stream_add_listener(
                    stream,
                    &mut listener,
                    &events,
                    (&mut *data as *mut AudioStreamData).cast(),
                );
            }
            let direction = match data.request.direction {
                PipeWireAudioStreamDirection::Capture => PW_DIRECTION_INPUT,
                PipeWireAudioStreamDirection::Playback => PW_DIRECTION_OUTPUT,
            };
            let flags = PW_STREAM_FLAG_AUTOCONNECT | PW_STREAM_FLAG_MAP_BUFFERS;
            let res = unsafe {
                pw_stream_connect(stream, direction, PW_ID_ANY, flags, ptr::null_mut(), 0)
            };
            if res < 0 {
                unsafe {
                    pw_stream_destroy(stream);
                }
                return Err(AudioError::Backend(format!(
                    "connect PipeWire audio stream: {res}"
                )));
            }

            Ok(Self {
                stream,
                listener,
                events,
                data,
                loop_: connection.loop_,
            })
        }

        fn iterate(&self, timeout_ms: i32) -> Result<(), AudioError> {
            let res = unsafe { pw_loop_iterate(self.loop_, timeout_ms) };
            if res < 0 {
                return Err(AudioError::Backend(format!(
                    "iterate PipeWire audio stream: {res}"
                )));
            }
            Ok(())
        }
    }

    impl Drop for PipeWireAudioNativeStream {
        fn drop(&mut self) {
            let _ = &self.listener;
            let _ = &self.events;
            let _ = &self.data;
            unsafe {
                pw_stream_destroy(self.stream);
            }
        }
    }

    fn stream_properties(
        request: &PipeWireAudioStreamRequest,
    ) -> Result<*mut PwProperties, AudioError> {
        let empty = CString::new("{}")
            .map_err(|error| AudioError::Backend(format!("PipeWire audio props json: {error}")))?;
        let props = unsafe { pw_properties_new_string(empty.as_ptr()) };
        if props.is_null() {
            return Err(AudioError::Backend(
                "create PipeWire audio stream properties".into(),
            ));
        }
        for (key, value) in audio_stream_properties(request) {
            set_property(props, &key, &value)?;
        }
        Ok(props)
    }

    fn set_property(props: *mut PwProperties, key: &str, value: &str) -> Result<(), AudioError> {
        let key = CString::new(key).map_err(|error| {
            AudioError::Backend(format!("PipeWire audio property key: {error}"))
        })?;
        let value = CString::new(value).map_err(|error| {
            AudioError::Backend(format!("PipeWire audio property value: {error}"))
        })?;
        let res = unsafe { pw_properties_set(props, key.as_ptr(), value.as_ptr()) };
        if res < 0 {
            return Err(AudioError::Backend(format!(
                "set PipeWire audio property {}: {res}",
                key.to_string_lossy()
            )));
        }
        Ok(())
    }

    extern "C" fn on_audio_stream_process(data: *mut c_void) {
        if data.is_null() {
            return;
        }
        let data = unsafe { &mut *data.cast::<AudioStreamData>() };
        match data.request.direction {
            PipeWireAudioStreamDirection::Capture => {
                match unsafe { capture_audio_stream_frame(data) } {
                    Ok(Some(frame)) => {
                        if let Some(sender) = &data.capture_sender {
                            let _ = sender.try_send(Ok(frame));
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        if let Some(sender) = &data.capture_sender {
                            let _ = sender.try_send(Err(error));
                        }
                    }
                }
            }
            PipeWireAudioStreamDirection::Playback => {
                if data.playback_done {
                    return;
                }
                match unsafe { write_audio_stream_frame(data) } {
                    Ok(true) => {
                        data.playback_done = true;
                        if let Some(sender) = &data.playback_sender {
                            let _ = sender.try_send(Ok(()));
                        }
                    }
                    Ok(false) => {}
                    Err(error) => {
                        if let Some(sender) = &data.playback_sender {
                            let _ = sender.try_send(Err(error));
                        }
                    }
                }
            }
        }
    }

    unsafe fn capture_audio_stream_frame(
        data: &mut AudioStreamData,
    ) -> Result<Option<AudioFrame>, String> {
        let buffer = unsafe { pw_stream_dequeue_buffer(data.stream) };
        if buffer.is_null() {
            return Ok(None);
        }
        let frame = extract_audio_frame_from_pw_buffer(data, buffer);
        unsafe {
            let _ = pw_stream_queue_buffer(data.stream, buffer);
        }
        frame.map(Some)
    }

    fn extract_audio_frame_from_pw_buffer(
        data: &mut AudioStreamData,
        buffer: *mut PwBuffer,
    ) -> Result<AudioFrame, String> {
        let mapped = mapped_audio_buffer(buffer)?;
        let frame = audio_frame_from_mapped_buffer(mapped, data.request.format, data.sequence)
            .map_err(|error| error.to_string())?;
        data.sequence = data.sequence.saturating_add(1);
        Ok(frame)
    }

    unsafe fn write_audio_stream_frame(data: &mut AudioStreamData) -> Result<bool, String> {
        let Some(frame) = data.playback_frame.take() else {
            return Ok(false);
        };
        let buffer = unsafe { pw_stream_dequeue_buffer(data.stream) };
        if buffer.is_null() {
            data.playback_frame = Some(frame);
            return Ok(false);
        }
        let result = write_audio_frame_to_pw_buffer(buffer, &frame);
        unsafe {
            let _ = pw_stream_queue_buffer(data.stream, buffer);
        }
        result.map(|()| true)
    }

    fn mapped_audio_buffer(
        buffer: *mut PwBuffer,
    ) -> Result<PipeWireAudioMappedBuffer<'static>, String> {
        let spa = unsafe { (*buffer).buffer };
        if spa.is_null() {
            return Err("PipeWire audio buffer did not include a SPA buffer".into());
        }
        let spa = unsafe { &*spa };
        if spa.n_datas == 0 || spa.datas.is_null() {
            return Err("PipeWire audio SPA buffer had no data planes".into());
        }
        let plane = unsafe { &*spa.datas };
        let chunk = if plane.chunk.is_null() {
            return Err("PipeWire audio SPA data plane had no chunk".into());
        } else {
            unsafe { &*plane.chunk }
        };
        let maxsize = usize::try_from(plane.maxsize)
            .map_err(|_| "PipeWire audio data plane maxsize overflow".to_string())?;
        let chunk_offset = if maxsize == 0 {
            0
        } else {
            usize::try_from(chunk.offset)
                .map_err(|_| "PipeWire audio chunk offset overflow".to_string())?
                % maxsize
        };
        let chunk_size = usize::try_from(chunk.size)
            .map_err(|_| "PipeWire audio chunk size overflow".to_string())?
            .min(maxsize.saturating_sub(chunk_offset));
        let bytes = if plane.data.is_null() {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(plane.data.cast::<u8>(), maxsize) }
        };
        Ok(PipeWireAudioMappedBuffer {
            bytes,
            chunk_offset,
            chunk_size,
            capture_time_micros: unsafe { (*buffer).time / 1_000 },
        })
    }

    fn write_audio_frame_to_pw_buffer(
        buffer: *mut PwBuffer,
        frame: &AudioFrame,
    ) -> Result<(), String> {
        let spa = unsafe { (*buffer).buffer };
        if spa.is_null() {
            return Err("PipeWire audio playback buffer did not include a SPA buffer".into());
        }
        let spa = unsafe { &*spa };
        if spa.n_datas == 0 || spa.datas.is_null() {
            return Err("PipeWire audio playback SPA buffer had no data planes".into());
        }
        let plane = unsafe { &mut *spa.datas };
        if plane.data.is_null() {
            return Err("PipeWire audio playback data plane was not mapped".into());
        }
        let maxsize = usize::try_from(plane.maxsize)
            .map_err(|_| "PipeWire audio playback maxsize overflow".to_string())?;
        if frame.payload.len() > maxsize {
            return Err(format!(
                "PipeWire audio playback payload too large: {} > {maxsize}",
                frame.payload.len()
            ));
        }
        let dst = unsafe { std::slice::from_raw_parts_mut(plane.data.cast::<u8>(), maxsize) };
        dst[..frame.payload.len()].copy_from_slice(&frame.payload);
        let chunk = if plane.chunk.is_null() {
            return Err("PipeWire audio playback data plane had no chunk".into());
        } else {
            unsafe { &mut *plane.chunk }
        };
        chunk.offset = 0;
        chunk.size = u32::try_from(frame.payload.len())
            .map_err(|_| "PipeWire audio playback payload length overflow".to_string())?;
        chunk.stride = 0;
        chunk.flags = 0;
        Ok(())
    }

    extern "C" fn on_registry_global(
        data: *mut c_void,
        id: u32,
        _permissions: u32,
        type_: *const c_char,
        _version: u32,
        props: *const SpaDict,
    ) {
        if data.is_null() || type_.is_null() {
            return;
        }
        let type_ = unsafe { CStr::from_ptr(type_) }
            .to_string_lossy()
            .into_owned();
        let properties = unsafe { dict_to_hash_map(props) };
        let data = unsafe { &mut *data.cast::<RegistryData>() };
        data.collector.global(PipeWireRegistryGlobal {
            id,
            type_,
            properties,
        });
    }

    extern "C" fn on_registry_global_remove(data: *mut c_void, id: u32) {
        if data.is_null() {
            return;
        }
        let data = unsafe { &mut *data.cast::<RegistryData>() };
        data.collector.global_remove(id);
    }

    unsafe fn dict_to_hash_map(dict: *const SpaDict) -> HashMap<String, String> {
        if dict.is_null() {
            return HashMap::new();
        }
        let dict = unsafe { &*dict };
        if dict.items.is_null() {
            return HashMap::new();
        }

        let mut out = HashMap::new();
        for index in 0..dict.n_items {
            let item = unsafe { &*dict.items.add(index as usize) };
            if item.key.is_null() || item.value.is_null() {
                continue;
            }
            let key = unsafe { CStr::from_ptr(item.key) }
                .to_string_lossy()
                .into_owned();
            let value = unsafe { CStr::from_ptr(item.value) }
                .to_string_lossy()
                .into_owned();
            out.insert(key, value);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_globals_collect_audio_nodes_and_remove_stale_ids() {
        let mut collector = PipeWireRegistryCollector::default();
        collector.global(PipeWireRegistryGlobal {
            id: 41,
            type_: PIPEWIRE_INTERFACE_NODE.into(),
            properties: HashMap::from([
                ("media.class".into(), "Audio/Sink".into()),
                ("node.description".into(), "Built-in Speakers".into()),
            ]),
        });
        collector.global(PipeWireRegistryGlobal {
            id: 77,
            type_: PIPEWIRE_INTERFACE_NODE.into(),
            properties: HashMap::from([
                ("media.class".into(), "Stream/Output/Audio".into()),
                ("node.name".into(), "browser".into()),
            ]),
        });
        collector.global(PipeWireRegistryGlobal {
            id: 99,
            type_: "PipeWire:Interface:Client".into(),
            properties: HashMap::new(),
        });
        collector.global_remove(77);

        let snapshot = collector.snapshot();
        assert_eq!(snapshot.nodes.len(), 1);
        assert_eq!(snapshot.nodes[0].id, 41);
        assert_eq!(
            snapshot.nodes[0].properties.get("node.description"),
            Some(&"Built-in Speakers".to_string())
        );
    }

    #[test]
    fn wpctl_set_default_command_targets_pipewire_node_id() {
        let command = wpctl_set_default_command(41);
        assert_eq!(command.program, "wpctl");
        assert_eq!(command.args, ["set-default", "41"]);
    }

    #[test]
    fn native_audio_stream_properties_target_requested_node() {
        let capture = audio_stream_properties(&PipeWireAudioStreamRequest {
            node_id: 42,
            direction: PipeWireAudioStreamDirection::Capture,
            format: AudioFormat::default(),
        });
        assert!(capture.contains(&("application.name".to_string(), "NexKVM".to_string())));
        assert!(capture.contains(&("media.type".to_string(), "Audio".to_string())));
        assert!(capture.contains(&("media.category".to_string(), "Capture".to_string())));
        assert!(capture.contains(&("target.object".to_string(), "42".to_string())));

        let playback = audio_stream_properties(&PipeWireAudioStreamRequest {
            node_id: 41,
            direction: PipeWireAudioStreamDirection::Playback,
            format: AudioFormat::default(),
        });
        assert!(playback.contains(&("media.category".to_string(), "Playback".to_string())));
        assert!(playback.contains(&("target.object".to_string(), "41".to_string())));
    }

    #[test]
    fn pipewire_audio_frame_from_mapped_buffer_copies_pcm_chunk() {
        let bytes = [0, 1, 2, 3, 4, 5, 6, 7];
        let frame = audio_frame_from_mapped_buffer(
            PipeWireAudioMappedBuffer {
                bytes: &bytes,
                chunk_offset: 2,
                chunk_size: 4,
                capture_time_micros: 99,
            },
            AudioFormat {
                codec: AudioCodec::Pcm,
                ..AudioFormat::default()
            },
            7,
        )
        .unwrap();

        assert_eq!(frame.sequence, 7);
        assert_eq!(frame.capture_time_micros, 99);
        assert_eq!(
            frame.samples_per_channel,
            AudioFormat::default().samples_per_frame()
        );
        assert_eq!(frame.codec, AudioCodec::Pcm);
        assert_eq!(frame.payload, Bytes::from_static(&[2, 3, 4, 5]));
    }

    #[tokio::test]
    async fn pipewire_audio_backend_routes_stream_frames_by_node_id() {
        use bytes::Bytes;
        use nexkvm_streaming::{AudioCodec, AudioFrame, AudioStreamBackend};

        let frame = AudioFrame {
            sequence: 2,
            capture_time_micros: 20,
            samples_per_channel: 480,
            codec: AudioCodec::Pcm,
            payload: Bytes::from_static(b"pcm"),
        };
        let stream = StaticPipeWireAudioStream::new(vec![(42, frame.clone())]);
        let backend = PipeWireAudioBackend::with_stream(
            StaticPipeWireAudioGraph::new(PipeWireAudioGraphSnapshot {
                nodes: vec![
                    PipeWireAudioNode::new(41).with_property("media.class", "Audio/Sink"),
                    PipeWireAudioNode::new(42).with_property("media.class", "Audio/Source"),
                ],
            }),
            stream.clone(),
        );

        let captured = backend
            .capture_audio_frame(&AudioDeviceId::new("pipewire-node:42"))
            .await
            .unwrap();
        assert_eq!(captured, frame);

        backend
            .play_audio_frame(&AudioDeviceId::new("pipewire-node:41"), captured.clone())
            .await
            .unwrap();
        assert_eq!(stream.played_frames(), vec![(41, captured)]);
        assert!(
            backend
                .capture_audio_frame(&AudioDeviceId::new("alsa:42"))
                .await
                .is_err()
        );
    }
}
