use pipewire::spa;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Snapshot of a single PipeWire node we care about.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioNode {
    pub id: u32,
    pub media_class: Option<String>,
    pub node_name: Option<String>,
    pub node_description: Option<String>,
    pub node_nick: Option<String>,
    pub application_name: Option<String>,
    pub application_process_id: Option<u32>,
    pub application_process_binary: Option<String>,
    pub media_name: Option<String>,
    pub client_id: Option<u32>,
    pub device_id: Option<u32>,
    pub serial: Option<u64>,
    pub state: NodeState,
    pub n_input_ports: u32,
    pub n_output_ports: u32,
    /// Raw key-value properties for anything not covered above.
    pub extra_props: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum NodeState {
    Creating,
    Suspended,
    Idle,
    Running,
    Error(String),
}

impl std::fmt::Display for NodeState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NodeState::Creating => write!(f, "creating"),
            NodeState::Suspended => write!(f, "suspended"),
            NodeState::Idle => write!(f, "idle"),
            NodeState::Running => write!(f, "running"),
            NodeState::Error(e) => write!(f, "error: {e}"),
        }
    }
}

/// A kind of node, derived from `media.class`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum NodeKind {
    Sink,
    Source,
    StreamOutput,
    StreamInput,
    Duplex,
    Other(String),
}

impl NodeKind {
    pub fn from_media_class(class: &str) -> Self {
        match class {
            "Audio/Sink" => NodeKind::Sink,
            "Audio/Source" => NodeKind::Source,
            "Stream/Output/Audio" => NodeKind::StreamOutput,
            "Stream/Input/Audio" => NodeKind::StreamInput,
            "Audio/Duplex" => NodeKind::Duplex,
            other => NodeKind::Other(other.to_owned()),
        }
    }
}

impl AudioNode {
    /// Parse a PipeWire property dict into an AudioNode (initial state, no info yet).
    pub fn from_props(id: u32, props: &spa::utils::dict::DictRef) -> Self {
        // Collect everything into a map first for convenience.
        let all: HashMap<String, String> = props
            .iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();

        let get = |key: &str| all.get(key).cloned();
        let get_u32 = |key: &str| all.get(key).and_then(|v| v.parse().ok());
        let get_u64 = |key: &str| all.get(key).and_then(|v| v.parse().ok());

        // Keys that are promoted to named fields — the rest land in extra_props.
        const KNOWN: &[&str] = &[
            "media.class",
            "node.name",
            "node.description",
            "node.nick",
            "application.name",
            "application.process.id",
            "application.process.binary",
            "media.name",
            "client.id",
            "device.id",
            "object.serial",
        ];
        let extra_props = all
            .iter()
            .filter(|(k, _)| !KNOWN.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        Self {
            id,
            media_class: get("media.class"),
            node_name: get("node.name"),
            node_description: get("node.description"),
            node_nick: get("node.nick"),
            application_name: get("application.name"),
            application_process_id: get_u32("application.process.id"),
            application_process_binary: get("application.process.binary"),
            media_name: get("media.name"),
            client_id: get_u32("client.id"),
            device_id: get_u32("device.id"),
            serial: get_u64("object.serial"),
            state: NodeState::Creating,
            n_input_ports: 0,
            n_output_ports: 0,
            extra_props,
        }
    }

    pub fn kind(&self) -> Option<NodeKind> {
        self.media_class.as_deref().map(NodeKind::from_media_class)
    }

    pub fn is_audio(&self) -> bool {
        self.media_class
            .as_deref()
            .map(|c| c.starts_with("Audio") || c.starts_with("Stream"))
            .unwrap_or(false)
    }

    /// Human-readable display name: tries nick → description → name → id.
    pub fn display_name(&self) -> String {
        self.node_nick
            .as_deref()
            .or(self.node_description.as_deref())
            .or(self.node_name.as_deref())
            .unwrap_or("(unnamed)")
            .to_owned()
    }
}

/// A directed connection between two PipeWire nodes.
/// `output_node` sends audio → `input_node` receives it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioLink {
    pub id: u32,
    /// Node that produces audio (e.g. a Stream/Output/Audio).
    pub output_node: u32,
    /// Node that consumes audio (e.g. an Audio/Sink).
    pub input_node: u32,
    pub active: bool,
}

/// Events broadcast from the PipeWire engine to any subscriber.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum PwEvent {
    NodeAdded(AudioNode),
    NodeUpdated(AudioNode),
    NodeRemoved { id: u32 },
    LinkAdded(AudioLink),
    LinkRemoved { id: u32 },
    /// Full graph state sent on WebSocket connect and after lag recovery.
    Snapshot { nodes: Vec<AudioNode>, links: Vec<AudioLink> },
}

/// Commands sent from other threads into the PipeWire engine loop.
pub enum EngineCmd {
    Shutdown,
}
