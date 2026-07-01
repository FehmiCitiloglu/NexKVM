//! Linux PipeWire audio routing backend scaffolding.
//!
//! PipeWire exposes audio devices as graph nodes. This module maps the stable
//! node metadata into NexKVM's platform-neutral [`AudioBackend`] boundary before
//! native graph mutation is wired in later slices.

use async_trait::async_trait;
use nexkvm_streaming::{
    AudioBackend, AudioDevice, AudioDeviceId, AudioDeviceRole, AudioError, AudioFormat,
};
use std::collections::HashMap;

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
