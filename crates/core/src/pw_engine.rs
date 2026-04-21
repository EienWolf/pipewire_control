use crate::model::{AudioNode, EngineCmd, NodeState, PwEvent};
use pipewire as pw;
use pw::{node::Node, proxy::ProxyT, types::ObjectType};
use std::{
    cell::RefCell,
    collections::HashMap,
    rc::Rc,
    sync::{Arc, RwLock},
};
use tokio::sync::broadcast;

/// Thread-safe handle to the PipeWire engine.
/// Clone freely — all clones share the same underlying state.
#[derive(Clone)]
pub struct PwEngine {
    /// Subscribe to receive graph change events.
    pub event_tx: broadcast::Sender<PwEvent>,
    /// Current snapshot of all audio nodes, keyed by PipeWire id.
    pub nodes: Arc<RwLock<HashMap<u32, AudioNode>>>,
    /// Send commands into the PipeWire thread.
    pub cmd_tx: pw::channel::Sender<EngineCmd>,
}

impl PwEngine {
    /// Start the PipeWire thread and return a handle.
    pub fn start() -> Self {
        let (event_tx, _) = broadcast::channel(256);
        let nodes: Arc<RwLock<HashMap<u32, AudioNode>>> = Arc::new(RwLock::new(HashMap::new()));
        let (cmd_tx, cmd_rx) = pw::channel::channel::<EngineCmd>();

        let engine = Self { event_tx: event_tx.clone(), nodes: nodes.clone(), cmd_tx };

        let nodes_thread = nodes.clone();
        std::thread::Builder::new()
            .name("pw-engine".into())
            .spawn(move || pw_thread(cmd_rx, event_tx, nodes_thread))
            .expect("failed to spawn PipeWire engine thread");

        engine
    }

    pub fn subscribe(&self) -> broadcast::Receiver<PwEvent> {
        self.event_tx.subscribe()
    }

    pub fn snapshot(&self) -> Vec<AudioNode> {
        self.nodes.read().unwrap().values().cloned().collect()
    }
}

// ---------------------------------------------------------------------------
// PipeWire thread — everything below runs on the dedicated PW thread.
// No Send constraints needed; Rc<> is fine here.
// ---------------------------------------------------------------------------

fn pw_thread(
    cmd_rx: pw::channel::Receiver<EngineCmd>,
    event_tx: broadcast::Sender<PwEvent>,
    nodes: Arc<RwLock<HashMap<u32, AudioNode>>>,
) {
    pw::init();

    let main_loop = pw::main_loop::MainLoopRc::new(None)
        .expect("failed to create PipeWire main loop");
    let context = pw::context::ContextRc::new(&main_loop, None)
        .expect("failed to create PipeWire context");
    let core = context.connect_rc(None)
        .expect("failed to connect to PipeWire");
    let registry = core.get_registry_rc()
        .expect("failed to get PipeWire registry");

    // Attach command channel.
    let ml_weak = main_loop.downgrade();
    let _cmd_attached = cmd_rx.attach(main_loop.loop_(), move |cmd| match cmd {
        EngineCmd::Shutdown => {
            if let Some(ml) = ml_weak.upgrade() {
                ml.quit();
            }
        }
    });

    // Proxy + listener storage — kept alive for the duration of the run.
    // Value: (Node proxy, Vec of associated listeners)
    type ProxyMap = HashMap<u32, (Node, Vec<Box<dyn pw::proxy::Listener>>)>;
    let proxies: Rc<RefCell<ProxyMap>> = Rc::new(RefCell::new(HashMap::new()));

    let registry_weak = registry.downgrade();
    let _reg_listener = registry
        .add_listener_local()
        .global({
            let proxies = proxies.clone();
            let nodes = nodes.clone();
            let event_tx = event_tx.clone();
            move |obj| {
                if obj.type_ != ObjectType::Node {
                    return;
                }
                let Some(props) = obj.props else { return };

                let audio_node = AudioNode::from_props(obj.id, props);
                if !audio_node.is_audio() {
                    return;
                }
                let node_id = audio_node.id;

                nodes.write().unwrap().insert(node_id, audio_node.clone());
                let _ = event_tx.send(PwEvent::NodeAdded(audio_node));

                // Bind proxy to receive state/port-count updates.
                let Some(registry) = registry_weak.upgrade() else { return };
                let Ok(node): Result<Node, _> = registry.bind(obj) else { return };

                let nodes_c = nodes.clone();
                let event_tx_c = event_tx.clone();
                let info_listener = node
                    .add_listener_local()
                    .info(move |info| {
                        let mut map = nodes_c.write().unwrap();
                        if let Some(n) = map.get_mut(&info.id()) {

                            n.state = match info.state() {
                                pw::node::NodeState::Creating => NodeState::Creating,
                                pw::node::NodeState::Suspended => NodeState::Suspended,
                                pw::node::NodeState::Idle => NodeState::Idle,
                                pw::node::NodeState::Running => NodeState::Running,
                                pw::node::NodeState::Error(e) => NodeState::Error(e.to_owned()),
                            };
                            n.n_input_ports = info.n_input_ports();
                            n.n_output_ports = info.n_output_ports();
                            // Merge any updated properties.
                            if let Some(props) = info.props() {
                                for (k, v) in props.iter() {
                                    match k {
                                        "node.nick" => n.node_nick = Some(v.to_owned()),
                                        "node.description" => n.node_description = Some(v.to_owned()),
                                        "application.name" => n.application_name = Some(v.to_owned()),
                                        "media.name" => n.media_name = Some(v.to_owned()),
                                        _ => { n.extra_props.insert(k.to_owned(), v.to_owned()); }
                                    }
                                }
                            }
                            let _ = event_tx_c.send(PwEvent::NodeUpdated(n.clone()));
                        }
                    })
                    .register();

                // Proxy removed listener cleans up our map entry.
                let proxy_id = node.upcast_ref().id();
                let proxies_weak = Rc::downgrade(&proxies);
                let nodes_c = nodes.clone();
                let event_tx_c = event_tx.clone();
                let remove_listener = node
                    .upcast_ref()
                    .add_listener_local()
                    .removed(move || {
                        nodes_c.write().unwrap().remove(&proxy_id);
                        let _ = event_tx_c.send(PwEvent::NodeRemoved { id: proxy_id });
                        if let Some(p) = proxies_weak.upgrade() {
                            p.borrow_mut().remove(&proxy_id);
                        }
                    })
                    .register();

                proxies.borrow_mut().insert(
                    node_id,
                    (node, vec![Box::new(info_listener), Box::new(remove_listener)]),
                );
            }
        })
        .global_remove({
            let nodes = nodes.clone();
            let event_tx = event_tx.clone();
            move |id| {
                if nodes.write().unwrap().remove(&id).is_some() {
                    let _ = event_tx.send(PwEvent::NodeRemoved { id });
                }
            }
        })
        .register();

    main_loop.run();
}
