use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A logical audio endpoint (virtual sink or source created in PipeWire).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VirtualNode {
    pub id: u32,
    pub name: String,
    pub kind: NodeKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NodeKind {
    Sink,
    Source,
}

/// Maps application stream PipeWire node IDs to virtual node IDs.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Router {
    nodes: HashMap<u32, VirtualNode>,
    /// stream_id -> virtual_node_id
    routes: HashMap<u32, u32>,
}

impl Router {
    pub fn add_node(&mut self, node: VirtualNode) {
        self.nodes.insert(node.id, node);
    }

    pub fn remove_node(&mut self, node_id: u32) {
        self.nodes.remove(&node_id);
        self.routes.retain(|_, v| *v != node_id);
    }

    pub fn route(&mut self, stream_id: u32, target_node_id: u32) {
        self.routes.insert(stream_id, target_node_id);
    }

    pub fn unroute(&mut self, stream_id: u32) {
        self.routes.remove(&stream_id);
    }

    pub fn target_for(&self, stream_id: u32) -> Option<&VirtualNode> {
        self.routes
            .get(&stream_id)
            .and_then(|id| self.nodes.get(id))
    }
}
