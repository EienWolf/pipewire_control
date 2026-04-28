use crate::model::{AudioLink, AudioNode, EngineCmd, NodeState, PwEvent};
use crate::playback::PlaybackKnobs;
use pipewire::{self as pw, spa};
use pw::{link::Link, metadata::Metadata, node::Node, proxy::ProxyT, stream::StreamRc, types::ObjectType};
use spa::pod::Pod;
use std::sync::atomic::{AtomicBool, Ordering};
use std::{
    cell::RefCell,
    collections::HashMap,
    rc::Rc,
    sync::{Arc, Mutex, RwLock},
};
use tokio::sync::broadcast;

/// Stable command sender that survives PipeWire daemon disconnects.
///
/// The underlying `pw::channel::Sender` is bound to a single PipeWire
/// connection; on reconnect, the engine swaps in a fresh sender. Holders of
/// `EngineCmdSender` (e.g. the web server) keep working transparently.
#[derive(Clone)]
pub struct EngineCmdSender {
    inner: Arc<Mutex<pw::channel::Sender<EngineCmd>>>,
}

impl EngineCmdSender {
    pub fn send(&self, cmd: EngineCmd) -> Result<(), EngineCmd> {
        self.inner.lock().unwrap().send(cmd)
    }
}

#[derive(Clone)]
pub struct PwEngine {
    pub event_tx: broadcast::Sender<PwEvent>,
    pub nodes: Arc<RwLock<HashMap<u32, AudioNode>>>,
    pub links: Arc<RwLock<HashMap<u32, AudioLink>>>,
    /// (default_sink_node_name, default_source_node_name)
    pub defaults: Arc<RwLock<(Option<String>, Option<String>)>>,
    pub cmd_tx: EngineCmdSender,
}

impl PwEngine {
    pub fn start() -> Self {
        let (event_tx, _) = broadcast::channel(256);
        let nodes: Arc<RwLock<HashMap<u32, AudioNode>>> = Arc::new(RwLock::new(HashMap::new()));
        let links: Arc<RwLock<HashMap<u32, AudioLink>>> = Arc::new(RwLock::new(HashMap::new()));
        let defaults: Arc<RwLock<(Option<String>, Option<String>)>> =
            Arc::new(RwLock::new((None, None)));
        let (cmd_tx_inner, cmd_rx) = pw::channel::channel::<EngineCmd>();
        let cmd_slot: Arc<Mutex<pw::channel::Sender<EngineCmd>>> =
            Arc::new(Mutex::new(cmd_tx_inner));
        let cmd_tx = EngineCmdSender { inner: cmd_slot.clone() };

        let engine = Self {
            event_tx: event_tx.clone(),
            nodes: nodes.clone(),
            links: links.clone(),
            defaults: defaults.clone(),
            cmd_tx: cmd_tx.clone(),
        };

        let nodes_t = nodes.clone();
        let links_t = links.clone();
        let defaults_t = defaults.clone();
        std::thread::Builder::new()
            .name("pw-engine".into())
            .spawn(move || pw_thread_loop(cmd_rx, cmd_slot, event_tx, nodes_t, links_t, defaults_t))
            .expect("failed to spawn PipeWire engine thread");

        engine
    }

    pub fn subscribe(&self) -> broadcast::Receiver<PwEvent> {
        self.event_tx.subscribe()
    }

    pub fn snapshot(&self) -> PwEvent {
        let nodes = self.nodes.read().unwrap().values().cloned().collect();
        let links = self.links.read().unwrap().values().cloned().collect();
        let (default_sink, default_source) = self.defaults.read().unwrap().clone();
        PwEvent::Snapshot { nodes, links, default_sink, default_source }
    }
}

// ---------------------------------------------------------------------------
// PipeWire thread
// ---------------------------------------------------------------------------

