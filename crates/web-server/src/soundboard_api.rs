//! HTTP handlers for the soundboard clip library.
//!
//! Step 1: ingest (upload + youtube), list/filter, metadata edit, delete.
//! Playback endpoints land in a later step once the engine wiring exists.

use axum::{
    extract::{Multipart, Path, Query, State},
    http::{header, StatusCode},
    response::IntoResponse,
    Json,
};
use pipewire_control_core::{
    model::EngineCmd,
    playback::{decode_to_stereo_f32, PlaybackInfo, PlaybackKnobs},
    soundboard::{
        self, build_clip, clip_path, ensure_sounds_dir, fetch_youtube_title, filter_clips,
        normalize_tags, slugify, tag_index, write_upload, SoundClip,
    },
};
use serde::Deserialize;
use std::collections::HashMap;

use crate::AppShared;

#[derive(Deserialize)]
pub struct ListQuery {
    /// Comma-separated tags — clips must have ALL of them.
    pub tags: Option<String>,
    /// Substring match on clip name (case-insensitive).
    pub q: Option<String>,
}

pub async fn list_sounds(
    Query(q): Query<ListQuery>,
    State(s): State<AppShared>,
) -> impl IntoResponse {
    let state = s.state.lock().unwrap();
    let required: Vec<String> = q
        .tags
        .as_deref()
        .map(|s| s.split(',').map(|t| t.trim().to_string()).collect())
        .unwrap_or_default();
    let filtered = filter_clips(&state.clips, &required, q.q.as_deref());
    let cloned: Vec<SoundClip> = filtered.into_iter().cloned().collect();
    Json(cloned)
}

pub async fn list_tags(State(s): State<AppShared>) -> impl IntoResponse {
    let state = s.state.lock().unwrap();
    let idx = tag_index(&state.clips);
    Json(
        idx.into_iter()
            .map(|(tag, count)| serde_json::json!({ "tag": tag, "count": count }))
            .collect::<Vec<_>>(),
    )
}

pub async fn get_sound(
    Path(id): Path<String>,
    State(s): State<AppShared>,
) -> impl IntoResponse {
    let state = s.state.lock().unwrap();
    match state.clips.iter().find(|c| c.id == id) {
        Some(c) => (StatusCode::OK, Json(Some(c.clone()))),
        None => (StatusCode::NOT_FOUND, Json(None)),
    }
}

/// Multipart upload. Fields:
/// - `file` (required): audio file bytes.
/// - `name` (optional): display name; defaults to original filename stem.
/// - `tags` (optional): comma-separated tags.
pub async fn upload_sound(
    State(s): State<AppShared>,
    mut mp: Multipart,
) -> impl IntoResponse {
    if let Err(e) = ensure_sounds_dir() {
        return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
    }

    let mut fields: HashMap<String, String> = HashMap::new();
    let mut file_bytes: Option<(String, Vec<u8>)> = None;

    loop {
        match mp.next_field().await {
            Ok(Some(field)) => {
                let name = field.name().unwrap_or("").to_string();
                if name == "file" {
                    let file_name = field
                        .file_name()
                        .unwrap_or("upload.bin")
                        .to_string();
                    match field.bytes().await {
                        Ok(b) => file_bytes = Some((file_name, b.to_vec())),
                        Err(e) => {
                            return (StatusCode::BAD_REQUEST, format!("file read: {e}")).into_response();
                        }
                    }
                } else {
                    match field.text().await {
                        Ok(v) => {
                            fields.insert(name, v);
                        }
                        Err(e) => {
                            return (StatusCode::BAD_REQUEST, format!("field read: {e}")).into_response();
                        }
                    }
                }
            }
            Ok(None) => break,
            Err(e) => return (StatusCode::BAD_REQUEST, format!("multipart: {e}")).into_response(),
        }
    }

    let Some((orig_name, bytes)) = file_bytes else {
        return (StatusCode::BAD_REQUEST, "missing 'file' field").into_response();
    };

    let display_name = fields
        .remove("name")
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| {
            std::path::Path::new(&orig_name)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("Clip")
                .to_string()
        });
    let tags: Vec<String> = fields
        .remove("tags")
        .map(|s| s.split(',').map(|t| t.to_string()).collect())
        .unwrap_or_default();
    let stem_hint = slugify(&display_name);

    let file_name = match write_upload(&orig_name, &bytes, Some(&stem_hint)) {
        Ok(n) => n,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };

    let clip = match build_clip(display_name, file_name, tags, None) {
        Ok(c) => c,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };

    let mut state = s.state.lock().unwrap();
    state.clips.push(clip.clone());
    let _ = state.save();
    (StatusCode::CREATED, Json(clip)).into_response()
}

