//! LV2 plugin catalog.
//!
//! Discovers installed LV2 plugins by shelling out to `lv2ls` and `lv2info`,
//! parses the textual `lv2info` output into a structured form usable by the
//! web UI's dynamic control generator, and caches the result on disk.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    process::Command,
    sync::{mpsc, Arc, Mutex},
};

// ── Public types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lv2Plugin {
    pub uri: String,
    pub name: String,
    pub class: String,
    pub author: Option<String>,
    pub has_native_ui: bool,
    pub ports: Vec<Lv2Port>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lv2Port {
    pub index: u32,
    pub symbol: String,
    pub name: String,
    pub direction: PortDirection,
    pub kind: PortKind,
    pub designation: Option<String>,
    pub group: Option<String>,
    pub min: Option<f32>,
    pub max: Option<f32>,
    pub default: Option<f32>,
    /// (value, label) pairs for enumeration ports.
    pub scale_points: Vec<(f32, String)>,
    pub flags: PortFlags,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum PortDirection { Input, Output }

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum PortKind { Audio, Control, Atom, Cv, Other }

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PortFlags {
    pub toggled: bool,
    pub integer: bool,
    pub logarithmic: bool,
    pub enumeration: bool,
    pub strict_bounds: bool,
    pub side_chain: bool,
    pub optional: bool,
    pub hidden: bool,
    pub reports_latency: bool,
}

// ── lv2info parser ────────────────────────────────────────────────────────────

/// Parse the textual output of `lv2info <uri>` into a structured plugin.
pub fn parse_lv2info(text: &str) -> Result<Lv2Plugin> {
    let mut lines = text.lines().peekable();

    // First non-blank line: the plugin URI itself.
    let uri = loop {
        match lines.next() {
            Some(l) if l.trim().is_empty() => continue,
            Some(l) => break l.trim().to_string(),
            None => return Err(anyhow!("empty lv2info output")),
        }
    };

    let mut name = String::new();
    let mut class = String::from("Plugin");
    let mut class_set = false;
    let mut author: Option<String> = None;
    let mut has_native_ui = false;
    let mut ports: Vec<Lv2Port> = Vec::new();

    // State while consuming the file.
    let mut current_port: Option<Lv2Port> = None;
    // Property continuations (e.g. multi-URI Type: or Properties:) go onto this key:
    let mut last_key: Option<&'static str> = None;
    // Whether we're inside a "Scale Points:" indented block of the current port.
    let mut in_scale_points = false;

    fn finish_port(current: &mut Option<Lv2Port>, ports: &mut Vec<Lv2Port>) {
        if let Some(p) = current.take() {
            ports.push(p);
        }
    }

    for raw in lines {
        let line = raw;
        let trimmed = line.trim();

        // Track UIs section by detecting "Class:" pointing to ui#... after we saw a UIs: header.
        if trimmed.starts_with("http://lv2plug.in/ns/extensions/ui#")
            || trimmed.contains("ui#X11UI")
            || trimmed.contains("ui#GtkUI")
            || trimmed.contains("ui#Qt5UI")
        {
            has_native_ui = true;
        }

        // New port starts a new block.
        if let Some(rest) = trimmed.strip_prefix("Port ") {
            finish_port(&mut current_port, &mut ports);
            let idx = rest.trim_end_matches(':')
                .parse::<u32>()
                .unwrap_or(0);
            current_port = Some(Lv2Port {
                index: idx,
                symbol: String::new(),
                name: String::new(),
                direction: PortDirection::Input,
                kind: PortKind::Other,
                designation: None,
                group: None,
                min: None,
                max: None,
                default: None,
                scale_points: Vec::new(),
                flags: PortFlags::default(),
            });
            last_key = None;
            in_scale_points = false;
            continue;
        }

        // Continuation lines (no key: prefix) belong to the previous key.
        // lv2info aligns continuations with whitespace; detect by absence of "key: value".
        let kv = split_kv(line);

        if let Some((key, value)) = kv {
            // New key — closes any pending scale-points block.
            if key != "Scale Points" { in_scale_points = false; }

            match (current_port.as_mut(), key) {
                // ── Plugin header keys ────────────────────────────────────
                (None, "Name")    => name = value.to_string(),
                (None, "Class") if !class_set => { class = value.to_string(); class_set = true; }
                (None, "Author")  => author = Some(value.to_string()),

                // ── Per-port keys ─────────────────────────────────────────
                (Some(p), "Type") => {
                    apply_type_uri(p, value);
                    last_key = Some("Type");
                }
                (Some(p), "Symbol") => { p.symbol = value.to_string(); last_key = None; }
                (Some(p), "Name")   => { p.name = value.to_string(); last_key = None; }
                (Some(p), "Group")  => { p.group = Some(value.to_string()); last_key = None; }
                (Some(p), "Designation") => { p.designation = Some(value.to_string()); last_key = None; }
                (Some(p), "Minimum") => { p.min = value.parse().ok(); last_key = None; }
                (Some(p), "Maximum") => { p.max = value.parse().ok(); last_key = None; }
                (Some(p), "Default") => { p.default = value.parse().ok(); last_key = None; }
                (Some(p), "Properties") => {
                    apply_property_uri(&mut p.flags, value);
                    last_key = Some("Properties");
                }
                (Some(_), "Scale Points") => {
                    // Header line; values are on following indented lines.
                    in_scale_points = true;
                    last_key = None;
                }
                _ => { last_key = None; }
            }
        } else if !trimmed.is_empty() {
            // Continuation line. Two cases we care about:
            //  - Inside a Scale Points block: lines like `<value> = "<label>"`.
            //  - After a Type: or Properties: line: a continuation URI.
            if in_scale_points {
                if let Some(p) = current_port.as_mut() {
                    if let Some((v, label)) = parse_scale_point(trimmed) {
                        p.scale_points.push((v, label));
                    }
                }
                continue;
            }
            match (current_port.as_mut(), last_key) {
                (Some(p), Some("Type")) => apply_type_uri(p, trimmed),
                (Some(p), Some("Properties")) => apply_property_uri(&mut p.flags, trimmed),
                _ => {}
            }
        }
    }

    finish_port(&mut current_port, &mut ports);

    if name.is_empty() {
        // Fallback to a short name from the URI.
        name = uri.rsplit('/').next().unwrap_or(&uri).to_string();
    }

    // Mark enumeration if scale points exist (lv2info usually sets the property too).
    for p in &mut ports {
        if !p.scale_points.is_empty() { p.flags.enumeration = true; }
    }

    Ok(Lv2Plugin { uri, name, class, author, has_native_ui, ports })
}

fn split_kv(line: &str) -> Option<(&str, &str)> {
    // Keys are at the start of the trimmed line and end with ":".
    // We need to stop at the first ":" only when it's not part of a URI.
    let trimmed = line.trim_start();
    // Accept either "Key: value" or a header line like "Scale Points:" with no value.
    let (key, value) = if let Some(idx) = trimmed.find(": ") {
        (trimmed[..idx].trim(), trimmed[idx + 2..].trim())
    } else if let Some(stripped) = trimmed.strip_suffix(':') {
        (stripped.trim(), "")
    } else {
        return None;
    };
    if key.is_empty() || key.contains('/') || key.contains('#') { return None; }
    Some((key, value))
}

fn apply_type_uri(port: &mut Lv2Port, uri: &str) {
    match uri {
        "http://lv2plug.in/ns/lv2core#AudioPort"   => port.kind = PortKind::Audio,
        "http://lv2plug.in/ns/lv2core#ControlPort" => port.kind = PortKind::Control,
        "http://lv2plug.in/ns/lv2core#CVPort"      => port.kind = PortKind::Cv,
        "http://lv2plug.in/ns/ext/atom#AtomPort"   => port.kind = PortKind::Atom,
        "http://lv2plug.in/ns/lv2core#InputPort"   => port.direction = PortDirection::Input,
        "http://lv2plug.in/ns/lv2core#OutputPort"  => port.direction = PortDirection::Output,
        _ => {}
    }
}

fn apply_property_uri(flags: &mut PortFlags, uri: &str) {
    match uri {
        "http://lv2plug.in/ns/lv2core#toggled"            => flags.toggled = true,
        "http://lv2plug.in/ns/lv2core#integer"            => flags.integer = true,
        "http://lv2plug.in/ns/lv2core#enumeration"        => flags.enumeration = true,
        "http://lv2plug.in/ns/lv2core#isSideChain"        => flags.side_chain = true,
        "http://lv2plug.in/ns/lv2core#connectionOptional" => flags.optional = true,
        "http://lv2plug.in/ns/lv2core#reportsLatency"     => flags.reports_latency = true,
        "http://lv2plug.in/ns/ext/port-props#logarithmic"     => flags.logarithmic = true,
        "http://lv2plug.in/ns/ext/port-props#hasStrictBounds" => flags.strict_bounds = true,
        "http://lv2plug.in/ns/ext/port-props#notOnGUI"        => flags.hidden = true,
        _ => {}
    }
}

fn parse_scale_point(line: &str) -> Option<(f32, String)> {
    // Format: `<number> = "<label>"`
    let (lhs, rhs) = line.split_once('=')?;
    let value: f32 = lhs.trim().parse().ok()?;
    let label = rhs.trim().trim_matches('"').to_string();
    Some((value, label))
}

// ── Discovery ─────────────────────────────────────────────────────────────────

/// Run `lv2ls` and `lv2info` for each URI, returning the full catalog.
/// Plugins whose info fails to parse are skipped but logged.
pub fn scan_plugins() -> Result<Vec<Lv2Plugin>> {
    let out = Command::new("lv2ls").output()
        .map_err(|e| anyhow!("failed to run lv2ls: {e}. Is lilv installed?"))?;
    if !out.status.success() {
        return Err(anyhow!("lv2ls exited with {}", out.status));
    }
    let uris: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();

    // Run lv2info in parallel with a small worker pool to keep memory bounded.
    let workers = 8usize;
    let queue = Arc::new(Mutex::new(uris.into_iter()));
    let (tx, rx) = mpsc::channel::<Option<Lv2Plugin>>();
    let mut handles = Vec::with_capacity(workers);
    for _ in 0..workers {
        let queue = queue.clone();
        let tx = tx.clone();
        handles.push(std::thread::spawn(move || {
            loop {
                let next = { queue.lock().unwrap().next() };
                let Some(uri) = next else { break };
                let result = Command::new("lv2info").arg(&uri).output();
                let plugin = match result {
                    Ok(o) if o.status.success() => {
                        let text = String::from_utf8_lossy(&o.stdout);
                        match parse_lv2info(&text) {
                            Ok(p) => Some(p),
                            Err(e) => {
                                tracing::warn!("parse_lv2info failed for {uri}: {e}");
                                None
                            }
                        }
                    }
                    Ok(o) => {
                        tracing::warn!("lv2info {uri} exited {:?}", o.status);
                        None
                    }
                    Err(e) => {
                        tracing::warn!("lv2info {uri} spawn failed: {e}");
                        None
                    }
                };
                let _ = tx.send(plugin);
            }
        }));
    }
    drop(tx);

    let mut plugins: Vec<Lv2Plugin> = Vec::new();
    while let Ok(Some(p)) = rx.recv() {
        plugins.push(p);
    }
    for h in handles { let _ = h.join(); }

    plugins.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(plugins)
}

// ── Cache ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lv2Catalog {
    pub plugins: Vec<Lv2Plugin>,
}