/// Outer loop: connects to PipeWire and reconnects on daemon errors/disconnects.
/// Returns only on EngineCmd::Shutdown.
fn pw_thread_loop(
    initial_cmd_rx: pw::channel::Receiver<EngineCmd>,
    cmd_slot: Arc<Mutex<pw::channel::Sender<EngineCmd>>>,
    event_tx: broadcast::Sender<PwEvent>,
    nodes: Arc<RwLock<HashMap<u32, AudioNode>>>,
    links: Arc<RwLock<HashMap<u32, AudioLink>>>,
    defaults: Arc<RwLock<(Option<String>, Option<String>)>>,
) {
    pw::init();

    // Long-lived: spawn a dedicated thread to call `wpctl get-volume`.
    let (vol_tx, vol_rx) = std::sync::mpsc::channel::<u32>();
    {
        let nodes_v = nodes.clone();
        let event_tx_v = event_tx.clone();
        std::thread::Builder::new()
            .name("vol-fetch".into())
            .spawn(move || vol_fetch_thread(vol_rx, nodes_v, event_tx_v))
            .expect("failed to spawn vol-fetch thread");
    }

    let mut cmd_rx = initial_cmd_rx;
    let mut backoff_ms: u64 = 250;

    loop {
        // Wipe state from any previous connection so subscribers see a clean reset.
        nodes.write().unwrap().clear();
        links.write().unwrap().clear();
        *defaults.write().unwrap() = (None, None);
        let _ = event_tx.send(PwEvent::Snapshot {
            nodes: vec![],
            links: vec![],
            default_sink: None,
            default_source: None,
        });

        let outcome = run_connection(cmd_rx, &event_tx, &nodes, &links, &defaults, &vol_tx);

        match outcome {
            ConnectionOutcome::Shutdown => break,
            ConnectionOutcome::Disconnected => {
                tracing::warn!("PipeWire connection lost; reconnecting in {backoff_ms}ms");
                std::thread::sleep(std::time::Duration::from_millis(backoff_ms));
                backoff_ms = (backoff_ms * 2).min(5_000);
                let (new_tx, new_rx) = pw::channel::channel::<EngineCmd>();
                *cmd_slot.lock().unwrap() = new_tx;
                cmd_rx = new_rx;
            }
            ConnectionOutcome::ConnectFailed => {
                tracing::warn!("PipeWire connect failed; retrying in {backoff_ms}ms");
                std::thread::sleep(std::time::Duration::from_millis(backoff_ms));
                backoff_ms = (backoff_ms * 2).min(5_000);
                let (new_tx, new_rx) = pw::channel::channel::<EngineCmd>();
                *cmd_slot.lock().unwrap() = new_tx;
                cmd_rx = new_rx;
            }
            ConnectionOutcome::Connected => {
                // Successful connection that ended cleanly without explicit shutdown
                // (e.g. main_loop quit by error listener after an established session).
                backoff_ms = 250;
                let (new_tx, new_rx) = pw::channel::channel::<EngineCmd>();
                *cmd_slot.lock().unwrap() = new_tx;
                cmd_rx = new_rx;
            }
        }
    }
}

enum ConnectionOutcome {
    Shutdown,
    Disconnected,
    ConnectFailed,
    Connected,
}