#[derive(Deserialize)]
pub struct YoutubeBody {
    pub url: String,
    pub name: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

pub async fn ingest_youtube(
    State(s): State<AppShared>,
    Json(body): Json<YoutubeBody>,
) -> impl IntoResponse {
    if body.url.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "missing url").into_response();
    }

    let resolved_name = match body.name.clone().filter(|s| !s.trim().is_empty()) {
        Some(n) => n,
        None => fetch_youtube_title(&body.url)
            .await
            .unwrap_or_else(|| "YouTube Clip".to_string()),
    };
    let stem_hint = slugify(&resolved_name);

    let file_name = match soundboard::download_youtube(&body.url, Some(&stem_hint)).await {
        Ok(n) => n,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };

    let clip = match build_clip(resolved_name, file_name, body.tags, Some(body.url)) {
        Ok(c) => c,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };

    let mut state = s.state.lock().unwrap();
    state.clips.push(clip.clone());
    let _ = state.save();
    (StatusCode::CREATED, Json(clip)).into_response()
}

#[derive(Deserialize)]
pub struct UpdateSoundBody {
    pub name: Option<String>,
    pub tags: Option<Vec<String>>,
    pub default_volume: Option<f32>,
    pub default_loop: Option<bool>,
}

pub async fn update_sound(
    Path(id): Path<String>,
    State(s): State<AppShared>,
    Json(body): Json<UpdateSoundBody>,
) -> impl IntoResponse {
    let mut state = s.state.lock().unwrap();
    let Some(clip) = state.clips.iter_mut().find(|c| c.id == id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(name) = body.name {
        clip.name = name;
    }
    if let Some(tags) = body.tags {
        clip.tags = normalize_tags(tags);
    }
    if let Some(v) = body.default_volume {
        clip.default_volume = v.clamp(0.0, 2.0);
    }
    if let Some(l) = body.default_loop {
        clip.default_loop = l;
    }
    let updated = clip.clone();
    let _ = state.save();
    (StatusCode::OK, Json(updated)).into_response()
}

pub async fn delete_sound(
    Path(id): Path<String>,
    State(s): State<AppShared>,
) -> impl IntoResponse {
    let mut state = s.state.lock().unwrap();
    let Some(pos) = state.clips.iter().position(|c| c.id == id) else {
        return StatusCode::NOT_FOUND;
    };
    let clip = state.clips.remove(pos);
    let _ = std::fs::remove_file(clip_path(&clip.file_name));
    let _ = state.save();
    StatusCode::NO_CONTENT
}

/// Stream the clip's audio bytes back so the UI can preview it with a plain
/// `<audio>` element.
pub async fn serve_sound_file(
    Path(id): Path<String>,
    State(s): State<AppShared>,
) -> impl IntoResponse {
    let file_name = {
        let state = s.state.lock().unwrap();
        match state.clips.iter().find(|c| c.id == id) {
            Some(c) => c.file_name.clone(),
            None => return (StatusCode::NOT_FOUND, "clip not found").into_response(),
        }
    };
    let path = clip_path(&file_name);
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let mime = mime_for(&file_name);
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, mime)],
                bytes,
            )
                .into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

// ── Playback ─────────────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
pub struct PlayBody {
    /// Sink node_name (e.g. `alsa_output...` or `pw-ctrl.preset.foo`).
    /// If absent, the default sink is used.
    pub target_node_name: Option<String>,
    pub volume: Option<f32>,
    pub loop_mode: Option<bool>,
}

