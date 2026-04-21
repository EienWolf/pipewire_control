use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::get,
    Router,
};
use pipewire_control_core::{
    model::PwEvent,
    pw_engine::PwEngine,
};
use tower_http::cors::CorsLayer;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let engine = PwEngine::start();

    let app = Router::new()
        .route("/health", get(health))
        .route("/nodes", get(list_nodes))
        .route("/ws", get(ws_handler))
        .layer(CorsLayer::permissive())
        .with_state(engine);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:7878").await.unwrap();
    tracing::info!("listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, app).await.unwrap();
}

async fn health() -> &'static str {
    "ok"
}

async fn list_nodes(State(engine): State<PwEngine>) -> impl IntoResponse {
    let mut nodes: Vec<_> = engine.snapshot();
    nodes.sort_by_key(|n| n.id);
    (StatusCode::OK, Json(nodes))
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(engine): State<PwEngine>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, engine))
}

async fn handle_socket(mut socket: WebSocket, engine: PwEngine) {
    // Send full snapshot immediately on connect.
    let snapshot = PwEvent::Snapshot(engine.snapshot());
    if let Ok(msg) = serde_json::to_string(&snapshot) {
        if socket.send(Message::Text(msg.into())).await.is_err() {
            return;
        }
    }

    // Stream subsequent events.
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
                        let snapshot = PwEvent::Snapshot(engine.snapshot());
                        if let Ok(msg) = serde_json::to_string(&snapshot) {
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
