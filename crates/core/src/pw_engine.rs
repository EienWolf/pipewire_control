use crate::model::{AudioLink, AudioNode, EngineCmd, NodeState, PwEvent};
use pipewire::{self as pw, spa};
use pw::{link::Link, node::Node, proxy::ProxyT, types::ObjectType};
use std::{
    cell::RefCell,
    collections::HashMap,
    rc::Rc,
    sync::{Arc, RwLock},
};
use tokio::sync::broadcast;

#[derive(Clone)]
pub struct PwEngine {
    pub event_tx: broadcast::Sender<PwEvent>,
    pub nodes: Arc<RwLock<HashMap<u32, AudioNode>>>,
    pub links: Arc<RwLock<HashMap<u32, AudioLink>>>,
    pub cmd_tx: pw::channel::Sender<EngineCmd>,
}

impl PwEngine {
    pub fn start() -> Self {
        let (event_tx, _) = broadcast::channel(256);
        let nodes: Arc<RwLock<HashMap<u32, AudioNode>>> = Arc::new(RwLock::new(HashMap::new()));
        let links: Arc<RwLock<HashMap<u32, AudioLink>>> = Arc::new(RwLock::new(HashMap::new()));
        let (cmd_tx, cmd_rx) = pw::channel::channel::<EngineCmd>();

        let engine = Self {
            event_tx: event_tx.clone(),
            nodes: nodes.clone(),
            links: links.clone(),
            cmd_tx,
        };

        let nodes_t = nodes.clone();
        let links_t = links.clone();
        std::thread::Builder::new()
            .name("pw-engine".into())
            .spawn(move || pw_thread(cmd_rx, event_tx, nodes_t, links_t))
            .expect("failed to spawn PipeWire engine thread");

        engine
    }

    pub fn subscribe(&self) -> broadcast::Receiver<PwEvent> {
        self.event_tx.subscribe()
    }

    pub fn snapshot(&self) -> PwEvent {
        let nodes = self.nodes.read().unwrap().values().cloned().collect();
        let links = self.links.read().unwrap().values().cloned().collect();
        PwEvent::Snapshot { nodes, links }
    }
}

// ---------------------------------------------------------------------------
// PipeWire thread
// ---------------------------------------------------------------------------

fn pw_thread(
    cmd_rx: pw::channel::Receiver<EngineCmd>,
    event_tx: broadcast::Sender<PwEvent>,
    nodes: Arc<RwLock<HashMap<u32, AudioNode>>>,
    links: Arc<RwLock<HashMap<u32, AudioLink>>>,
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

    let ml_weak = main_loop.downgrade();
    let _cmd_attached = cmd_rx.attach(main_loop.loop_(), move |cmd| match cmd {
        EngineCmd::Shutdown => {
            if let Some(ml) = ml_weak.upgrade() { ml.quit(); }
        }
    });

    type NodeProxyMap = HashMap<u32, (Node, Vec<Box<dyn pw::proxy::Listener>>)>;
    type LinkProxyMap = HashMap<u32, (Link, Vec<Box<dyn pw::proxy::Listener>>)>;
    let node_proxies: Rc<RefCell<NodeProxyMap>> = Rc::new(RefCell::new(HashMap::new()));
    let link_proxies: Rc<RefCell<LinkProxyMap>> = Rc::new(RefCell::new(HashMap::new()));

    let registry_weak = registry.downgrade();
    let _reg_listener = registry
        .add_listener_local()
        .global({
            let node_proxies = node_proxies.clone();
            let link_proxies = link_proxies.clone();
            let nodes = nodes.clone();
            let links = links.clone();
            let event_tx = event_tx.clone();
            move |obj| {
                let Some(registry) = registry_weak.upgrade() else { return };
                match obj.type_ {
                    ObjectType::Node => handle_node(
                        obj, &registry, &nodes, &event_tx, &node_proxies,
                    ),
                    ObjectType::Link => handle_link(
                        obj, &registry, &links, &event_tx, &link_proxies,
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
}

// ── Node handler ─────────────────────────────────────────────────────────────

fn handle_node(
    obj: &pw::registry::GlobalObject<&spa::utils::dict::DictRef>,
    registry: &pw::registry::RegistryRc,
    nodes: &Arc<RwLock<HashMap<u32, AudioNode>>>,
    event_tx: &broadcast::Sender<PwEvent>,
    proxies: &Rc<RefCell<HashMap<u32, (Node, Vec<Box<dyn pw::proxy::Listener>>)>>>,
) {

    let Some(props) = obj.props else { return };
    let audio_node = AudioNode::from_props(obj.id, props);
    if !audio_node.is_audio() { return; }
    let node_id = audio_node.id;

    nodes.write().unwrap().insert(node_id, audio_node.clone());
    let _ = event_tx.send(PwEvent::NodeAdded(audio_node));

    let Ok(node): Result<Node, _> = registry.bind(obj) else { return };

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

    let proxy_id = node.upcast_ref().id();
    let proxies_weak = Rc::downgrade(proxies);
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
            if is_new {
                let _ = event_tx_c.send(PwEvent::LinkAdded(audio_link));
            }
        })
        .register();

    let proxy_id = link.upcast_ref().id();
    let proxies_weak = Rc::downgrade(proxies);
    let links_c = links.clone();
    let event_tx_c = event_tx.clone();
    let remove_listener = link
        .upcast_ref()
        .add_listener_local()
        .removed(move || {
            links_c.write().unwrap().remove(&proxy_id);
            let _ = event_tx_c.send(PwEvent::LinkRemoved { id: proxy_id });
            if let Some(p) = proxies_weak.upgrade() {
                p.borrow_mut().remove(&proxy_id);
            }
        })
        .register();

    proxies.borrow_mut().insert(
        link_id,
        (link, vec![Box::new(info_listener), Box::new(remove_listener)]),
    );
}