pub async fn play_sound(
    Path(id): Path<String>,
    State(s): State<AppShared>,
    body: Option<Json<PlayBody>>,
) -> impl IntoResponse {
    let body = body.map(|b| b.0).unwrap_or_default();

    let clip = {
        let state = s.state.lock().unwrap();
        match state.clips.iter().find(|c| c.id == id) {
            Some(c) => c.clone(),
            None => return (StatusCode::NOT_FOUND, "clip not found").into_response(),
        }
    };

    let path = clip_path(&clip.file_name);
    let decoded = match tokio::task::spawn_blocking(move || decode_to_stereo_f32(&path)).await {
        Ok(Ok(d)) => d,
        Ok(Err(e)) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("decode: {e}")).into_response();
        }
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("join: {e}")).into_response();
        }
    };

    let volume = body.volume.unwrap_or(clip.default_volume).clamp(0.0, 2.0);
    let loop_mode = body.loop_mode.unwrap_or(clip.default_loop);
    let knobs = PlaybackKnobs::new(volume, loop_mode);
    let playback_id = uuid::Uuid::new_v4().simple().to_string();
    let duration_ms = decoded.duration_ms();
    let target = body.target_node_name.clone();

    let entry = crate::PlaybackEntry {
        clip_id: clip.id.clone(),
        target_node_name: target.clone(),
        duration_ms,
        started_at: now_unix(),
        knobs: knobs.clone(),
    };
    s.playbacks
        .lock()
        .unwrap()
        .insert(playback_id.clone(), entry);

    if let Err(_e) = s.engine.cmd_tx.send(EngineCmd::PlayClip {
        playback_id: playback_id.clone(),
        clip_id: clip.id,
        samples: decoded.samples,
        sample_rate: decoded.sample_rate,
        target_node_name: target,
        knobs,
    }) {
        s.playbacks.lock().unwrap().remove(&playback_id);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "engine cmd send failed".to_string(),
        )
            .into_response();
    }

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "playback_id": playback_id,
            "duration_ms": duration_ms,
        })),
    )
        .into_response()
}

pub async fn list_playbacks(State(s): State<AppShared>) -> impl IntoResponse {
    let pbs = s.playbacks.lock().unwrap();
    let infos: Vec<PlaybackInfo> = pbs
        .iter()
        .map(|(pid, entry)| entry.to_info(pid))
        .collect();
    Json(infos)
}

pub async fn stop_playback(
    Path(id): Path<String>,
    State(s): State<AppShared>,
) -> impl IntoResponse {
    {
        let pbs = s.playbacks.lock().unwrap();
        let Some(entry) = pbs.get(&id) else {
            return StatusCode::NOT_FOUND.into_response();
        };
        // Set the RT-side flag so the next process tick zero-fills and tears down.
        entry.knobs.request_stop();
    }
    // Also send a Stop cmd as a belt-and-suspenders for streams not currently
    // ticking (e.g. paused before any process).
    let _ = s
        .engine
        .cmd_tx
        .send(EngineCmd::StopPlayback { playback_id: id });
    StatusCode::OK.into_response()
}

#[derive(Deserialize)]
pub struct VolumeBody {
    pub volume: f32,
}

pub async fn set_playback_volume(
    Path(id): Path<String>,
    State(s): State<AppShared>,
    Json(body): Json<VolumeBody>,
) -> impl IntoResponse {
    let pbs = s.playbacks.lock().unwrap();
    let Some(entry) = pbs.get(&id) else {
        return StatusCode::NOT_FOUND;
    };
    entry.knobs.set_volume(body.volume.clamp(0.0, 2.0));
    StatusCode::OK
}

#[derive(Deserialize)]
pub struct LoopBody {
    #[serde(rename = "loop")]
    pub loop_mode: bool,
}

pub async fn set_playback_loop(
    Path(id): Path<String>,
    State(s): State<AppShared>,
    Json(body): Json<LoopBody>,
) -> impl IntoResponse {
    let pbs = s.playbacks.lock().unwrap();
    let Some(entry) = pbs.get(&id) else {
        return StatusCode::NOT_FOUND;
    };
    entry.knobs.set_loop(body.loop_mode);
    StatusCode::OK
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn mime_for(file_name: &str) -> &'static str {
    match std::path::Path::new(file_name)
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
        .as_deref()
    {
        Some("opus") | Some("ogg") => "audio/ogg",
        Some("mp3") => "audio/mpeg",
        Some("wav") => "audio/wav",
        Some("flac") => "audio/flac",
        Some("m4a") | Some("aac") => "audio/aac",
        _ => "application/octet-stream",
    }
}
