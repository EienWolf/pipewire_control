# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

**pipewire_control** is a PipeWire-based audio routing and effects application inspired by SteelSeries Sonar. It exposes its functionality through three interfaces: a CLI, a native GTK4 app, and an optional web server. The audio engine supports virtual sinks/sources, per-stream routing, and a plugin-based effects/EQ chain (modeled after EasyEffects).

## Build Commands

```bash
# Build all crates
cargo build

# Build release
cargo build --release

# Run CLI
cargo run -p pwctl -- <args>

# Run GTK4 app
cargo run -p pipewire-control-gtk

# Run web server
cargo run -p pipewire-control-web

# Run all tests
cargo test

# Run tests for a specific crate
cargo test -p pipewire-control-core

# Run a single test
cargo test -p pipewire-control-core test_name

# Lint
cargo clippy --all-targets --all-features

# Format
cargo fmt --all
```

## Workspace Structure

```
pipewire_control/
├── Cargo.toml              # Workspace root
├── crates/
│   ├── core/               # PipeWire engine, routing state, effects chain
│   ├── cli/                # CLI binary (pwctl)
│   ├── gtk-app/            # GTK4 + libadwaita UI
│   ├── web-server/         # Optional HTTP/WebSocket server (axum)
│   └── effects/            # Built-in DSP effects + LV2/LADSPA plugin loader
└── plugins/                # Optional bundled LV2 plugins
```

## Architecture

### Core (`crates/core`)

The heart of the project. All three frontends depend on it — never put PipeWire or DSP logic in the frontends.

- **`pw_engine`**: Owns the PipeWire main loop and registry. Manages node lifecycle (virtual sinks, sources, filters) using the `pipewire` crate (pipewire-rs).
- **`router`**: State machine for stream-to-node assignments. Tracks which app stream maps to which virtual sink and which effects chain.
- **`effects_chain`**: An ordered list of `Effect` trait objects per virtual sink. Each effect wraps either a built-in DSP block or an LV2/LADSPA plugin instance.
- **`state`**: Serializable snapshot of the entire routing + effects config. Loaded at startup, persisted on change. Lives in `~/.config/pipewire-control/`.
- **`ipc`**: Unix domain socket IPC used by CLI and web server to communicate with the running daemon. The GTK app may embed core directly instead of going through IPC.

### Daemon model

The core runs as a background daemon (`pipewire-controld`). CLI and web server talk to it via IPC. The GTK app can either connect to the daemon or embed core in-process (configurable).

### Effects Plugin System

Modeled after EasyEffects:
- `Effect` trait: `fn process(&mut self, buffer: &mut [f32])` + metadata (name, parameters).
- Built-in effects live in `crates/effects/src/builtin/` (parametric EQ, compressor, limiter, gate).
- LV2 plugins loaded via `lv2` crate; LADSPA via raw `dlopen`. Plugin discovery scans `LV2_PATH` and `LADSPA_PATH`.
- Each virtual sink has its own chain: `Vec<Box<dyn Effect>>`.

### Frontends

- **CLI** (`crates/cli`): Built with `clap`. Sends commands to daemon via IPC. Subcommands: `sink`, `source`, `route`, `effects`, `profile`.
- **GTK4 app** (`crates/gtk-app`): Built with `gtk4` + `libadwaita`. Embeds or connects to core. Uses `relm4` for reactive UI patterns.
- **Web server** (`crates/web-server`): `axum`-based REST + WebSocket. REST for commands, WebSocket for live state push. Optional — not started by default.

## Key Dependencies

| Crate | Purpose |
|---|---|
| `pipewire` | PipeWire Rust bindings (pipewire-rs) |
| `gtk4` + `libadwaita` | Native GTK4 UI |
| `relm4` | Elm-style reactive GTK4 patterns |
| `clap` | CLI argument parsing |
| `axum` | Web server |
| `tokio` | Async runtime (web server + IPC) |
| `lv2` | LV2 plugin host |
| `serde` + `serde_json` / `toml` | State serialization |
| `zbus` | D-Bus integration (for media key support, MPRIS) |

## IPC Protocol

Unix socket at `/run/user/$UID/pipewire-controld.sock`. Messages are newline-delimited JSON (`serde_json`). Request: `{"cmd": "route", "params": {...}}`. Response: `{"ok": true, "data": {...}}` or `{"ok": false, "error": "..."}`.

## Feature Roadmap (Priority Order)

1. **Phase 1 — Core engine**: PipeWire virtual sinks, stream detection, basic routing (no effects).
2. **Phase 2 — CLI**: Full CRUD for sinks, sources, routes, profiles via IPC.
3. **Phase 3 — Effects chain**: Built-in parametric EQ per sink; LV2 plugin loader.
4. **Phase 4 — GTK4 app**: Visual stream router + EQ editor.
5. **Phase 5 — Web server**: REST API + WebSocket, browser-based UI (optional).

## System Requirements

- PipeWire ≥ 0.3.65
- GTK 4.x + libadwaita (for GTK app)
- LV2 development headers (optional, for plugin hosting)
- Rust stable ≥ 1.75

## Notes

- The PipeWire main loop is **not** thread-safe; all PW operations must happen on the PW thread. Use `pipewire::channel::channel()` to send commands from other threads into the PW loop.
- Virtual sinks are created as `pw_filter` nodes with the `PW_KEY_MEDIA_CLASS` set to `"Audio/Sink"`.
- Stream routing is done by setting `node.target` link metadata on the PipeWire session manager (WirePlumber).
