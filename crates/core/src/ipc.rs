use serde::{Deserialize, Serialize};

pub fn socket_path() -> String {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
        .unwrap_or_else(|_| "/tmp".to_string());
    format!("{runtime_dir}/pipewire-controld.sock")
}

/// Commands sent from CLI / web server to the daemon over the Unix socket.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", content = "params")]
pub enum IpcRequest {
    AddSink { name: String },
    RemoveSink { id: u32 },
    Route { stream_id: u32, sink_id: u32 },
    Unroute { stream_id: u32 },
    ListNodes,
    Shutdown,
}

/// Daemon responses.
#[derive(Debug, Serialize, Deserialize)]
pub struct IpcResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl IpcResponse {
    pub fn ok(data: impl Serialize) -> Self {
        Self {
            ok: true,
            data: Some(serde_json::to_value(data).unwrap_or_default()),
            error: None,
        }
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Self { ok: false, data: None, error: Some(msg.into()) }
    }
}