fn run_connection(
    cmd_rx: pw::channel::Receiver<EngineCmd>,
    event_tx: &broadcast::Sender<PwEvent>,
    nodes: &Arc<RwLock<HashMap<u32, AudioNode>>>,
    links: &Arc<RwLock<HashMap<u32, AudioLink>>>,
    defaults: &Arc<RwLock<(Option<String>, Option<String>)>>,
    vol_tx: &std::sync::mpsc::Sender<u32>,
) -> ConnectionOutcome {
    let main_loop = match pw::main_loop::MainLoopRc::new(None) {
        Ok(ml) => ml,
        Err(e) => {
            tracing::error!("failed to create PipeWire main loop: {e}");
            return ConnectionOutcome::ConnectFailed;
        }
    };
    let context = match pw::context::ContextRc::new(&main_loop, None) {
        Ok(ctx) => ctx,
        Err(e) => {
            tracing::error!("failed to create PipeWire context: {e}");
            return ConnectionOutcome::ConnectFailed;
        }
    };
    let core = match context.connect_rc(None) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("failed to connect to PipeWire: {e}");
            return ConnectionOutcome::ConnectFailed;
        }
    };
    let registry = match core.get_registry_rc() {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("failed to get PipeWire registry: {e}");
            return ConnectionOutcome::ConnectFailed;
        }
    };

    // Owned clones for closures that need 'static lifetime.
    let nodes = nodes.clone();
    let links = links.clone();
    let defaults = defaults.clone();
    let event_tx = event_tx.clone();
    let vol_tx = vol_tx.clone();

    // Track whether the loop is exiting due to Shutdown vs. core error.
    let shutdown_flag: Rc<RefCell<bool>> = Rc::new(RefCell::new(false));
    let disconnected_flag: Rc<RefCell<bool>> = Rc::new(RefCell::new(false));

    // All Rc state must be declared before any closures that capture them.
    type NodeProxyMap     = HashMap<u32, (Node,     Vec<Box<dyn pw::proxy::Listener>>)>;
    type LinkProxyMap     = HashMap<u32, (Link,     Vec<Box<dyn pw::proxy::Listener>>)>;
    type MetadataProxyMap = HashMap<u32, (Metadata, Vec<Box<dyn pw::proxy::Listener>>)>;
    let node_proxies:     Rc<RefCell<NodeProxyMap>>     = Rc::new(RefCell::new(HashMap::new()));
    let link_proxies:     Rc<RefCell<LinkProxyMap>>     = Rc::new(RefCell::new(HashMap::new()));
    let metadata_proxies: Rc<RefCell<MetadataProxyMap>> = Rc::new(RefCell::new(HashMap::new()));

    // ID of the PipeWire "default" Metadata object.
    let default_metadata_id: Rc<RefCell<Option<u32>>> = Rc::new(RefCell::new(None));

    // ── Active soundboard playbacks (lives entirely on the PW thread) ────────
    type PlaybackMap = HashMap<String, (StreamRc, Vec<Box<dyn std::any::Any>>)>;
    let playbacks: Rc<RefCell<PlaybackMap>> = Rc::new(RefCell::new(HashMap::new()));

    // ── Per-connection cmd channel for RT callbacks ──────────────────────────
    // RT process callbacks can't lock the public sender's mutex, so they get a
    // direct (per-connection) clone of the raw pw::channel sender.
    let (rt_finish_tx, rt_finish_rx) = pw::channel::channel::<EngineCmd>();
    let _rt_finish_attached = rt_finish_rx.attach(main_loop.loop_(), {
        let playbacks = playbacks.clone();
        let event_tx_rt = event_tx.clone();
        move |cmd| {
            if let EngineCmd::PlaybackFinished { playback_id } = cmd {
                if let Some((stream, _)) = playbacks.borrow_mut().remove(&playback_id) {
                    let _ = stream.disconnect();
                }
                let _ = event_tx_rt.send(PwEvent::PlaybackEnded { playback_id });
            }
        }
    });

    // ── Core error / disconnect listener ─────────────────────────────────────
    let ml_weak_err = main_loop.downgrade();
    let disc_flag_err = disconnected_flag.clone();
    let _core_listener = core
        .add_listener_local()
        .error(move |id, _seq, res, message| {
            tracing::warn!("PipeWire core error: id={id} res={res} msg={message}");
            // res=-EPIPE (-32) typically means the daemon hung up; any error on
            // the core proxy (id=0) is fatal to this connection.
            if id == 0 || res == -32 {
                *disc_flag_err.borrow_mut() = true;
                if let Some(ml) = ml_weak_err.upgrade() { ml.quit(); }
            }
        })
        .register();

    // ── cmd_rx handler ────────────────────────────────────────────────────────
    let ml_weak = main_loop.downgrade();
    let core_for_cmds = core.clone();
    let cmd_tx_for_finish = rt_finish_tx.clone();
    let event_tx_cmd = event_tx.clone();
    let shutdown_flag_cmd = shutdown_flag.clone();
    let _cmd_attached = cmd_rx.attach(main_loop.loop_(), {
        let playbacks = playbacks.clone();
        move |cmd| match cmd {
            EngineCmd::Shutdown => {
                *shutdown_flag_cmd.borrow_mut() = true;
                if let Some(ml) = ml_weak.upgrade() { ml.quit(); }
            }
            EngineCmd::PlayClip {
                playback_id, clip_id, samples, sample_rate, target_node_name, knobs,
            } => {
                let frames = (samples.len() / 2) as u64;
                let duration_ms = if sample_rate > 0 {
                    frames * 1000 / sample_rate as u64
                } else { 0 };
                match start_playback(
                    &core_for_cmds, &playback_id, samples, sample_rate,
                    target_node_name.as_deref(), knobs, cmd_tx_for_finish.clone(),
                ) {
                    Ok(entry) => {
                        playbacks.borrow_mut().insert(playback_id.clone(), entry);
                        let _ = event_tx_cmd.send(PwEvent::PlaybackStarted {
                            playback_id, clip_id, target_node_name, duration_ms,
                        });
                    }
                    Err(e) => {
                        tracing::warn!("PlayClip {playback_id} failed: {e}");
                        let _ = event_tx_cmd.send(PwEvent::PlaybackEnded { playback_id });
                    }
                }
            }
            EngineCmd::StopPlayback { playback_id } => {
                if let Some((stream, _)) = playbacks.borrow_mut().remove(&playback_id) {
                    let _ = stream.disconnect();
                }
                let _ = event_tx_cmd.send(PwEvent::PlaybackEnded { playback_id });
            }
            EngineCmd::PlaybackFinished { playback_id } => {
                if let Some((stream, _)) = playbacks.borrow_mut().remove(&playback_id) {
                    let _ = stream.disconnect();
                }
                let _ = event_tx_cmd.send(PwEvent::PlaybackEnded { playback_id });
            }
        }
    });

    // ── Registry listener ─────────────────────────────────────────────────────
    let registry_weak = registry.downgrade();
    let _reg_listener = registry
        .add_listener_local()
        .global({
            let node_proxies = node_proxies.clone();
            let link_proxies = link_proxies.clone();
            let metadata_proxies = metadata_proxies.clone();
            let default_metadata_id = default_metadata_id.clone();
            let nodes = nodes.clone();
            let links = links.clone();
            let event_tx = event_tx.clone();
            let vol_tx = vol_tx.clone();
            move |obj| {
                let Some(registry) = registry_weak.upgrade() else { return };
                match obj.type_ {
                    ObjectType::Node => handle_node(
                        obj, &registry, &nodes, &event_tx, &node_proxies, &vol_tx,
                    ),
                    ObjectType::Link => handle_link(
                        obj, &registry, &links, &event_tx, &link_proxies,
                    ),
                    ObjectType::Metadata => handle_metadata(
                        obj, &registry, &defaults, &event_tx,
                        &metadata_proxies, &default_metadata_id,
                    ),
                    _ => {}
                }
            }
        })
        .global_remove({
            let nodes = nodes.clone();
            let links = links.clone();
            let event_tx = event_tx.clone();
            move |id| {
                if nodes.write().unwrap().remove(&id).is_some() {
                    let _ = event_tx.send(PwEvent::NodeRemoved { id });
                } else if links.write().unwrap().remove(&id).is_some() {
                    let _ = event_tx.send(PwEvent::LinkRemoved { id });
                }
            }
        })
        .register();

    main_loop.run();

    // Drop all per-connection state explicitly so proxies/listeners are torn down
    // before we try to reconnect.
    drop(_cmd_attached);
    drop(_rt_finish_attached);
    drop(_reg_listener);
    drop(_core_listener);
    node_proxies.borrow_mut().clear();
    link_proxies.borrow_mut().clear();
    metadata_proxies.borrow_mut().clear();
    playbacks.borrow_mut().clear();

    if *shutdown_flag.borrow() {
        ConnectionOutcome::Shutdown
    } else if *disconnected_flag.borrow() {
        ConnectionOutcome::Disconnected
    } else {
        ConnectionOutcome::Connected
    }
}

