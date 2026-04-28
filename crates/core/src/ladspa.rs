//! LADSPA plugin catalog.
//!
//! Discovers installed LADSPA plugins by shelling out to `listplugins` and
//! `analyseplugin`. Mirrors the shape of `lv2.rs` so the web UI can render
//! controls the same way.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    process::Command,
    sync::{mpsc, Arc, Mutex},
};

// ── Public types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LadspaPlugin {
    /// Absolute path to the `.so` file.
    pub path: String,
    /// Plugin label (the in-file identifier, unique per `.so`).
    pub label: String,
    pub unique_id: u32,
    pub name: String,
    pub maker: Option<String>,
    pub ports: Vec<LadspaPort>,
}

impl LadspaPlugin {
    /// Stable opaque key used in URLs and chain references: `{path}::{label}`.
    pub fn key(&self) -> String {
        format!("{}::{}", self.path, self.label)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LadspaPort {
    /// Declaration order index (0-based).
    pub index: u32,
    pub name: String,
    pub direction: PortDirection,
    pub kind: PortKind,
    pub min: Option<f32>,
    pub max: Option<f32>,
    pub default: Option<f32>,
    pub flags: PortFlags,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum PortDirection { Input, Output }

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum PortKind { Audio, Control }

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PortFlags {
    pub toggled: bool,
    pub integer: bool,
    pub logarithmic: bool,
    pub sample_rate: bool,
}

// ── analyseplugin parser ──────────────────────────────────────────────────────

/// Parse the textual output of `analyseplugin <path>` (which may contain many
/// plugin blocks back-to-back) into a list of plugins, all tagged with `path`.
pub fn parse_analyseplugin(text: &str, path: &str) -> Vec<LadspaPlugin> {
    let mut plugins = Vec::new();
    let mut buf: Vec<&str> = Vec::new();
    for line in text.lines() {
        if line.starts_with("Plugin Name:") && !buf.is_empty() {
            if let Some(p) = parse_block(&buf, path) { plugins.push(p); }
            buf.clear();
        }
        buf.push(line);
    }
    if !buf.is_empty() {
        if let Some(p) = parse_block(&buf, path) { plugins.push(p); }
    }
    plugins
}

fn parse_block(lines: &[&str], path: &str) -> Option<LadspaPlugin> {
    let mut name = String::new();
    let mut label = String::new();
    let mut unique_id: u32 = 0;
    let mut maker: Option<String> = None;
    let mut ports: Vec<LadspaPort> = Vec::new();
    let mut in_ports = false;

    for raw in lines {
        let trimmed = raw.trim();
        if trimmed.is_empty() { continue; }

        // Port lines start with a quoted port name (after a leading tab).
        if in_ports && trimmed.starts_with('"') {
            if let Some(p) = parse_port_line(trimmed, ports.len() as u32) {
                ports.push(p);
            }
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("Plugin Name:") {
            name = unquote(rest.trim());
        } else if let Some(rest) = trimmed.strip_prefix("Plugin Label:") {
            label = unquote(rest.trim());
        } else if let Some(rest) = trimmed.strip_prefix("Plugin Unique ID:") {
            unique_id = rest.trim().parse().unwrap_or(0);
        } else if let Some(rest) = trimmed.strip_prefix("Maker:") {
            let s = unquote(rest.trim());
            if !s.is_empty() && s != "None" { maker = Some(s); }
        } else if trimmed.starts_with("Ports:") {
            in_ports = true;
            // The first port may share the same line after "Ports:".
            let after = trimmed.trim_start_matches("Ports:").trim();
            if after.starts_with('"') {
                if let Some(p) = parse_port_line(after, ports.len() as u32) {
                    ports.push(p);
                }
            }
        }
    }

    if label.is_empty() { return None; }
    if name.is_empty() { name = label.clone(); }
    Some(LadspaPlugin {
        path: path.to_string(),
        label, unique_id, name, maker, ports,
    })
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        s[1..s.len()-1].to_string()
    } else {
        s.to_string()
    }
}

/// Parse one port line like:
/// `"Gain" input, control, 0 to ..., default 1, logarithmic`
fn parse_port_line(line: &str, index: u32) -> Option<LadspaPort> {
    // Split into "name" and the rest.
    let rest = line.strip_prefix('"')?;
    let close = rest.find('"')?;
    let name = rest[..close].to_string();
    let tail = rest[close+1..].trim_start_matches([' ', ',']).to_string();

    // Tail is comma-separated. First two tokens are direction and kind.
    let parts: Vec<&str> = tail.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
    if parts.len() < 2 { return None; }

    let direction = match parts[0] {
        "input"  => PortDirection::Input,
        "output" => PortDirection::Output,
        _        => return None,
    };
    let kind = match parts[1] {
        "audio"   => PortKind::Audio,
        "control" => PortKind::Control,
        _         => return None,
    };

    let mut min: Option<f32> = None;
    let mut max: Option<f32> = None;
    let mut default: Option<f32> = None;
    let mut flags = PortFlags::default();

    for tok in parts.iter().skip(2) {
        let t = *tok;
        if let Some(rest) = t.strip_prefix("default ") {
            default = parse_num(rest.trim());
        } else if t.contains(" to ") {
            // Range: "<min> to <max>" — either side may be "..." (unbounded)
            // and either may carry a "*srate" suffix we ignore.
            if let Some((a, b)) = t.split_once(" to ") {
                min = parse_num(a.trim());
                max = parse_num(b.trim());
            }
        } else {
            match t {
                "logarithmic" => flags.logarithmic = true,
                "integer"     => flags.integer = true,
                "toggled"     => flags.toggled = true,
                _ if t.contains("sample rate") || t.contains("SAMPLE RATE") => flags.sample_rate = true,
                _ => {}
            }
        }
    }

    Some(LadspaPort { index, name, direction, kind, min, max, default, flags })
}

fn parse_num(s: &str) -> Option<f32> {
    let s = s.trim();
    if s.starts_with("...") || s.is_empty() { return None; }
    // Strip trailing "*srate" or " (...)" annotations.
    let s = s.split_whitespace().next().unwrap_or(s);
    let s = s.trim_end_matches(['*']);
    s.parse::<f32>().ok()
}

/// Pick the first audio port matching `direction`, in declaration order.
pub fn primary_audio_port(plugin: &LadspaPlugin, direction: PortDirection) -> Option<&LadspaPort> {
    plugin.ports.iter().find(|p| p.kind == PortKind::Audio && p.direction == direction)
}

// ── Discovery ─────────────────────────────────────────────────────────────────

/// Run `listplugins` to find every `.so` file with at least one plugin, then
/// `analyseplugin <path>` per file to extract the structured info.
pub fn scan_plugins() -> Result<Vec<LadspaPlugin>> {
    let out = Command::new("listplugins").output()
        .map_err(|e| anyhow!("failed to run listplugins: {e}. Is ladspa-sdk installed?"))?;
    if !out.status.success() {
        return Err(anyhow!("listplugins exited with {}", out.status));
    }
    let text = String::from_utf8_lossy(&out.stdout).into_owned();

    // listplugins prints `<path>:` lines followed by indented `Name (id/label)`
    // entries. We only need the unique set of `.so` paths.
    let mut paths: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim_end();
        if line.ends_with(':') && line.starts_with('/') {
            paths.push(line.trim_end_matches(':').to_string());
        }
    }

    // analyseplugin in parallel with a small worker pool.
    let workers = 8usize;
    let queue = Arc::new(Mutex::new(paths.into_iter()));
    let (tx, rx) = mpsc::channel::<Vec<LadspaPlugin>>();
    let mut handles = Vec::with_capacity(workers);
    for _ in 0..workers {
        let queue = queue.clone();
        let tx = tx.clone();
        handles.push(std::thread::spawn(move || {
            loop {
                let next = { queue.lock().unwrap().next() };
                let Some(path) = next else { break };
                let result = Command::new("analyseplugin").arg(&path).output();
                let plugins = match result {
                    Ok(o) if o.status.success() => {
                        let text = String::from_utf8_lossy(&o.stdout);
                        parse_analyseplugin(&text, &path)
                    }
                    Ok(o) => {
                        tracing::warn!("analyseplugin {path} exited {:?}", o.status);
                        Vec::new()
                    }
                    Err(e) => {
                        tracing::warn!("analyseplugin {path} spawn failed: {e}");
                        Vec::new()
                    }
                };
                let _ = tx.send(plugins);
            }
        }));
    }
    drop(tx);

    let mut plugins: Vec<LadspaPlugin> = Vec::new();
    while let Ok(batch) = rx.recv() {
        plugins.extend(batch);
    }
    for h in handles { let _ = h.join(); }

    plugins.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(plugins)
}

// ── Cache ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LadspaCatalog {
    pub plugins: Vec<LadspaPlugin>,
}

impl LadspaCatalog {
    pub fn cache_path() -> PathBuf {
        dirs::cache_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("pipewire-control")
            .join("ladspa-catalog.json")
    }

