use anyhow::Result;
use clap::{Parser, Subcommand};
use pipewire_control_core::ipc::{IpcRequest, IpcResponse};

#[derive(Parser)]
#[command(name = "pwctl", about = "PipeWire audio router control CLI")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Manage virtual sinks
    Sink {
        #[command(subcommand)]
        action: SinkAction,
    },
    /// Route an application stream to a virtual sink
    Route {
        stream_id: u32,
        sink_id: u32,
    },
    /// Remove a route
    Unroute { stream_id: u32 },
    /// List all nodes known to the daemon
    List,
    /// Stop the daemon
    Shutdown,
}

#[derive(Subcommand)]
enum SinkAction {
    /// Create a new virtual sink
    Add { name: String },
    /// Remove a virtual sink
    Remove { id: u32 },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    let request = match cli.command {
        Commands::Sink { action: SinkAction::Add { name } } => IpcRequest::AddSink { name },
        Commands::Sink { action: SinkAction::Remove { id } } => IpcRequest::RemoveSink { id },
        Commands::Route { stream_id, sink_id } => IpcRequest::Route { stream_id, sink_id },
        Commands::Unroute { stream_id } => IpcRequest::Unroute { stream_id },
        Commands::List => IpcRequest::ListNodes,
        Commands::Shutdown => IpcRequest::Shutdown,
    };

    let response = send_request(request).await?;
    println!("{}", serde_json::to_string_pretty(&response)?);
    Ok(())
}

async fn send_request(req: IpcRequest) -> Result<IpcResponse> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    let path = pipewire_control_core::ipc::socket_path();
    let mut stream = UnixStream::connect(&path).await?;
    let line = serde_json::to_string(&req)? + "\n";
    stream.write_all(line.as_bytes()).await?;

    let mut reader = BufReader::new(stream);
    let mut response_line = String::new();
    reader.read_line(&mut response_line).await?;
    Ok(serde_json::from_str(&response_line)?)
}