// ── Node handler ─────────────────────────────────────────────────────────────

fn handle_node(
    obj: &pw::registry::GlobalObject<&spa::utils::dict::DictRef>,
    registry: &pw::registry::RegistryRc,
    nodes: &Arc<RwLock<HashMap<u32, AudioNode>>>,
    event_tx: &broadcast::Sender<PwEvent>,
    proxies: &Rc<RefCell<HashMap<u32, (Node, Vec<Box<dyn pw::proxy::Listener>>)>>>,
    vol_tx: &std::sync::mpsc::Sender<u32>,
) {

    let Some(props) = obj.props else { return };
    let audio_node = AudioNode::from_props(obj.id, props);
    if !audio_node.is_audio() { return; }
    let node_id = audio_node.id;

    nodes.write().unwrap().insert(node_id, audio_node.clone());
    let _ = event_tx.send(PwEvent::NodeAdded(audio_node));

    let Ok(node): Result<Node, _> = registry.bind(obj) else { return };

    // Subscribe to Props param so we get volume/mute state and live updates.
    node.subscribe_params(&[spa::param::ParamType::Props]);

    let nodes_c = nodes.clone();
    let event_tx_c = event_tx.clone();
    let info_listener = node
        .add_listener_local()
        .info(move |info| {
            let mut map = nodes_c.write().unwrap();
            if let Some(n) = map.get_mut(&info.id()) {
                n.state = match info.state() {
                    pw::node::NodeState::Creating  => NodeState::Creating,
                    pw::node::NodeState::Suspended => NodeState::Suspended,
                    pw::node::NodeState::Idle      => NodeState::Idle,
                    pw::node::NodeState::Running   => NodeState::Running,
                    pw::node::NodeState::Error(e)  => NodeState::Error(e.to_owned()),
                };
                n.n_input_ports  = info.n_input_ports();
                n.n_output_ports = info.n_output_ports();
                if let Some(props) = info.props() {
                    for (k, v) in props.iter() {
                        match k {
                            "node.nick"        => n.node_nick        = Some(v.to_owned()),
                            "node.description" => n.node_description = Some(v.to_owned()),
                            "application.name" => n.application_name = Some(v.to_owned()),
                            "media.name"       => n.media_name       = Some(v.to_owned()),
                            _ => { n.extra_props.insert(k.to_owned(), v.to_owned()); }
                        }
                    }
                }
                let _ = event_tx_c.send(PwEvent::NodeUpdated(n.clone()));
            }
        })
        .register();

    let vol_tx_c = vol_tx.clone();
    let param_listener = node
        .add_listener_local()
        .param(move |_seq, _id, _index, _next, _param| {
            // Param fired (volume/mute changed) — ask the subprocess thread to fetch new values.
            let _ = vol_tx_c.send(node_id);
        })
        .register();

    let proxies_weak = Rc::downgrade(proxies);
    let remove_listener = node
        .upcast_ref()
        .add_listener_local()
        .removed(move || {
            if let Some(p) = proxies_weak.upgrade() {
                p.borrow_mut().remove(&node_id);
            }
        })
        .register();

    proxies.borrow_mut().insert(
        node_id,
        (node, vec![Box::new(info_listener), Box::new(param_listener), Box::new(remove_listener)]),
    );
}

