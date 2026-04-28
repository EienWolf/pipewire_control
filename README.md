# pipewire_control

PipeWire-based audio routing and effects application inspired by SteelSeries Sonar. Provides virtual audio sinks/sources, per-stream routing, parametric EQ, and a plugin-based effects chain.

## Features

- **Preset System**: Create profiles with LV2 plugin chains (EQ, reverb, etc.) applied to specific audio streams
- **Virtual Mics**: Mix multiple microphone inputs into a single virtual capture source for apps like Discord/OBS
- **Stream Routing**: Route individual audio streams to presets
- **Real-time Monitoring**: Web-based visualization of all audio nodes and connections
- **LV2 Plugin Support**: Use any LV2 plugin for audio processing

## Requirements

- Rust (stable)
- PipeWire with LV2 support
- `lv2` and `ladspa` binaries (usually installed via `pipewire` package)

## Build

```bash
cargo build                          # build all crates
cargo build --release                 # release build
cargo test -p pipewire-control-core   # run tests
cargo clippy --all-targets --all-features  # lint
cargo fmt --all                      # format code
```

## Run

```bash
# Web server (default port 7878)
cargo run -p pipewire-control-web

# CLI tool (pwctl)
cargo run -p pwctl -- <command>

# GTK4 application
cargo run -p pipewire-control-gtk
```

## Web Server API

The web server provides a REST API and WebSocket for real-time control:

### Endpoints

| Method | Path | Description |
|--------|------|-------------|
| GET/POST | `/presets` | List/create presets |
| GET/PUT/DELETE | `/presets/{id}` | Get/update/delete preset |
| POST | `/presets/{id}/activate` | Activate preset (spin up PipeWire filter) |
| POST | `/presets/{id}/deactivate` | Deactivate preset |
| POST | `/presets/{id}/outputs` | Add output assignment |
| DELETE/PUT | `/presets/{id}/outputs/{idx}` | Remove/update output volume |
| POST | `/presets/{id}/route/{node_id}` | Route stream to preset |
| POST | `/presets/{id}/unroute/{node_id}` | Remove routing |
| GET/POST | `/virtual-mics` | List/create virtual mics |
| POST | `/virtual-mics/{id}/activate` | Activate virtual mic |
| GET | `/nodes` | Get all audio nodes snapshot |
| GET | `/lv2/plugins` | List available LV2 plugins |
| GET | `/lv2/plugins/{uri}` | Get plugin details |
| POST | `/lv2/rescan` | Rescan LV2 plugins |
| GET | `/ws` | WebSocket connection |

### UI Features

- **Monitor Tab**: Real-time audio node visualization
- **Graph Tab**: Draggable SVG graph with bezier links
- **Presets Tab**: EQ bands editor, output assignments, stream routing
- **Virtual Mics Tab**: Input sources with gain control

## Architecture

```
pipewire_control/
├── crates/
│   ├── core/        # Core audio engine (PipeWire, routing, DSP)
│   ├── cli/         # CLI binary (pwctl)
│   ├── gtk-app/     # GTK4 application
│   └── web-server/  # Web server + UI
```

### Core Modules

| Module | Purpose |
|--------|---------|
| `pw_engine` | PipeWire main loop, registry listener |
| `preset` | Virtual sink with LV2 effect chain |
| `virtual_mic` | Virtual source mixing inputs |
| `lv2` | LV2 plugin catalog |
| `conf_gen` | Generates PipeWire config files |
| `state` | Persistent application state |

All audio processing happens in PipeWire-generated filter chains; no DSP runs in Rust.

## Configuration

State is persisted to `~/.config/pipewire-control/state.toml`. Changes are applied by:

1. Edit preset/virtual mic configuration
2. Call `/config/apply` endpoint
3. This regenerates `~/.config/pipewire/pipewire.conf.d/pwctl.conf`
4. PipeWire and WirePlumber are restarted

## License

MIT License
