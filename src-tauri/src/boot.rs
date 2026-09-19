//! Boot flow:
//!  1. `setup` (main thread) shows the splash window immediately, then spawns
//!     a worker thread running `boot_in_background`.
//!  2. Worker: ensure the bundled server tree is extracted to the app data dir
//!     (shipped as server.tar.gz — a plain directory snapshot; it carries no
//!     symlinks, so extraction needs no post-processing). Skipped when the
//!     staged .build-meta.json already matches.
//!  3. Pick a free loopback port from the OS.
//!  4. Spawn the bundled Node sidecar running the Next.js standalone server
//!     with PORT/HOSTNAME pointed at it.
//!  5. Poll /api/health until it responds 200 (or time out with a fatal dialog).
//!  6. Close the splash window and open the main window against the local server.
//!
//! Exiting the app terminates the sidecar. No system Node.js is required.

use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tauri::path::BaseDirectory;
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};

use crate::health::{wait_for_health, HealthOutcome};
use crate::port::{pick_free_port, pick_sticky_port};
use crate::server_tree::{ensure_server, read_bundled_meta, stage_fresh_server};
use crate::sidecar::ensure_sidecar;
use crate::splash::{js_string, splash_status, STATUS_CHECKING, STATUS_EXTRACTING, STATUS_PROBING};

const MAX_PORT_ATTEMPTS: u32 = 5;

