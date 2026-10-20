//! NMW desktop app. The Rust side owns the mesh connection (today a simulated
//! mesh from `nmw-host`; a radio over USB later) and the web UI in `../ui`
//! talks to it through the commands below and `nmw-event` events.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use nmw_host::{
    parse_addr, ChannelInfo, ContactInfo, Mesh, Runtime, Status, VirtualMesh, VirtualMeshConfig,
};
use rand::RngCore;
use serde::Serialize;
use std::path::{Path, PathBuf};
use tauri::{Emitter, Manager, State};

struct AppState {
    runtime: Runtime,
    /// Where the joined channels are saved.
    channels_file: PathBuf,
}

impl AppState {
    fn save_channels(&self, m: &dyn Mesh) {
        let json = serde_json::to_string_pretty(&m.channels()).expect("channels serialize");
        if let Err(e) = std::fs::write(&self.channels_file, json) {
            eprintln!("could not save {}: {e}", self.channels_file.display());
        }
    }
}

#[derive(Serialize)]
struct Snapshot {
    status: Status,
    contacts: Vec<ContactInfo>,
    channels: Vec<ChannelInfo>,
}

#[tauri::command]
fn snapshot(state: State<AppState>) -> Snapshot {
    state.runtime.with(|m| Snapshot {
        status: m.status(),
        contacts: m.contacts(),
        channels: m.channels(),
    })
}

#[tauri::command]
fn join_channel(
    state: State<AppState>,
    name: String,
    key: Option<String>,
) -> Result<ChannelInfo, String> {
    state.runtime.with(|m| {
        let info = m.join_channel(&name, key.as_deref())?;
        state.save_channels(m);
        Ok(info)
    })
}

#[tauri::command]
fn leave_channel(state: State<AppState>, id: String) {
    state.runtime.with(|m| {
        m.leave_channel(&id);
        state.save_channels(m);
    })
}

#[tauri::command]
fn send_group(state: State<AppState>, id: String, text: String) -> Result<(), String> {
    state.runtime.with(|m| m.send_group(&id, &text))
}

#[tauri::command]
fn send_text(state: State<AppState>, to: String, text: String) -> Result<u16, String> {
    let to = parse_addr(&to).ok_or("not a valid address (32 hex characters)")?;
    state.runtime.with(|m| m.send_text(&to, &text))
}

#[tauri::command]
fn announce(state: State<AppState>) {
    state.runtime.with(|m| m.announce());
}

/// The node identity seed lives in the app data directory so the address
/// stays the same between runs.
fn load_or_create_seed(dir: &Path) -> std::io::Result<[u8; 32]> {
    let path = dir.join("identity.key");
    if let Ok(bytes) = std::fs::read(&path) {
        if let Ok(seed) = <[u8; 32]>::try_from(bytes.as_slice()) {
            return Ok(seed);
        }
    }
    let mut seed = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut seed);
    std::fs::create_dir_all(dir)?;
    std::fs::write(&path, seed)?;
    Ok(seed)
}

/// Channels saved by an earlier run, or `#public` on the first run.
fn load_channels(path: &Path) -> Vec<ChannelInfo> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            eprintln!("ignoring unreadable {}: {e}", path.display());
            Vec::new()
        }),
        Err(_) => nmw_host::resolve_channel(nmw_host::PUBLIC_CHANNEL, None)
            .into_iter()
            .collect(),
    }
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            let seed = load_or_create_seed(&data_dir)?;
            let channels_file = data_dir.join("channels.json");
            let mut mesh = VirtualMesh::new(VirtualMeshConfig {
                my_seed: seed,
                my_name: "My node".into(),
                ..Default::default()
            });
            for c in load_channels(&channels_file) {
                if let Err(e) = mesh.join_channel(&c.name, (!c.public).then_some(c.key.as_str())) {
                    eprintln!("skipping saved channel {}: {e}", c.name);
                }
            }
            // NMW_SPEED=10 runs the simulated mesh ten times faster, for demos.
            let speed = std::env::var("NMW_SPEED")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(1.0);
            let handle = app.handle().clone();
            let runtime = Runtime::start(Box::new(mesh), speed, move |event| {
                let _ = handle.emit("nmw-event", event);
            });
            app.manage(AppState {
                runtime,
                channels_file,
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            snapshot,
            send_text,
            announce,
            join_channel,
            leave_channel,
            send_group
        ])
        .run(tauri::generate_context!())
        .expect("error while running the NMW desktop app");
}
