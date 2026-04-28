use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{delete, get, post, put},
    Router,
};
use pipewire_control_core::{
    conf_gen,
    ladspa::{self as ladspa_mod, LadspaCatalog, LadspaPlugin},
    lv2::{self, Lv2Catalog, Lv2Plugin},
    model::PwEvent,
    playback::{PlaybackInfo, PlaybackKnobs},
    preset::{make_id, ChainNode, ChainNodeKind, ChannelLayout, OutputAssignment, Preset},
    pw_engine::PwEngine,
    state::AppState,
    virtual_mic::{make_mic_id, MicInput, VirtualMic},
};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tower_http::{cors::CorsLayer, services::ServeDir};

mod soundboard_api;

// ── Shared state ──────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct AppShared {
    pub(crate) engine: PwEngine,
    pub(crate) state: Arc<Mutex<AppState>>,
    pub(crate) lv2: Arc<Mutex<Lv2Catalog>>,
    pub(crate) ladspa: Arc<Mutex<LadspaCatalog>>,
    pub(crate) static_dir: Arc<std::path::PathBuf>,
    pub(crate) playbacks: Arc<Mutex<HashMap<String, PlaybackEntry>>>,
}

pub struct PlaybackEntry {
    pub clip_id: String,
    pub target_node_name: Option<String>,
    pub duration_ms: u64,
    pub started_at: u64,
    pub knobs: Arc<PlaybackKnobs>,
}

impl PlaybackEntry {
    pub fn to_info(&self, playback_id: &str) -> PlaybackInfo {
        PlaybackInfo {
            playback_id: playback_id.to_string(),
            clip_id: self.clip_id.clone(),
            target_node_name: self.target_node_name.clone(),
            volume: self.knobs.volume(),
            loop_mode: self.knobs.looped(),
            duration_ms: self.duration_ms,
            started_at: self.started_at,
        }
    }
}

// ── REST DTOs ─────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct CreatePresetBody {
    name: String,
    #[serde(default)]
    outputs: Vec<OutputAssignment>,
}

#[derive(Deserialize)]
struct UpdatePresetBody {
    name: Option<String>,
    channels: Option<ChannelLayout>,
}

#[derive(Deserialize)]
struct AddOutputBody {
    node_name: String,
    volume: f32,
}

#[derive(Deserialize)]
struct UpdateVolumeBody {
    volume: f32,
}

#[derive(Deserialize)]
struct CreateVirtualMicBody {
    name: String,
}

#[derive(Deserialize)]
struct UpdateVirtualMicBody {
    name: Option<String>,
}

#[derive(Deserialize)]
struct AddMicInputBody {
    node_name: String,
    gain: f32,
}

#[derive(Deserialize)]
struct UpdateInputGainBody {
    gain: f32,
}

