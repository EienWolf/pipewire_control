//! Soundboard: clip library with metadata, tags, duration probing, and YouTube ingest.
//!
//! Step 1 scope: pure data model + filesystem-backed library + decode probing +
//! yt-dlp ingest. No PipeWire playback yet — that lands in a later step.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use tokio::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SoundClip {
    pub id: String,
    pub name: String,
    /// Filename inside the sounds dir (not absolute, so the library is portable).
    pub file_name: String,
    /// Duration in milliseconds, probed at ingest.
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default = "default_volume")]
    pub default_volume: f32,
    #[serde(default)]
    pub default_loop: bool,
    /// Optional source URL (e.g. the YouTube link the clip was downloaded from).
    #[serde(default)]
    pub source_url: Option<String>,
    /// Unix timestamp (seconds) of when the clip was added.
    #[serde(default)]
    pub added_at: u64,
}

fn default_volume() -> f32 {
    1.0
}

/// Returns `~/.config/pipewire-control/sounds/`. Created lazily on first ingest.
pub fn sounds_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("pipewire-control")
        .join("sounds")
}

pub fn ensure_sounds_dir() -> Result<PathBuf> {
    let dir = sounds_dir();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating sounds dir {}", dir.display()))?;
    Ok(dir)
}

pub fn clip_path(file_name: &str) -> PathBuf {
    sounds_dir().join(file_name)
}

/// Probe an audio file for its duration in milliseconds using symphonia.
/// Falls back to 0 if the format reports no frame count and no sample rate
/// (rare, but possible for streamed containers).
pub fn probe_duration_ms(path: &Path) -> Result<u64> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("opening {} for probe", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .with_context(|| format!("probing {}", path.display()))?;

    let track = probed
        .format
        .default_track()
        .ok_or_else(|| anyhow!("no default track in {}", path.display()))?;

    let params = &track.codec_params;
    let sample_rate = params.sample_rate.unwrap_or(0) as u64;
    let n_frames = params.n_frames.unwrap_or(0);

    if sample_rate == 0 || n_frames == 0 {
        // Some formats only expose duration via time_base + n_frames in TimeBase units.
        if let (Some(tb), Some(frames)) = (params.time_base, params.n_frames) {
            let secs = frames as f64 * tb.numer as f64 / tb.denom as f64;
            return Ok((secs * 1000.0) as u64);
        }
        return Ok(0);
    }
    Ok(n_frames * 1000 / sample_rate)
}

/// Slugify a name for use as a filename stem (keeps ASCII alnum, dashes, underscores).
pub fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_dash = false;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if matches!(ch, '-' | '_') {
            out.push(ch);
            prev_dash = false;
        } else if ch.is_whitespace() || ch == '.' {
            if !prev_dash && !out.is_empty() {
                out.push('-');
                prev_dash = true;
            }
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        out.push_str("clip");
    }
    out
}