impl Lv2Catalog {
    pub fn cache_path() -> PathBuf {
        dirs::cache_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("pipewire-control")
            .join("lv2-catalog.json")
    }

    pub fn load() -> Result<Self> {
        let path = Self::cache_path();
        let raw = std::fs::read_to_string(&path)?;
        Ok(serde_json::from_str(&raw)?)
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::cache_path();
        if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }
        std::fs::write(&path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Load from cache or fall back to a fresh scan (and persist it).
    pub fn load_or_scan() -> Result<Self> {
        match Self::load() {
            Ok(c) if !c.plugins.is_empty() => Ok(c),
            _ => Self::rescan(),
        }
    }

    pub fn rescan() -> Result<Self> {
        let plugins = scan_plugins()?;
        let cat = Self { plugins };
        let _ = cat.save();
        Ok(cat)
    }

    pub fn find(&self, uri: &str) -> Option<&Lv2Plugin> {
        self.plugins.iter().find(|p| p.uri == uri)
    }
}

/// Returns the conventional folder name for a plugin URI, matching
/// `scripts/dump-lv2.sh`'s sanitization (used by the web UI to look up
/// optional `static/lv2-ui/<sanitized>/index.js` overrides).
pub fn sanitize_uri(uri: &str) -> String {
    let mut out = String::with_capacity(uri.len());
    let mut prev_underscore = false;
    for c in uri.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            prev_underscore = false;
        } else if !prev_underscore {
            out.push('_');
            prev_underscore = true;
        }
    }
    out.trim_matches('_').to_string()
}

