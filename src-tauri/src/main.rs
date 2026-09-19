//! MAIC Desktop shell: Tauri builder and window lifecycle. The boot flow
//! (extract → stage → serve → open the main window) lives in `boot`.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod boot;
mod health;
mod port;
#[cfg(windows)]
mod proc;
mod server_tree;
mod sidecar;
mod splash;

use tauri::{Manager, RunEvent};

use crate::boot::{boot_in_background, owns_server, ServerChild};
use crate::splash::create_splash_window;

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            create_splash_window(app)?;
            // Heavy boot work runs on a worker thread; the splash is swapped
            // for the main window once /api/health is green.
            let handle = app.handle().clone();
            std::thread::spawn(move || boot_in_background(&handle));
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::Destroyed = event {
                // Only "main" owns the server lifetime (see owns_server):
                // splash closes during the splash -> main handoff, while the
                // server must stay alive for main. RunEvent::Exit below is
                // the final backup.
                if !owns_server(window.label()) {
                    return;
                }
                if let Some(state) = window.app_handle().try_state::<ServerChild>() {
                    state.stop();
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("failed to build maic-desktop")
        .run(|app, event| {
            if let RunEvent::Exit = event {
                if let Some(state) = app.try_state::<ServerChild>() {
                    state.stop();
                }
            }
        });
}