/// Spawn the sidecar and block until /api/health is green.
/// Returns the base URL plus the child handle (killed on app exit).
///
/// Self-heal: a crash whose log shows MODULE_NOT_FOUND means the staged tree
/// is broken in a way the static checks above did not catch (a file quarantined
/// or truncated after extraction). The tree is then re-extracted from the
/// bundled tarball exactly once and startup retried — the bundle itself was
/// boot-verified at build time, so a clean re-extraction is the repair.
fn start_server(
    app: &tauri::AppHandle,
    server_dir: &std::path::Path,
    node_bin: &std::path::Path,
    staged_meta: &str,
) -> Result<(String, Child), String> {
    let server_js = server_dir.join("server.js");
    if !server_js.exists() {
        return Err(format!(
            "server.js missing at {}. Rebuild with: pnpm prepare:server",
            server_js.display()
        ));
    }

    let mut last_err = String::new();
    // At most two full passes: the initial pass, plus one retry pass after a
    // MODULE_NOT_FOUND heal (re-extraction). Spelled as a bounded loop so
    // clippy::never_loop stays quiet.
    for pass in 0..2u32 {
        // First attempt uses the sticky port (keeps the origin — and therefore
        // IndexedDB/localStorage — stable across launches). Fall back to fresh
        // ports if it is busy (e.g. a second instance).
        let mut first: Option<u16> = match pick_sticky_port(app) {
            Ok(p) => Some(p),
            Err(e) => {
                eprintln!("maic-desktop: sticky port unavailable ({e}), allocating fresh");
                None
            }
        };
        for _ in 0..MAX_PORT_ATTEMPTS {
            let port = match first.take() {
                Some(p) => p,
                None => pick_free_port()?,
            };
            splash_status(app, STATUS_PROBING, Some(port));
            let mut cmd = Command::new(node_bin);
            cmd.arg(server_js.to_string_lossy().to_string())
                .env("PORT", port.to_string())
                .env("HOSTNAME", "127.0.0.1")
                .env("NODE_ENV", "production")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            #[cfg(windows)]
            crate::proc::hide_console(&mut cmd);
            let child = cmd.spawn();

            match child {
                Ok(mut child) => {
                    // Keep the tail of server output: on failure it goes into
                    // the fatal dialog, so a bug report carries the real Node
                    // error.
                    let log: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
                    // Drain pipes so a chatty server log can't block on a full
                    // buffer.
                    if let Some(out) = child.stdout.take() {
                        let log = Arc::clone(&log);
                        std::thread::spawn(move || {
                            use std::io::BufRead;
                            for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
                                println!("[server] {line}");
                                push_log(&log, &line);
                            }
                        });
                    }
                    if let Some(err) = child.stderr.take() {
                        let log = Arc::clone(&log);
                        std::thread::spawn(move || {
                            use std::io::BufRead;
                            for line in std::io::BufReader::new(err).lines().map_while(Result::ok) {
                                eprintln!("[server] {line}");
                                push_log(&log, &line);
                            }
                        });
                    }

                    match wait_for_health(port, &mut child) {
                        HealthOutcome::Healthy => {
                            // Record the port that actually serves, so the next
                            // launch reuses it and the origin
                            // (IndexedDB/localStorage) stays put.
                            if let Ok(data_dir) = app.path().app_data_dir() {
                                let _ = std::fs::write(
                                    data_dir.join("server-port.json"),
                                    port.to_string(),
                                );
                            }
                            return Ok((format!("http://127.0.0.1:{port}/"), child));
                        }
                        outcome => {
                            // Give the drain threads a moment to flush the
                            // exit error.
                            std::thread::sleep(Duration::from_millis(500));
                            let tail = log.lock().map(|g| g.clone()).unwrap_or_default();
                            let _ = child.kill();
                            last_err = match outcome {
                                HealthOutcome::NoListener => format!(
                                    "server on port {port} never accepted connections (it likely crashed on startup){}",
                                    format_log_tail(&tail),
                                ),
                                _ => format!(
                                    "server on port {port} never became healthy{}",
                                    format_log_tail(&tail),
                                ),
                            };
                            // Broken staged tree (MODULE_NOT_FOUND in the log):
                            // re-extract once, then pass 1 retries from the
                            // top. A genuinely bad bundle still reports its
                            // error after the second pass.
                            if tail.contains("MODULE_NOT_FOUND") && pass == 0 {
                                eprintln!(
                                    "maic-desktop: server crashed with MODULE_NOT_FOUND; re-extracting server tree and retrying"
                                );
                                let tarball = app
                                    .path()
                                    .resolve("resources/server.tar.gz", BaseDirectory::Resource)
                                    .map_err(|e| format!("failed to resolve resources: {e}"))?;
                                let data_dir = app
                                    .path()
                                    .app_data_dir()
                                    .map_err(|e| format!("failed to resolve app data dir: {e}"))?;
                                splash_status(app, STATUS_EXTRACTING, None);
                                stage_fresh_server(
                                    app,
                                    &tarball,
                                    &data_dir,
                                    server_dir,
                                    staged_meta,
                                )?;
                                break;
                            }
                            // Try another port.
                        }
                    }
                }
                Err(e) => {
                    last_err = format!("failed to spawn bundled node (port busy?): {e}");
                }
            }
        }
        // All port attempts exhausted without a healthy server (and without a
        // healable crash on this pass): fall through to the next pass or to
        // the final error below.
    }
    Err(format!(
        "could not start the bundled MAIC server after {MAX_PORT_ATTEMPTS} attempts. {last_err}"
    ))
}

/// Append a line to the shared tail buffer, keeping roughly the last 4 KB.
fn push_log(log: &Mutex<String>, line: &str) {
    if let Ok(mut guard) = log.lock() {
        guard.push_str(line);
        guard.push('\n');
        const KEEP: usize = 4096;
        if guard.len() > KEEP * 2 {
            let drop = guard.len() - KEEP;
            let cut = guard[drop..]
                .find('\n')
                .map(|i| drop + i + 1)
                .unwrap_or(drop);
            guard.drain(..cut);
        }
    }
}

/// Render the captured tail for the fatal dialog (empty when silent).
fn format_log_tail(tail: &str) -> String {
    let tail = tail.trim();
    if tail.is_empty() {
        String::new()
    } else {
        format!(".\n\nServer output:\n{tail}")
    }
}

/// App state holding the server child so it can be killed on exit.
pub(crate) struct ServerChild(Mutex<Option<Child>>);

impl ServerChild {
    pub(crate) fn stop(&self) {
        if let Ok(mut guard) = self.0.lock() {
            if let Some(mut child) = guard.take() {
                let _ = child.kill();
            }
        }
    }
}