// ── Volume/mute fetch thread ──────────────────────────────────────────────────

fn vol_fetch_thread(
    rx: std::sync::mpsc::Receiver<u32>,
    nodes: Arc<RwLock<HashMap<u32, AudioNode>>>,
    event_tx: broadcast::Sender<PwEvent>,
) {
    while let Ok(first_id) = rx.recv() {
        // Drain queued IDs to deduplicate rapid-fire param events.
        let mut pending = std::collections::HashSet::new();
        pending.insert(first_id);
        while let Ok(id) = rx.try_recv() {
            pending.insert(id);
        }

        for node_id in pending {
            let Ok(out) = std::process::Command::new("wpctl")
                .args(["get-volume", &node_id.to_string()])
                .output()
            else { continue };
            if !out.status.success() { continue }

            let text = String::from_utf8_lossy(&out.stdout);
            let Some(rest) = text.trim().strip_prefix("Volume: ") else { continue };
            let muted = rest.contains("[MUTED]");
            let vol_str = rest.split_whitespace().next().unwrap_or("1.0");
            let Ok(vol) = vol_str.parse::<f32>() else { continue };

            let mut map = nodes.write().unwrap();
            if let Some(n) = map.get_mut(&node_id) {
                if n.volume == Some(vol) && n.mute == Some(muted) { continue }
                n.volume = Some(vol);
                n.mute = Some(muted);
                let _ = event_tx.send(PwEvent::NodeUpdated(n.clone()));
            }
        }
    }
}

// ── Link handler ─────────────────────────────────────────────────────────────

