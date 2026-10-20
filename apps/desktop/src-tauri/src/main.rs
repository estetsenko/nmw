//! NMW desktop app. The Rust side owns the mesh connection (today a simulated
//! mesh from `nmw-host`; a radio over USB later) and the web UI in `../ui`
//! talks to it through the commands below and `nmw-event` events.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use nmw_host::{parse_addr, ContactInfo, Runtime, Status, VirtualMesh, VirtualMeshConfig};
use rand::RngCore;
use serde::Serialize;
use std::path::Path;
use tauri::{Emitter, Manager, State};

struct AppState {
    runtime: Runtime,
}

#[derive(Serialize)]
struct Snapshot {
    status: Status,
    contacts: Vec<ContactInfo>,
}

#[tauri::command]
fn snapshot(state: State<AppState>) -> Snapshot {
    state.runtime.with(|m| Snapshot {
        status: m.status(),
        contacts: m.contacts(),
    })
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

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            let seed = load_or_create_seed(&app.path().app_data_dir()?)?;
            let mesh = VirtualMesh::new(VirtualMeshConfig {
                my_seed: seed,
                my_name: "My node".into(),
                ..Default::default()
            });
            // NMW_SPEED=10 runs the simulated mesh ten times faster, for demos.
            let speed = std::env::var("NMW_SPEED")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(1.0);
            let handle = app.handle().clone();
            let runtime = Runtime::start(Box::new(mesh), speed, move |event| {
                let _ = handle.emit("nmw-event", event);
            });
            app.manage(AppState { runtime });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![snapshot, send_text, announce])
        .run(tauri::generate_context!())
        .expect("error while running the NMW desktop app");
}
