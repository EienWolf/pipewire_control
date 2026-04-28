use crate::{preset::Preset, soundboard::SoundClip, virtual_mic::VirtualMic};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct AppState {
    pub active_profile: Option<String>,
    #[serde(default)]
    pub presets: Vec<Preset>,
    #[serde(default)]
    pub virtual_mics: Vec<VirtualMic>,
    /// node_name → preset_id — persisted WirePlumber routing rules
    #[serde(default)]
    pub stream_routes: HashMap<String, String>,
    #[serde(default)]
    pub clips: Vec<SoundClip>,
}

impl AppState {
    pub fn config_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("~/.config"))
            .join("pipewire-control")
            .join("state.toml")
    }

    pub fn load() -> Result<Self> {
        let path = Self::config_path();
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&path)?;
        Ok(toml::from_str(&raw)?)
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, toml::to_string_pretty(self)?)?;
        Ok(())
    }
}