// Helpers used by conf_gen and the chain editor: pick the primary audio in/out
// port for a single-instance link. Preference order: designation `center` →
// `left` (mono pin) → first non-sidechain audio port in declaration order.
pub fn primary_audio_port(plugin: &Lv2Plugin, direction: PortDirection) -> Option<&Lv2Port> {
    let candidates: Vec<&Lv2Port> = plugin.ports.iter()
        .filter(|p| p.kind == PortKind::Audio && p.direction == direction && !p.flags.side_chain)
        .collect();
    if candidates.is_empty() { return None; }
    let by_designation = |sfx: &str| -> Option<&Lv2Port> {
        candidates.iter().copied().find(|p| {
            p.designation.as_deref().map(|d| d.ends_with(sfx)).unwrap_or(false)
        })
    };
    by_designation("#center")
        .or_else(|| by_designation("#left"))
        .or_else(|| candidates.first().copied())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_COMPRESSOR: &str = include_str!(
        "../../../lv2info/http_lsp_plug_in_plugins_lv2_compressor_mono.txt"
    );

    #[test]
    fn parses_compressor_header() {
        let p = parse_lv2info(SAMPLE_COMPRESSOR).expect("parse ok");
        assert_eq!(p.uri, "http://lsp-plug.in/plugins/lv2/compressor_mono");
        assert_eq!(p.name, "LSP Compressor Mono");
        assert_eq!(p.class, "Compressor Plugin");
        assert!(p.has_native_ui);
        assert!(!p.ports.is_empty());
    }

    #[test]
    fn parses_audio_in_out() {
        let p = parse_lv2info(SAMPLE_COMPRESSOR).unwrap();
        let audio_in = p.ports.iter().find(|x| x.symbol == "in").unwrap();
        assert_eq!(audio_in.kind, PortKind::Audio);
        assert_eq!(audio_in.direction, PortDirection::Input);
        let audio_out = p.ports.iter().find(|x| x.symbol == "out").unwrap();
        assert_eq!(audio_out.kind, PortKind::Audio);
        assert_eq!(audio_out.direction, PortDirection::Output);
    }

    #[test]
    fn parses_control_port_with_properties() {
        let p = parse_lv2info(SAMPLE_COMPRESSOR).unwrap();
        let in2lk = p.ports.iter().find(|x| x.symbol == "in2lk").unwrap();
        assert_eq!(in2lk.kind, PortKind::Control);
        assert!(in2lk.flags.logarithmic);
        assert!(in2lk.flags.strict_bounds);
        assert_eq!(in2lk.min, Some(0.0));
        assert!(in2lk.max.unwrap() > 9.9);
    }

    #[test]
    fn parses_designation_enabled() {
        let p = parse_lv2info(SAMPLE_COMPRESSOR).unwrap();
        let enabled = p.ports.iter().find(|x| x.symbol == "enabled").unwrap();
        assert_eq!(
            enabled.designation.as_deref(),
            Some("http://lv2plug.in/ns/lv2core#enabled")
        );
        assert!(enabled.flags.toggled);
    }

    #[test]
    fn parses_enumeration_with_scale_points() {
        // chorus_mono has a high-pass mode enumeration ("hpm") with 4 scale points.
        let text = std::fs::read_to_string(
            concat!(env!("CARGO_MANIFEST_DIR"),
                    "/../../lv2info/http_lsp_plug_in_plugins_lv2_chorus_mono.txt")
        ).unwrap();
        let p = parse_lv2info(&text).unwrap();
        let hpm = p.ports.iter().find(|x| x.symbol == "hpm")
            .expect("chorus_mono should expose hpm");
        assert!(hpm.flags.enumeration);
        assert!(hpm.flags.integer);
        assert_eq!(hpm.scale_points.len(), 4);
        assert!(hpm.scale_points.iter().any(|(v, l)| *v == 0.0 && l == "off"));
    }

    #[test]
    fn primary_port_prefers_center_designation() {
        let p = parse_lv2info(SAMPLE_COMPRESSOR).unwrap();
        let inp = primary_audio_port(&p, PortDirection::Input).unwrap();
        assert_eq!(inp.symbol, "in");
        let outp = primary_audio_port(&p, PortDirection::Output).unwrap();
        assert_eq!(outp.symbol, "out");
    }

    #[test]
    fn sanitize_matches_dump_script_format() {
        assert_eq!(
            sanitize_uri("http://lsp-plug.in/plugins/lv2/compressor_mono"),
            "http_lsp_plug_in_plugins_lv2_compressor_mono"
        );
    }
}