// ── main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let engine = PwEngine::start();
    let app_state = Arc::new(Mutex::new(AppState::load().unwrap_or_default()));

    let static_dir = Arc::new(
        std::env::current_dir().unwrap().join("crates/web-server/static")
    );

    // Load (or scan) the LV2 catalog in the background so startup isn't delayed.
    let lv2 = Arc::new(Mutex::new(Lv2Catalog::load().unwrap_or(Lv2Catalog { plugins: vec![] })));
    {
        let lv2 = lv2.clone();
        std::thread::spawn(move || {
            let needs_scan = lv2.lock().unwrap().plugins.is_empty();
            if !needs_scan { return; }
            match Lv2Catalog::rescan() {
                Ok(cat) => {
                    tracing::info!("LV2 catalog: {} plugins", cat.plugins.len());
                    *lv2.lock().unwrap() = cat;
                }
                Err(e) => tracing::warn!("LV2 scan failed: {e}"),
            }
        });
    }

    // LADSPA catalog: same lazy pattern as LV2.
    let ladspa = Arc::new(Mutex::new(LadspaCatalog::load().unwrap_or(LadspaCatalog { plugins: vec![] })));
    {
        let ladspa = ladspa.clone();
        std::thread::spawn(move || {
            let needs_scan = ladspa.lock().unwrap().plugins.is_empty();
            if !needs_scan { return; }
            match LadspaCatalog::rescan() {
                Ok(cat) => {
                    tracing::info!("LADSPA catalog: {} plugins", cat.plugins.len());
                    *ladspa.lock().unwrap() = cat;
                }
                Err(e) => tracing::warn!("LADSPA scan failed: {e}"),
            }
        });
    }

    let playbacks: Arc<Mutex<HashMap<String, PlaybackEntry>>> = Arc::new(Mutex::new(HashMap::new()));

    // Listen for PlaybackEnded events and remove entries from the map.
    {
        let pbs = playbacks.clone();
        let mut rx = engine.subscribe();
        tokio::spawn(async move {
            while let Ok(ev) = rx.recv().await {
                if let PwEvent::PlaybackEnded { playback_id } = ev {
                    pbs.lock().unwrap().remove(&playback_id);
                }
            }
        });
    }

    let shared = AppShared {
        engine,
        state: app_state,
        lv2,
        ladspa,
        static_dir: static_dir.clone(),
        playbacks,
    };

    let app = Router::new()
        .route("/health", get(health))
        .route("/nodes", get(list_nodes))
        // Presets
        .route("/presets", get(list_presets).post(create_preset))
        .route("/presets/{id}", get(get_preset).put(update_preset).delete(delete_preset))
        .route("/presets/{id}/outputs", post(add_output))
        .route("/presets/{id}/outputs/{idx}", delete(remove_output).put(update_output_volume))
        .route("/presets/{id}/source-inputs", post(add_source_input))
        .route("/presets/{id}/source-inputs/{idx}", delete(remove_source_input))
        // Effect chain
        .route("/presets/{id}/chain", post(add_chain_node))
        .route("/presets/{id}/chain/{idx}", delete(remove_chain_node))
        .route("/presets/{id}/chain/{idx}/move", put(move_chain_node))
        .route("/presets/{id}/chain/{idx}/control", put(set_chain_control))
        .route("/presets/{id}/chain/{idx}/bypass", put(set_chain_bypass))
        .route("/presets/{id}/chain/{idx}/label", put(set_chain_label))
        // Virtual mics
        .route("/virtual-mics", get(list_virtual_mics).post(create_virtual_mic))
        .route("/virtual-mics/{id}", get(get_virtual_mic).put(update_virtual_mic).delete(delete_virtual_mic))
        .route("/virtual-mics/{id}/remap", post(remap_virtual_mic))
        .route("/virtual-mics/{id}/inputs", post(add_mic_input))
        .route("/virtual-mics/{id}/inputs/{idx}", delete(remove_mic_input).put(update_mic_input_gain))
        // Stream routing
        .route("/presets/{id}/route/{node_id}", post(route_stream))
        .route("/presets/{id}/unroute/{node_id}", post(unroute_stream))
        // Node volume/mute control (proxied to wpctl)
        .route("/nodes/{id}/volume", post(set_node_volume))
        .route("/nodes/{id}/mute", post(toggle_node_mute))
        // Config generation
        .route("/config/preview", post(config_preview))
        .route("/config/apply", post(config_apply))
        // WebSocket
        .route("/ws", get(ws_handler))
        // Soundboard
        .route("/sounds", get(soundboard_api::list_sounds).post(soundboard_api::upload_sound))
        .route("/sounds/tags", get(soundboard_api::list_tags))
        .route("/sounds/youtube", post(soundboard_api::ingest_youtube))
        .route("/sounds/{id}", get(soundboard_api::get_sound)
            .put(soundboard_api::update_sound)
            .delete(soundboard_api::delete_sound))
        .route("/sounds/{id}/file", get(soundboard_api::serve_sound_file))
        .route("/sounds/{id}/play", post(soundboard_api::play_sound))
        .route("/playbacks", get(soundboard_api::list_playbacks))
        .route("/playbacks/{id}/stop", post(soundboard_api::stop_playback))
        .route("/playbacks/{id}/volume", put(soundboard_api::set_playback_volume))
        .route("/playbacks/{id}/loop", put(soundboard_api::set_playback_loop))
        // LV2 catalog
        .route("/lv2/plugins", get(list_lv2_plugins))
        .route("/lv2/plugins/{uri}", get(get_lv2_plugin))
        .route("/lv2/rescan", post(rescan_lv2))
        .route("/lv2/ui/{uri}", get(check_lv2_ui))
        // LADSPA catalog
        .route("/ladspa/plugins", get(list_ladspa_plugins))
        .route("/ladspa/plugins/{key}", get(get_ladspa_plugin))
        .route("/ladspa/rescan", post(rescan_ladspa))
        .layer(CorsLayer::permissive())
        .with_state(shared)
        .fallback_service(ServeDir::new(static_dir.as_path()));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:7878").await.unwrap();
    tracing::info!("listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, app).await.unwrap();
}

// ── Basic handlers ────────────────────────────────────────────────────────────

async fn health() -> &'static str { "ok" }

async fn list_nodes(State(s): State<AppShared>) -> impl IntoResponse {
    let mut nodes = s.engine.nodes.read().unwrap().values().cloned().collect::<Vec<_>>();
    nodes.sort_by_key(|n| n.id);
    (StatusCode::OK, Json(nodes))
}

