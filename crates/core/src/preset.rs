use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum ChannelLayout {
    #[default]
    Stereo,
    Mono,
    Surround51,
    Surround71,
}

impl ChannelLayout {
    pub fn channels(&self) -> u8 {
        match self {
            Self::Mono       => 1,
            Self::Stereo     => 2,
            Self::Surround51 => 6,
            Self::Surround71 => 8,
        }
    }

    pub fn position(&self) -> &'static str {
        match self {
            Self::Mono       => "[ MONO ]",
            Self::Stereo     => "[ FL FR ]",
            Self::Surround51 => "[ FL FR FC LFE RL RR ]",
            Self::Surround71 => "[ FL FR FC LFE RL RR SL SR ]",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Preset {
    pub id: String,
    pub name: String,
    /// Ordered chain of effect plugins. Audio flows from index 0 → N.
    #[serde(default)]
    pub chain: Vec<ChainNode>,
    pub outputs: Vec<OutputAssignment>,
    #[serde(default)]
    pub channels: ChannelLayout,
    #[serde(default)]
    pub source_inputs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainNode {
    /// Identifier unique within the preset (used as the filter-graph node name).
    pub id: String,
    /// Optional human label; falls back to the plugin name in the UI.
    #[serde(default)]
    pub label: Option<String>,
    /// When true the node is replaced by a passthrough in the generated graph.
    #[serde(default)]
    pub bypass: bool,
    pub kind: ChainNodeKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChainNodeKind {
    /// LV2 plugin referenced by URI. Controls map LV2 symbols → values.
    Lv2 {
        plugin_uri: String,
        #[serde(default)]
        controls: BTreeMap<String, f32>,
    },
    /// Built-in filter-chain primitive (`copy`, `bq_*`, `mixer`, etc.).
    Builtin {
        label: String,
        #[serde(default)]
        controls: BTreeMap<String, f32>,
    },
    /// LADSPA plugin referenced by `.so` path + label. Controls map
    /// LADSPA port *names* (case-sensitive, as printed by `analyseplugin`).
    Ladspa {
        path: String,
        label: String,
        #[serde(default)]
        controls: BTreeMap<String, f32>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputAssignment {
    pub node_name: String,
    pub volume: f32,
}

impl Preset {
    pub fn new(name: &str) -> Self {
        Self {
            id: make_id(name),
            name: name.to_owned(),
            chain: Vec::new(),
            outputs: Vec::new(),
            channels: ChannelLayout::default(),
            source_inputs: Vec::new(),
        }
    }

    /// Node name used in PipeWire for this preset's virtual sink.
    pub fn pw_node_name(&self) -> String {
        format!("pw-ctrl.preset.{}", self.id)
    }

    /// Generate a fresh chain-node id that doesn't collide with existing ones.
    pub fn next_chain_id(&self) -> String {
        for n in 0.. {
            let id = format!("n{n}");
            if !self.chain.iter().any(|c| c.id == id) {
                return id;
            }
        }
        unreachable!()
    }
}

pub fn make_id(name: &str) -> String {
    let slug: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    slug.trim_matches('-').to_owned()
}