fn handle_link(
    obj: &pw::registry::GlobalObject<&spa::utils::dict::DictRef>,
    registry: &pw::registry::RegistryRc,
    links: &Arc<RwLock<HashMap<u32, AudioLink>>>,
    event_tx: &broadcast::Sender<PwEvent>,
    proxies: &Rc<RefCell<HashMap<u32, (Link, Vec<Box<dyn pw::proxy::Listener>>)>>>,
) {

    let link_id = obj.id;
    let Ok(link): Result<Link, _> = registry.bind(obj) else { return };

    let links_c = links.clone();
    let event_tx_c = event_tx.clone();
    let info_listener = link
        .add_listener_local()
        .info(move |info| {
            let active = matches!(info.state(), pw::link::LinkState::Active);
            let audio_link = AudioLink {
                id: info.id(),
                output_node: info.output_node_id(),
                input_node: info.input_node_id(),
                active,
            };
            let mut map = links_c.write().unwrap();
            let is_new = !map.contains_key(&audio_link.id);
            map.insert(audio_link.id, audio_link.clone());
            drop(map);
            if is_new {
                let _ = event_tx_c.send(PwEvent::LinkAdded(audio_link));
            } else {
                let _ = event_tx_c.send(PwEvent::LinkUpdated(audio_link));
            }
        })
        .register();

    let proxies_weak = Rc::downgrade(proxies);
    let remove_listener = link
        .upcast_ref()
        .add_listener_local()
        .removed(move || {
            // Just drop our proxy — global_remove already handles the map entry and event.
            if let Some(p) = proxies_weak.upgrade() {
                p.borrow_mut().remove(&link_id);
            }
        })
        .register();

    proxies.borrow_mut().insert(
        link_id,
        (link, vec![Box::new(info_listener), Box::new(remove_listener)]),
    );
}

// ── Metadata handler ──────────────────────────────────────────────────────────

fn handle_metadata(
    obj: &pw::registry::GlobalObject<&spa::utils::dict::DictRef>,
    registry: &pw::registry::RegistryRc,
    defaults: &Arc<RwLock<(Option<String>, Option<String>)>>,
    event_tx: &broadcast::Sender<PwEvent>,
    proxies: &Rc<RefCell<HashMap<u32, (Metadata, Vec<Box<dyn pw::proxy::Listener>>)>>>,
    default_metadata_id: &Rc<RefCell<Option<u32>>>,
) {
    // Only care about the "default" metadata object (holds default.audio.sink/source).
    let Some(props) = obj.props else { return };
    let is_default = props.iter().any(|(k, v)| k == "metadata.name" && v == "default");
    if !is_default { return; }
    *default_metadata_id.borrow_mut() = Some(obj.id);

    let Ok(metadata): Result<Metadata, _> = registry.bind(obj) else { return };

    let defaults_c = defaults.clone();
    let event_tx_c = event_tx.clone();
    let prop_listener = metadata
        .add_listener_local()
        .property(move |_subject, key, _type_, value| {
            // Value is JSON like {"name":"node.name"} or bare node name.
            let node_name = value.map(|v| {
                serde_json::from_str::<serde_json::Value>(v)
                    .ok()
                    .and_then(|j| j.get("name")?.as_str().map(str::to_owned))
                    .unwrap_or_else(|| v.to_owned())
            });
            let mut d = defaults_c.write().unwrap();
            match key {
                Some("default.audio.sink")   => d.0 = node_name,
                Some("default.audio.source") => d.1 = node_name,
                _ => return 0,
            }
            let (sink_name, source_name) = d.clone();
            drop(d);
            let _ = event_tx_c.send(PwEvent::DefaultsChanged { sink_name, source_name });
            0
        })
        .register();

    proxies.borrow_mut().insert(obj.id, (metadata, vec![Box::new(prop_listener)]));
}

// ── Soundboard playback ──────────────────────────────────────────────────────