    pub fn load() -> Result<Self> {
        let raw = std::fs::read_to_string(Self::cache_path())?;
        Ok(serde_json::from_str(&raw)?)
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::cache_path();
        if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }
        std::fs::write(&path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn rescan() -> Result<Self> {
        let plugins = scan_plugins()?;
        let cat = Self { plugins };
        let _ = cat.save();
        Ok(cat)
    }

    /// Look up by the `{path}::{label}` opaque key.
    pub fn find(&self, key: &str) -> Option<&LadspaPlugin> {
        let (path, label) = key.split_once("::")?;
        self.plugins.iter().find(|p| p.path == path && p.label == label)
    }

    pub fn find_pair(&self, path: &str, label: &str) -> Option<&LadspaPlugin> {
        self.plugins.iter().find(|p| p.path == path && p.label == label)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
Plugin Name: "Mono Amplifier"
Plugin Label: "amp_mono"
Plugin Unique ID: 1048
Maker: "Richard Furse (LADSPA example plugins)"
Copyright: "None"
Must Run Real-Time: No
Ports:	"Gain" input, control, 0 to ..., default 1, logarithmic
	"Input" input, audio
	"Output" output, audio

Plugin Name: "Stereo Amplifier"
Plugin Label: "amp_stereo"
Plugin Unique ID: 1049
Maker: "Richard Furse (LADSPA example plugins)"
Ports:	"Gain" input, control, 0 to ..., default 1, logarithmic
	"Input (Left)" input, audio
	"Output (Left)" output, audio
	"Input (Right)" input, audio
	"Output (Right)" output, audio
"#;

    #[test]
    fn parses_two_plugins() {
        let plugins = parse_analyseplugin(SAMPLE, "/usr/lib/ladspa/amp.so");
        assert_eq!(plugins.len(), 2);
        assert_eq!(plugins[0].label, "amp_mono");
        assert_eq!(plugins[0].name, "Mono Amplifier");
        assert_eq!(plugins[0].unique_id, 1048);
        assert_eq!(plugins[1].label, "amp_stereo");
    }

    #[test]
    fn parses_control_port_with_default_and_log() {
        let p = &parse_analyseplugin(SAMPLE, "/x.so")[0];
        let gain = p.ports.iter().find(|x| x.name == "Gain").unwrap();
        assert_eq!(gain.kind, PortKind::Control);
        assert_eq!(gain.direction, PortDirection::Input);
        assert_eq!(gain.min, Some(0.0));
        assert_eq!(gain.max, None);
        assert_eq!(gain.default, Some(1.0));
        assert!(gain.flags.logarithmic);
    }

    #[test]
    fn primary_audio_picks_first_in_order() {
        let p = &parse_analyseplugin(SAMPLE, "/x.so")[1];
        let i = primary_audio_port(p, PortDirection::Input).unwrap();
        let o = primary_audio_port(p, PortDirection::Output).unwrap();
        assert_eq!(i.name, "Input (Left)");
        assert_eq!(o.name, "Output (Left)");
    }

    #[test]
    fn parses_integer_range() {
        let line = r#""VAD Threshold (%)" input, control, 0 to 99, default 49.5, integer"#;
        let p = parse_port_line(line, 0).unwrap();
        assert_eq!(p.min, Some(0.0));
        assert_eq!(p.max, Some(99.0));
        assert_eq!(p.default, Some(49.5));
        assert!(p.flags.integer);
    }
}