// ── Node volume/mute handlers ─────────────────────────────────────────────────

#[derive(Deserialize)]
struct SetVolumeBody {
    volume: f32,
}

async fn set_node_volume(
    Path(id): Path<u32>,
    Json(body): Json<SetVolumeBody>,
) -> impl IntoResponse {
    let vol = format!("{:.4}", body.volume.clamp(0.0, 1.5));
    match tokio::process::Command::new("wpctl")
        .args(["set-volume", &id.to_string(), &vol])
        .status().await
    {
        Ok(s) if s.success() => StatusCode::OK.into_response(),
        Ok(s) => (StatusCode::INTERNAL_SERVER_ERROR, format!("wpctl exited {s}")).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn toggle_node_mute(Path(id): Path<u32>) -> impl IntoResponse {
    match tokio::process::Command::new("wpctl")
        .args(["set-mute", &id.to_string(), "toggle"])
        .status().await
    {
        Ok(s) if s.success() => StatusCode::OK.into_response(),
        Ok(s) => (StatusCode::INTERNAL_SERVER_ERROR, format!("wpctl exited {s}")).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

// ── Preset handlers ───────────────────────────────────────────────────────────

async fn list_presets(State(s): State<AppShared>) -> impl IntoResponse {
    let presets = s.state.lock().unwrap().presets.clone();
    Json(presets)
}

async fn get_preset(
    Path(id): Path<String>,
    State(s): State<AppShared>,
) -> impl IntoResponse {
    let state = s.state.lock().unwrap();
    match state.presets.iter().find(|p| p.id == id) {
        Some(p) => (StatusCode::OK, Json(Some(p.clone()))),
        None => (StatusCode::NOT_FOUND, Json(None)),
    }
}

async fn create_preset(
    State(s): State<AppShared>,
    Json(body): Json<CreatePresetBody>,
) -> impl IntoResponse {
    let id = make_id(&body.name);
    {
        let state = s.state.lock().unwrap();
        if state.presets.iter().any(|p| p.id == id) {
            return (StatusCode::CONFLICT, Json(None::<Preset>));
        }
    }
    let mut preset = Preset::new(&body.name);
    preset.outputs = body.outputs;

    let mut state = s.state.lock().unwrap();
    state.presets.push(preset.clone());
    let _ = state.save();
    (StatusCode::CREATED, Json(Some(preset)))
}

async fn update_preset(
    Path(id): Path<String>,
    State(s): State<AppShared>,
    Json(body): Json<UpdatePresetBody>,
) -> impl IntoResponse {
    let mut state = s.state.lock().unwrap();
    let Some(preset) = state.presets.iter_mut().find(|p| p.id == id) else {
        return StatusCode::NOT_FOUND;
    };
    if let Some(name) = body.name { preset.name = name; }
    if let Some(ch) = body.channels { preset.channels = ch; }
    let _ = state.save();
    StatusCode::OK
}

async fn delete_preset(
    Path(id): Path<String>,
    State(s): State<AppShared>,
) -> impl IntoResponse {
    let mut state = s.state.lock().unwrap();
    state.presets.retain(|p| p.id != id);
    let _ = state.save();
    StatusCode::NO_CONTENT
}

async fn add_output(
    Path(id): Path<String>,
    State(s): State<AppShared>,
    Json(body): Json<AddOutputBody>,
) -> impl IntoResponse {
    let mut state = s.state.lock().unwrap();
    let Some(preset) = state.presets.iter_mut().find(|p| p.id == id) else {
        return StatusCode::NOT_FOUND;
    };
    preset.outputs.push(OutputAssignment { node_name: body.node_name, volume: body.volume });
    let _ = state.save();
    StatusCode::OK
}

async fn remove_output(
    Path((id, idx)): Path<(String, usize)>,
    State(s): State<AppShared>,
) -> impl IntoResponse {
    let mut state = s.state.lock().unwrap();
    let Some(preset) = state.presets.iter_mut().find(|p| p.id == id) else {
        return StatusCode::NOT_FOUND;
    };
    if idx >= preset.outputs.len() { return StatusCode::NOT_FOUND; }
    preset.outputs.remove(idx);
    let _ = state.save();
    StatusCode::OK
}

async fn update_output_volume(
    Path((id, idx)): Path<(String, usize)>,
    State(s): State<AppShared>,
    Json(body): Json<UpdateVolumeBody>,
) -> impl IntoResponse {
    let mut state = s.state.lock().unwrap();
    let Some(preset) = state.presets.iter_mut().find(|p| p.id == id) else {
        return StatusCode::NOT_FOUND;
    };
    if idx >= preset.outputs.len() { return StatusCode::NOT_FOUND; }
    preset.outputs[idx].volume = body.volume;
    let _ = state.save();
    StatusCode::OK
}

// ── Source inputs ─────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct AddSourceInputBody {
    node_name: String,
}

async fn add_source_input(
    Path(id): Path<String>,
    State(s): State<AppShared>,
    Json(body): Json<AddSourceInputBody>,
) -> impl IntoResponse {
    let mut state = s.state.lock().unwrap();
    let Some(preset) = state.presets.iter_mut().find(|p| p.id == id) else {
        return StatusCode::NOT_FOUND;
    };
    if preset.source_inputs.contains(&body.node_name) {
        return StatusCode::CONFLICT;
    }
    preset.source_inputs.push(body.node_name);
    let _ = state.save();
    StatusCode::OK
}

async fn remove_source_input(
    Path((id, idx)): Path<(String, usize)>,
    State(s): State<AppShared>,
) -> impl IntoResponse {
    let mut state = s.state.lock().unwrap();
    let Some(preset) = state.presets.iter_mut().find(|p| p.id == id) else {
        return StatusCode::NOT_FOUND;
    };
    if idx >= preset.source_inputs.len() { return StatusCode::NOT_FOUND; }
    preset.source_inputs.remove(idx);
    let _ = state.save();
    StatusCode::OK
}

// ── Stream routing ────────────────────────────────────────────────────────────

/// Route a stream node to a preset sink using pw-metadata (immediate) and persists to WirePlumber config.
async fn route_stream(
    Path((id, node_id)): Path<(String, u32)>,
    State(s): State<AppShared>,
) -> impl IntoResponse {
    let (preset_node, node_name, presets, routes) = {
        let mut state = s.state.lock().unwrap();
        let Some(preset) = state.presets.iter().find(|p| p.id == id) else {
            return (StatusCode::NOT_FOUND, "preset not found".to_string()).into_response();
        };
        let preset_node = format!("pw-ctrl.preset.{}", preset.id);
        let node_name = s.engine.nodes.read().unwrap()
            .get(&node_id).and_then(|n| n.node_name.clone());
        if let Some(ref name) = node_name {
            state.stream_routes.insert(name.clone(), id.clone());
            let _ = state.save();
        }
        (preset_node, node_name, state.presets.clone(), state.stream_routes.clone())
    };
    if node_name.is_some() {
        let _ = conf_gen::write_wp_routing(&presets, &routes);
    }
    match tokio::process::Command::new("pw-metadata")
        .args([node_id.to_string().as_str(), "target.object", preset_node.as_str()])
        .status().await
    {
        Ok(s) if s.success() => StatusCode::OK.into_response(),
        Ok(s) => (StatusCode::INTERNAL_SERVER_ERROR, format!("pw-metadata exited {s}")).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// Remove a routing override — stream goes back to WirePlumber default. Also removes the WP rule.
async fn unroute_stream(
    Path((_id, node_id)): Path<(String, u32)>,
    State(s): State<AppShared>,
) -> impl IntoResponse {
    let (presets, routes) = {
        let mut state = s.state.lock().unwrap();
        let node_name = s.engine.nodes.read().unwrap()
            .get(&node_id).and_then(|n| n.node_name.clone());
        if let Some(ref name) = node_name {
            state.stream_routes.remove(name);
            let _ = state.save();
        }
        (state.presets.clone(), state.stream_routes.clone())
    };
    let _ = conf_gen::write_wp_routing(&presets, &routes);
    match tokio::process::Command::new("pw-metadata")
        .args([node_id.to_string().as_str(), "target.object", ""])
        .status().await
    {
        Ok(s) if s.success() => StatusCode::OK.into_response(),
        Ok(s) => (StatusCode::INTERNAL_SERVER_ERROR, format!("pw-metadata exited {s}")).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

// ── Config generation ─────────────────────────────────────────────────────────

/// Generate preview config files in /tmp/pwctl/ and return their contents.
async fn config_preview(State(s): State<AppShared>) -> impl IntoResponse {
    let state = s.state.lock().unwrap();
    let lv2 = s.lv2.lock().unwrap();
    let ladspa = s.ladspa.lock().unwrap();
    match conf_gen::write_preview(&state.presets, &state.virtual_mics, &state.stream_routes, &lv2, &ladspa) {
        Ok((pw, wp)) => Json(serde_json::json!({
            "pipewire_conf": pw,
            "wireplumber_conf": wp,
            "pipewire_path": conf_gen::PREVIEW_PW,
            "wireplumber_path": conf_gen::PREVIEW_WP,
        })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// Copy preview files to real config dirs and restart PipeWire.
async fn config_apply(State(s): State<AppShared>) -> impl IntoResponse {
    // Regenerate to ensure files are current before applying.
    {
        let state = s.state.lock().unwrap();
        let lv2 = s.lv2.lock().unwrap();
        let ladspa = s.ladspa.lock().unwrap();
        if let Err(e) = conf_gen::write_preview(&state.presets, &state.virtual_mics, &state.stream_routes, &lv2, &ladspa) {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    }

    if let Err(e) = conf_gen::apply_preview() {
        return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
    }

    // Restart PipeWire and WirePlumber.
    let restart = tokio::process::Command::new("systemctl")
        .args(["--user", "restart", "pipewire", "wireplumber"])
        .status()
        .await;

    match restart {
        Ok(status) if status.success() => StatusCode::OK.into_response(),
        Ok(status) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("systemctl exited with {status}"),
        ).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

// ── Virtual mic handlers ──────────────────────────────────────────────────────

async fn list_virtual_mics(State(s): State<AppShared>) -> impl IntoResponse {
    Json(s.state.lock().unwrap().virtual_mics.clone())
}

async fn get_virtual_mic(
    Path(id): Path<String>,
    State(s): State<AppShared>,
) -> impl IntoResponse {
    let state = s.state.lock().unwrap();
    match state.virtual_mics.iter().find(|m| m.id == id) {
        Some(m) => (StatusCode::OK, Json(Some(m.clone()))),
        None    => (StatusCode::NOT_FOUND, Json(None)),
    }
}

async fn create_virtual_mic(
    State(s): State<AppShared>,
    Json(body): Json<CreateVirtualMicBody>,
) -> impl IntoResponse {
    let id = make_mic_id(&body.name);
    {
        let state = s.state.lock().unwrap();
        if state.virtual_mics.iter().any(|m| m.id == id) {
            return (StatusCode::CONFLICT, Json(None::<VirtualMic>));
        }
    }
    let mic = VirtualMic::new(&body.name);
    let mut state = s.state.lock().unwrap();
    state.virtual_mics.push(mic.clone());
    let _ = state.save();
    (StatusCode::CREATED, Json(Some(mic)))
}

async fn update_virtual_mic(
    Path(id): Path<String>,
    State(s): State<AppShared>,
    Json(body): Json<UpdateVirtualMicBody>,
) -> impl IntoResponse {
    let mut state = s.state.lock().unwrap();
    let Some(mic) = state.virtual_mics.iter_mut().find(|m| m.id == id) else {
        return StatusCode::NOT_FOUND;
    };
    if let Some(name) = body.name { mic.name = name; }
    let _ = state.save();
    StatusCode::OK
}

/// Recompute the vmic id from its current name and rewrite every reference
/// (preset outputs that point at `pw-ctrl.vmic.{old}.mix`). External apps that
/// were connected to the old node name will lose their connection — expected,
/// since the underlying PipeWire node ceases to exist on next /config/apply.
async fn remap_virtual_mic(
    Path(id): Path<String>,
    State(s): State<AppShared>,
) -> impl IntoResponse {
    let mut state = s.state.lock().unwrap();
    let Some(mic) = state.virtual_mics.iter().find(|m| m.id == id) else {
        return (StatusCode::NOT_FOUND, "vmic not found".to_string()).into_response();
    };
    let new_id = make_mic_id(&mic.name);
    if new_id.is_empty() {
        return (StatusCode::BAD_REQUEST, "name slug is empty".to_string()).into_response();
    }
    if new_id == id {
        return (StatusCode::OK, Json(serde_json::json!({"id": id, "changed": false}))).into_response();
    }
    if state.virtual_mics.iter().any(|m| m.id == new_id) {
        return (StatusCode::CONFLICT, format!("id {new_id} already in use")).into_response();
    }

    let old_mix = format!("pw-ctrl.vmic.{}.mix", id);
    let new_mix = format!("pw-ctrl.vmic.{}.mix", new_id);
    let old_src = format!("pw-ctrl.vmic.{}", id);
    let new_src = format!("pw-ctrl.vmic.{}", new_id);

    for p in state.presets.iter_mut() {
        for o in p.outputs.iter_mut() {
            if o.node_name == old_mix { o.node_name = new_mix.clone(); }
            else if o.node_name == old_src { o.node_name = new_src.clone(); }
        }
        for si in p.source_inputs.iter_mut() {
            if *si == old_src { *si = new_src.clone(); }
            else if *si == old_mix { *si = new_mix.clone(); }
        }
    }
    if let Some(mic) = state.virtual_mics.iter_mut().find(|m| m.id == id) {
        mic.id = new_id.clone();
    }
    let _ = state.save();
    (StatusCode::OK, Json(serde_json::json!({"id": new_id, "changed": true}))).into_response()
}

async fn delete_virtual_mic(
    Path(id): Path<String>,
    State(s): State<AppShared>,
) -> impl IntoResponse {
    let mut state = s.state.lock().unwrap();
    state.virtual_mics.retain(|m| m.id != id);
    let _ = state.save();
    StatusCode::NO_CONTENT
}

async fn add_mic_input(
    Path(id): Path<String>,
    State(s): State<AppShared>,
    Json(body): Json<AddMicInputBody>,
) -> impl IntoResponse {
    let mut state = s.state.lock().unwrap();
    let Some(mic) = state.virtual_mics.iter_mut().find(|m| m.id == id) else {
        return StatusCode::NOT_FOUND;
    };
    mic.inputs.push(MicInput { node_name: body.node_name, gain: body.gain });
    let _ = state.save();
    StatusCode::OK
}

async fn remove_mic_input(
    Path((id, idx)): Path<(String, usize)>,
    State(s): State<AppShared>,
) -> impl IntoResponse {
    let mut state = s.state.lock().unwrap();
    let Some(mic) = state.virtual_mics.iter_mut().find(|m| m.id == id) else {
        return StatusCode::NOT_FOUND;
    };
    if idx >= mic.inputs.len() { return StatusCode::NOT_FOUND; }
    mic.inputs.remove(idx);
    let _ = state.save();
    StatusCode::OK
}

async fn update_mic_input_gain(
    Path((id, idx)): Path<(String, usize)>,
    State(s): State<AppShared>,
    Json(body): Json<UpdateInputGainBody>,
) -> impl IntoResponse {
    let mut state = s.state.lock().unwrap();
    let Some(mic) = state.virtual_mics.iter_mut().find(|m| m.id == id) else {
        return StatusCode::NOT_FOUND;
    };
    if idx >= mic.inputs.len() { return StatusCode::NOT_FOUND; }
    mic.inputs[idx].gain = body.gain;
    let _ = state.save();
    StatusCode::OK
}

// ── WebSocket ─────────────────────────────────────────────────────────────────

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(s): State<AppShared>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, s.engine))
}

async fn handle_socket(mut socket: WebSocket, engine: PwEngine) {
    let snapshot = engine.snapshot();
    if let Ok(msg) = serde_json::to_string(&snapshot) {
        if socket.send(Message::Text(msg.into())).await.is_err() {
            return;
        }
    }

    let mut rx = engine.subscribe();
    loop {
        tokio::select! {
            event = rx.recv() => {
                match event {
                    Ok(ev) => {
                        match serde_json::to_string(&ev) {
                            Ok(msg) => {
                                if socket.send(Message::Text(msg.into())).await.is_err() {
                                    break;
                                }
                            }
                            Err(e) => tracing::warn!("serialize error: {e}"),
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("WebSocket client lagged by {n} events, sending fresh snapshot");
                        if let Ok(msg) = serde_json::to_string(&engine.snapshot()) {
                            if socket.send(Message::Text(msg.into())).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Ping(data))) => {
                        let _ = socket.send(Message::Pong(data)).await;
                    }
                    _ => {}
                }
            }
        }
    }
}

// ── LV2 catalog handlers ──────────────────────────────────────────────────────

#[derive(serde::Serialize)]
struct Lv2Summary<'a> {
    uri: &'a str,
    name: &'a str,
    class: &'a str,
    has_native_ui: bool,
}

async fn list_lv2_plugins(State(s): State<AppShared>) -> impl IntoResponse {
    let cat = s.lv2.lock().unwrap();
    let summaries: Vec<Lv2Summary> = cat.plugins.iter().map(|p| Lv2Summary {
        uri: &p.uri,
        name: &p.name,
        class: &p.class,
        has_native_ui: p.has_native_ui,
    }).collect();
    Json(serde_json::to_value(&summaries).unwrap()).into_response()
}

async fn get_lv2_plugin(
    Path(uri): Path<String>,
    State(s): State<AppShared>,
) -> impl IntoResponse {
    let cat = s.lv2.lock().unwrap();
    match cat.find(&uri) {
        Some(p) => (StatusCode::OK, Json(Some(p.clone()))),
        None => (StatusCode::NOT_FOUND, Json(None::<Lv2Plugin>)),
    }
}

async fn rescan_lv2(State(s): State<AppShared>) -> impl IntoResponse {
    match Lv2Catalog::rescan() {
        Ok(cat) => {
            let n = cat.plugins.len();
            *s.lv2.lock().unwrap() = cat;
            (StatusCode::OK, Json(serde_json::json!({ "plugins": n }))).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn check_lv2_ui(
    Path(uri): Path<String>,
    State(s): State<AppShared>,
) -> impl IntoResponse {
    let folder = lv2::sanitize_uri(&uri);
    let path = s.static_dir.join("lv2-ui").join(&folder).join("index.js");
    let exists = path.exists();
    let rel = format!("/lv2-ui/{folder}/index.js");
    Json(serde_json::json!({ "exists": exists, "path": rel })).into_response()
}

// ── Chain handlers ────────────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(untagged)]
enum AddChainNodeBody {
    /// Tagged form: `{ "kind": "lv2"|"ladspa", ... }`.
    Tagged(AddChainTagged),
    /// Legacy LV2-only form: `{ "plugin_uri": "..." }`. Kept so older clients keep working.
    LegacyLv2 {
        plugin_uri: String,
        #[serde(default)] position: Option<usize>,
        #[serde(default)] label: Option<String>,
    },
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum AddChainTagged {
    Lv2 {
        plugin_uri: String,
        #[serde(default)] position: Option<usize>,
        #[serde(default)] label: Option<String>,
    },
    Ladspa {
        path: String,
        plugin_label: String,
        #[serde(default)] position: Option<usize>,
        #[serde(default)] label: Option<String>,
    },
}

async fn add_chain_node(
    Path(id): Path<String>,
    State(s): State<AppShared>,
    Json(body): Json<AddChainNodeBody>,
) -> impl IntoResponse {
    // Normalize all body shapes into a single (kind, position, label) triple.
    enum Resolved {
        Lv2 { uri: String, position: Option<usize>, label: Option<String> },
        Ladspa { path: String, plugin_label: String, position: Option<usize>, label: Option<String> },
    }
    let resolved = match body {
        AddChainNodeBody::LegacyLv2 { plugin_uri, position, label }
        | AddChainNodeBody::Tagged(AddChainTagged::Lv2 { plugin_uri, position, label }) =>
            Resolved::Lv2 { uri: plugin_uri, position, label },
        AddChainNodeBody::Tagged(AddChainTagged::Ladspa { path, plugin_label, position, label }) =>
            Resolved::Ladspa { path, plugin_label, position, label },
    };

    let (kind, position, ext_label) = match resolved {
        Resolved::Lv2 { uri, position, label } => {
            let defaults: std::collections::BTreeMap<String, f32> = {
                let cat = s.lv2.lock().unwrap();
                cat.find(&uri).map(|p| {
                    p.ports.iter()
                        .filter(|port| matches!(port.kind, lv2::PortKind::Control)
                                    && matches!(port.direction, lv2::PortDirection::Input))
                        .filter_map(|port| port.default.map(|d| (port.symbol.clone(), d)))
                        .collect()
                }).unwrap_or_default()
            };
            (ChainNodeKind::Lv2 { plugin_uri: uri, controls: defaults }, position, label)
        }
        Resolved::Ladspa { path, plugin_label, position, label } => {
            let defaults: std::collections::BTreeMap<String, f32> = {
                let cat = s.ladspa.lock().unwrap();
                cat.find_pair(&path, &plugin_label).map(|p| {
                    p.ports.iter()
                        .filter(|port| port.kind == ladspa_mod::PortKind::Control
                                    && port.direction == ladspa_mod::PortDirection::Input)
                        .filter_map(|port| port.default.map(|d| (port.name.clone(), d)))
                        .collect()
                }).unwrap_or_default()
            };
            (ChainNodeKind::Ladspa { path, label: plugin_label, controls: defaults }, position, label)
        }
    };

    let mut state = s.state.lock().unwrap();
    let Some(preset) = state.presets.iter_mut().find(|p| p.id == id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let node = ChainNode {
        id: preset.next_chain_id(),
        label: ext_label,
        bypass: false,
        kind,
    };
    let pos = position.unwrap_or(preset.chain.len()).min(preset.chain.len());
    preset.chain.insert(pos, node.clone());
    let _ = state.save();
    (StatusCode::CREATED, Json(node)).into_response()
}

async fn remove_chain_node(
    Path((id, idx)): Path<(String, usize)>,
    State(s): State<AppShared>,
) -> impl IntoResponse {
    let mut state = s.state.lock().unwrap();
    let Some(preset) = state.presets.iter_mut().find(|p| p.id == id) else {
        return StatusCode::NOT_FOUND;
    };
    if idx >= preset.chain.len() { return StatusCode::NOT_FOUND; }
    preset.chain.remove(idx);
    let _ = state.save();
    StatusCode::OK
}

#[derive(Deserialize)]
struct MoveBody { to: usize }

async fn move_chain_node(
    Path((id, idx)): Path<(String, usize)>,
    State(s): State<AppShared>,
    Json(body): Json<MoveBody>,
) -> impl IntoResponse {
    let mut state = s.state.lock().unwrap();
    let Some(preset) = state.presets.iter_mut().find(|p| p.id == id) else {
        return StatusCode::NOT_FOUND;
    };
    if idx >= preset.chain.len() { return StatusCode::NOT_FOUND; }
    let to = body.to.min(preset.chain.len() - 1);
    let node = preset.chain.remove(idx);
    preset.chain.insert(to, node);
    let _ = state.save();
    StatusCode::OK
}

#[derive(Deserialize)]
struct ControlBody { symbol: String, value: f32 }

async fn set_chain_control(
    Path((id, idx)): Path<(String, usize)>,
    State(s): State<AppShared>,
    Json(body): Json<ControlBody>,
) -> impl IntoResponse {
    let mut state = s.state.lock().unwrap();
    let Some(preset) = state.presets.iter_mut().find(|p| p.id == id) else {
        return StatusCode::NOT_FOUND;
    };
    let Some(node) = preset.chain.get_mut(idx) else { return StatusCode::NOT_FOUND };
    match &mut node.kind {
        ChainNodeKind::Lv2 { controls, .. }
        | ChainNodeKind::Builtin { controls, .. }
        | ChainNodeKind::Ladspa { controls, .. } => {
            controls.insert(body.symbol, body.value);
        }
    }
    let _ = state.save();
    StatusCode::OK
}

#[derive(Deserialize)]
struct BypassBody { bypass: bool }

async fn set_chain_bypass(
    Path((id, idx)): Path<(String, usize)>,
    State(s): State<AppShared>,
    Json(body): Json<BypassBody>,
) -> impl IntoResponse {
    let mut state = s.state.lock().unwrap();
    let Some(preset) = state.presets.iter_mut().find(|p| p.id == id) else {
        return StatusCode::NOT_FOUND;
    };
    let Some(node) = preset.chain.get_mut(idx) else { return StatusCode::NOT_FOUND };
    node.bypass = body.bypass;
    let _ = state.save();
    StatusCode::OK
}

#[derive(Deserialize)]
struct LabelBody { label: Option<String> }

async fn set_chain_label(
    Path((id, idx)): Path<(String, usize)>,
    State(s): State<AppShared>,
    Json(body): Json<LabelBody>,
) -> impl IntoResponse {
    let mut state = s.state.lock().unwrap();
    let Some(preset) = state.presets.iter_mut().find(|p| p.id == id) else {
        return StatusCode::NOT_FOUND;
    };
    let Some(node) = preset.chain.get_mut(idx) else { return StatusCode::NOT_FOUND };
    node.label = body.label;
    let _ = state.save();
    StatusCode::OK
}

// ── LADSPA catalog handlers ───────────────────────────────────────────────────

#[derive(serde::Serialize)]
struct LadspaSummary<'a> {
    /// Opaque key `{path}::{label}` used by /ladspa/plugins/{key} and the chain API.
    key: String,
    path: &'a str,
    label: &'a str,
    name: &'a str,
    unique_id: u32,
}

async fn list_ladspa_plugins(State(s): State<AppShared>) -> impl IntoResponse {
    let cat = s.ladspa.lock().unwrap();
    let summaries: Vec<LadspaSummary> = cat.plugins.iter().map(|p| LadspaSummary {
        key: p.key(),
        path: &p.path,
        label: &p.label,
        name: &p.name,
        unique_id: p.unique_id,
    }).collect();
    Json(serde_json::to_value(&summaries).unwrap()).into_response()
}

async fn get_ladspa_plugin(
    Path(key): Path<String>,
    State(s): State<AppShared>,
) -> impl IntoResponse {
    let cat = s.ladspa.lock().unwrap();
    match cat.find(&key) {
        Some(p) => (StatusCode::OK, Json(Some(p.clone()))),
        None => (StatusCode::NOT_FOUND, Json(None::<LadspaPlugin>)),
    }
}

async fn rescan_ladspa(State(s): State<AppShared>) -> impl IntoResponse {
    match LadspaCatalog::rescan() {
        Ok(cat) => {
            let n = cat.plugins.len();
            *s.ladspa.lock().unwrap() = cat;
            (StatusCode::OK, Json(serde_json::json!({ "plugins": n }))).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}