/// Pick a unique filename inside the sounds dir given a desired stem and extension.
pub fn unique_file_name(stem: &str, ext: &str) -> Result<String> {
    let dir = ensure_sounds_dir()?;
    let mut candidate = format!("{stem}.{ext}");
    let mut n = 1u32;
    while dir.join(&candidate).exists() {
        n += 1;
        candidate = format!("{stem}-{n}.{ext}");
    }
    Ok(candidate)
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Build a `SoundClip` from a file already present in the sounds dir.
pub fn build_clip(
    name: String,
    file_name: String,
    tags: Vec<String>,
    source_url: Option<String>,
) -> Result<SoundClip> {
    let path = clip_path(&file_name);
    let duration_ms = probe_duration_ms(&path).unwrap_or(0);
    Ok(SoundClip {
        id: uuid::Uuid::new_v4().to_string(),
        name,
        file_name,
        duration_ms,
        tags: normalize_tags(tags),
        default_volume: 1.0,
        default_loop: false,
        source_url,
        added_at: now_unix(),
    })
}

pub fn normalize_tags(tags: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = tags
        .into_iter()
        .map(|t| t.trim().to_lowercase())
        .filter(|t| !t.is_empty())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Write raw bytes (e.g. from an HTTP multipart upload) into the sounds dir
/// and return the resulting filename. The extension is preserved from `original_name`.
pub fn write_upload(original_name: &str, bytes: &[u8], desired_stem: Option<&str>) -> Result<String> {
    let ext = Path::new(original_name)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("bin")
        .to_lowercase();
    let stem = desired_stem
        .map(slugify)
        .unwrap_or_else(|| {
            let raw_stem = Path::new(original_name)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("clip");
            slugify(raw_stem)
        });
    let file_name = unique_file_name(&stem, &ext)?;
    let path = clip_path(&file_name);
    std::fs::write(&path, bytes)
        .with_context(|| format!("writing upload to {}", path.display()))?;
    Ok(file_name)
}

/// Download a YouTube (or any yt-dlp-supported) URL into the sounds dir.
/// Extracts audio as Vorbis inside an ogg container — symphonia 0.5 does NOT
/// ship an Opus codec, so we explicitly avoid opus here.
///
/// Returns the resulting filename (relative to sounds dir).
pub async fn download_youtube(url: &str, desired_stem: Option<&str>) -> Result<String> {
    let dir = ensure_sounds_dir()?;
    let stem_hint = desired_stem.map(slugify);

    // We don't know the final filename until yt-dlp picks the title. Use a
    // unique tmp prefix, then rename predictably afterwards.
    let tmp_id = uuid::Uuid::new_v4().simple().to_string();
    let tmp_template = dir.join(format!("__dl_{tmp_id}.%(ext)s"));

    let mut cmd = Command::new("yt-dlp");
    cmd.arg("--no-playlist")
        .arg("-x")
        .arg("--audio-format")
        .arg("vorbis")
        .arg("-o")
        .arg(&tmp_template)
        .arg("--print")
        .arg("after_move:filepath")
        .arg(url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let output = cmd
        .output()
        .await
        .context("spawning yt-dlp (is it installed and on PATH?)")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("yt-dlp failed: {stderr}"));
    }

    let printed = String::from_utf8_lossy(&output.stdout);
    let downloaded_path = printed
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .last()
        .ok_or_else(|| anyhow!("yt-dlp produced no filepath output"))?;
    let downloaded = PathBuf::from(downloaded_path);
    if !downloaded.exists() {
        return Err(anyhow!(
            "yt-dlp reported {} but file is missing",
            downloaded.display()
        ));
    }

    let ext = downloaded
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("opus")
        .to_string();
    let stem = stem_hint.unwrap_or_else(|| {
        let raw = downloaded
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("yt-clip");
        let raw = raw.strip_prefix("__dl_").unwrap_or(raw);
        if raw.len() == 32 && raw.chars().all(|c| c.is_ascii_hexdigit()) {
            "yt-clip".to_string()
        } else {
            slugify(raw)
        }
    });

    let final_name = unique_file_name(&stem, &ext)?;
    let final_path = clip_path(&final_name);
    std::fs::rename(&downloaded, &final_path)
        .with_context(|| format!("renaming {} → {}", downloaded.display(), final_path.display()))?;
    Ok(final_name)
}

/// Try to fetch a clean title for a YouTube URL via yt-dlp (no download).
/// Returns None on any error so callers can fall back to a generic name.
pub async fn fetch_youtube_title(url: &str) -> Option<String> {
    let output = Command::new("yt-dlp")
        .arg("--no-playlist")
        .arg("--print")
        .arg("title")
        .arg("--skip-download")
        .arg(url)
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let title = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if title.is_empty() {
        None
    } else {
        Some(title)
    }
}

/// Filter clips by an optional set of required tags (AND semantics) and an
/// optional name substring (case-insensitive).
pub fn filter_clips<'a>(
    clips: &'a [SoundClip],
    require_tags: &[String],
    name_query: Option<&str>,
) -> Vec<&'a SoundClip> {
    let req: Vec<String> = require_tags
        .iter()
        .map(|t| t.trim().to_lowercase())
        .filter(|t| !t.is_empty())
        .collect();
    let q = name_query.map(|s| s.to_lowercase());
    clips
        .iter()
        .filter(|c| req.iter().all(|t| c.tags.iter().any(|ct| ct == t)))
        .filter(|c| match &q {
            Some(q) => c.name.to_lowercase().contains(q),
            None => true,
        })
        .collect()
}

/// Aggregate every tag across the library with usage counts (sorted desc).
pub fn tag_index(clips: &[SoundClip]) -> Vec<(String, usize)> {
    use std::collections::HashMap;
    let mut counts: HashMap<String, usize> = HashMap::new();
    for c in clips {
        for t in &c.tags {
            *counts.entry(t.clone()).or_insert(0) += 1;
        }
    }
    let mut v: Vec<_> = counts.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v
}
