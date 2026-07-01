//! Linux PipeWire audio routing backend scaffolding.
//!
//! PipeWire exposes audio devices as graph nodes. This module maps the stable
//! node metadata into NexKVM's platform-neutral [`AudioBackend`] boundary before
//! native graph mutation is wired in later slices.

use async_trait::async_trait;
use nexkvm_streaming::{
    AudioBackend, AudioDevice, AudioDeviceId, AudioDeviceRole, AudioError, AudioFormat,
};
use std::collections::{BTreeMap, HashMap};

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

/// Static graph used by tests and diagnostics.
#[derive(Debug, Clone)]
pub struct StaticPipeWireAudioGraph {
    snapshot: PipeWireAudioGraphSnapshot,
}

/// Native PipeWire graph accessor for the current Linux user session.
#[derive(Debug, Clone, Copy, Default)]
pub struct NativePipeWireAudioGraph;

impl StaticPipeWireAudioGraph {
    /// Create a static graph from a snapshot.
    #[must_use]
    pub const fn new(snapshot: PipeWireAudioGraphSnapshot) -> Self {
        Self { snapshot }
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
impl PipeWireAudioGraph for NativePipeWireAudioGraph {
    async fn snapshot(&self) -> Result<PipeWireAudioGraphSnapshot, AudioError> {
        native_pipewire_audio_snapshot().await
    }

    async fn set_default_playback(&self, _node_id: u32) -> Result<(), AudioError> {
        Err(AudioError::Unsupported(
            "PipeWire default playback mutation is not wired yet",
        ))
    }
}

/// PipeWire-backed Linux audio backend.
#[derive(Debug, Clone)]
pub struct PipeWireAudioBackend<G> {
    graph: G,
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
            preferred_format: AudioFormat::opus_stereo_48k(),
        }
    }
}

#[async_trait]
impl<G> AudioBackend for PipeWireAudioBackend<G>
where
    G: PipeWireAudioGraph,
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
#[allow(clashing_extern_declarations)]
mod native_pipewire {
    use super::{
        AudioError, PipeWireAudioGraphSnapshot, PipeWireRegistryCollector, PipeWireRegistryGlobal,
    };
    use std::collections::HashMap;
    use std::ffi::{CStr, c_char, c_int, c_void};
    use std::ptr;
    use std::time::{Duration, Instant};

    const REGISTRY_ENUMERATION_TIMEOUT: Duration = Duration::from_millis(500);
    const REGISTRY_ITERATE_TIMEOUT_MS: c_int = 25;
    const PW_VERSION_REGISTRY: u32 = 3;

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
        fn pw_proxy_destroy(proxy: *mut PwProxy);
    }

    pub(super) fn snapshot() -> Result<PipeWireAudioGraphSnapshot, AudioError> {
        let mut connection = PipeWireConnection::connect()?;
        connection.enumerate()
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
}