/// Spawn a PipeWire playback stream for an in-memory stereo f32 clip.
/// On finish (non-loop), the RT process callback posts `PlaybackFinished`
/// back into the cmd channel so the main loop can drop the stream cleanly.
fn start_playback(
    core: &pw::core::CoreRc,
    playback_id: &str,
    samples: Arc<Vec<f32>>,
    sample_rate: u32,
    target_node_name: Option<&str>,
    knobs: Arc<PlaybackKnobs>,
    cmd_tx: pw::channel::Sender<EngineCmd>,
) -> Result<(StreamRc, Vec<Box<dyn std::any::Any>>), pw::Error> {
    use pw::properties::properties;

    let mut props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Playback",
        *pw::keys::MEDIA_ROLE => "Production",
        *pw::keys::AUDIO_CHANNELS => "2",
        *pw::keys::APP_NAME => "pipewire-control soundboard",
        *pw::keys::NODE_NAME => format!("pw-ctrl.sb.{playback_id}").as_str(),
    };
    if let Some(target) = target_node_name {
        props.insert("target.object", target);
    }

    let stream = StreamRc::new(core.clone(), "pw-ctrl-soundboard", props)?;

    let stream_weak = stream.downgrade();
    let finished_flag = Arc::new(AtomicBool::new(false));
    let finished_for_cb = finished_flag.clone();
    let pid_for_cb = playback_id.to_string();
    let cmd_tx_for_cb = cmd_tx.clone();

    // RT-side state.
    let mut position: usize = 0;

    let process_listener = stream
        .add_local_listener::<()>()
        .process(move |stream, _| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let datas = buffer.datas_mut();
            if datas.is_empty() { return; }
            let stride = std::mem::size_of::<f32>() * 2; // stereo f32
            let data = &mut datas[0];

            let n_frames = if let Some(slice) = data.data() {
                let cap_frames = slice.len() / stride;
                let mut produced = 0usize;
                let total = samples.len();
                let vol = knobs.volume();
                let looped = knobs.looped();
                let stop = knobs.stop_requested();

                while produced < cap_frames {
                    if stop {
                        // Zero-fill the rest, mark finished, send cleanup.
                        for f in produced..cap_frames {
                            let off = f * stride;
                            slice[off..off + stride].fill(0);
                        }
                        produced = cap_frames;
                        if !finished_for_cb.swap(true, Ordering::Relaxed) {
                            let _ = cmd_tx_for_cb.send(EngineCmd::PlaybackFinished {
                                playback_id: pid_for_cb.clone(),
                            });
                        }
                        break;
                    }
                    if position >= total {
                        if looped {
                            position = 0;
                        } else {
                            for f in produced..cap_frames {
                                let off = f * stride;
                                slice[off..off + stride].fill(0);
                            }
                            produced = cap_frames;
                            if !finished_for_cb.swap(true, Ordering::Relaxed) {
                                let _ = cmd_tx_for_cb.send(EngineCmd::PlaybackFinished {
                                    playback_id: pid_for_cb.clone(),
                                });
                            }
                            break;
                        }
                    }
                    let l = samples[position] * vol;
                    let r = samples[position + 1] * vol;
                    position += 2;

                    let off = produced * stride;
                    slice[off..off + 4].copy_from_slice(&l.to_le_bytes());
                    slice[off + 4..off + 8].copy_from_slice(&r.to_le_bytes());
                    produced += 1;
                }
                produced
            } else { 0 };

            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = stride as _;
            *chunk.size_mut() = (stride * n_frames) as _;
        })
        .register()?;

    // Build the EnumFormat param for stereo f32 at the source rate.
    let mut audio_info = spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(spa::param::audio::AudioFormat::F32LE);
    audio_info.set_rate(sample_rate);
    audio_info.set_channels(2);
    let mut position_chmap = [0; spa::param::audio::MAX_CHANNELS];
    position_chmap[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    position_chmap[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    audio_info.set_position(position_chmap);

    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(pw::spa::pod::Object {
            type_: spa::sys::SPA_TYPE_OBJECT_Format,
            id: spa::sys::SPA_PARAM_EnumFormat,
            properties: audio_info.into(),
        }),
    )
    .unwrap()
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&values).unwrap()];

    stream.connect(
        spa::utils::Direction::Output,
        None,
        pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::RT_PROCESS,
        &mut params,
    )?;

    let _ = stream_weak; // (kept for potential diagnostics)
    Ok((stream, vec![Box::new(process_listener)]))
}