/// Heavy boot work off the main thread: extract, stage, serve, then swap
/// the splash window for the real one. Runs on a worker thread; UI updates
/// go through the AppHandle (send-safe).
pub(crate) fn boot_in_background(app: &tauri::AppHandle) {
    if let Err(message) = boot(app) {
        boot_failed(app, &message);
    }
}

fn boot(app: &tauri::AppHandle) -> Result<(), String> {
    // Bundled build marker (also used to version the staged sidecar copy).
    let staged_meta = app
        .path()
        .resolve("resources/server.tar.gz", BaseDirectory::Resource)
        .map(|t| read_bundled_meta(&t).unwrap_or_else(|_| "{}".to_string()))
        .unwrap_or_else(|_| "{}".to_string());
    splash_status(app, STATUS_CHECKING, None);
    let server_dir = ensure_server(app, &staged_meta)?;
    let node_bin = ensure_sidecar(app, &staged_meta)?;
    let (url, child) = start_server(app, &server_dir, &node_bin, &staged_meta)?;
    println!("maic-desktop: serving {url}");
    app.manage(ServerChild(Mutex::new(Some(child))));
    let parsed: tauri::Url = url
        .parse()
        .map_err(|e| format!("invalid server url: {e}"))?;
    // Windows MUST be created on the main thread: building the main window
    // here (worker thread) silently fails, leaving no windows at all — the
    // runtime then exits and takes the healthy server down with it.
    let handle = app.clone();
    app.run_on_main_thread(move || {
        if let Some(splash) = handle.get_webview_window("splash") {
            let _ = splash.close();
        }
        if let Err(e) = WebviewWindowBuilder::new(&handle, "main", WebviewUrl::External(parsed))
            .title("MAIC Desktop")
            .inner_size(1280.0, 800.0)
            .build()
        {
            boot_failed(&handle, &format!("failed to create main window: {e}"));
        }
    })
    .map_err(|e| format!("failed to schedule main window: {e}"))?;
    Ok(())
}

/// Show the fatal error inside the splash window when one exists — the
/// message (including the captured server log tail) outlives the dialog —
/// and always show the dialog. Without a splash nothing carries the error,
/// so exit once the dialog closes.
fn boot_failed(app: &tauri::AppHandle, message: &str) {
    eprintln!("maic-desktop fatal: {message}");
    let splash = app.get_webview_window("splash");
    if let Some(splash) = &splash {
        // Order matters: paint the message first, then the dialog — the
        // user lands on a window that explains the failure either way.
        let _ = splash.eval(format!(
            "window.__maicFatal && window.__maicFatal({})",
            js_string(message)
        ));
    }
    let _ = app
        .dialog()
        .message(message.to_string())
        .title("MAIC Desktop")
        .kind(MessageDialogKind::Error)
        .blocking_show();
    if splash.is_none() {
        std::process::exit(1);
    }
}

/// Whether destroying the window with this label should tear down the server.
/// Only "main" owns the server lifetime: "splash" closes during the
/// splash -> main handoff while the server must keep running for main.
pub(crate) fn owns_server(label: &str) -> bool {
    label == "main"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_main_owns_server_lifetime() {
        assert!(owns_server("main"));
        // Closing any other window (notably the splash during the
        // splash -> main handoff) must not kill the server.
        assert!(!owns_server("splash"));
        assert!(!owns_server(""));
    }
}

#[cfg(test)]
mod log_tail_tests {
    use super::*;

    #[test]
    fn tail_keeps_last_lines() {
        let log = Mutex::new(String::new());
        for i in 0..200 {
            push_log(
                &log,
                &format!("line {i:03} padding-padding-padding-padding"),
            );
        }
        let guard = log.lock().unwrap();
        assert!(guard.len() <= 8192 + 64);
        assert!(guard.contains("line 199"));
        assert!(!guard.contains("line 000"));
    }

    #[test]
    fn tail_formats_for_dialog() {
        assert_eq!(format_log_tail(""), "");
        assert_eq!(format_log_tail("   \n  "), "");
        let out = format_log_tail("Error: boom\n");
        assert!(out.contains("Server output:"));
        assert!(out.contains("Error: boom"));
    }
}
