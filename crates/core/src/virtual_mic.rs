use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VirtualMic {
    pub id: String,
    pub name: String,
    pub inputs: Vec<MicInput>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MicInput {
    pub node_name: String,
    pub gain: f32,
}

impl VirtualMic {
    pub fn new(name: &str) -> Self {
        Self {
            id: make_mic_id(name),
            name: name.to_owned(),
            inputs: vec![],
        }
    }
}

pub fn make_mic_id(name: &str) -> String {
    let slug: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    slug.trim_matches('-').to_owned()
}
