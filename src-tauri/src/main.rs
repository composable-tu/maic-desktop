// main.rs — MAIC Desktop shell.
//
// Boot flow (all on the main thread, inside `setup`):
//  1. Ensure the bundled server tree is extracted to the app data dir
//     (shipped as server.tar.gz because bundlers don't preserve symlinks;
//     skipped when the staged .build-meta.json already matches).
//  2. Pick a free loopback port from the OS.
//  3. Spawn the bundled Node sidecar running the Next.js standalone server
//     with PORT/HOSTNAME pointed at it.
//  4. Poll /api/health until it responds 200 (or time out with a fatal dialog).
//  5. Open the main window against the local server.
//
// Exiting the app terminates the sidecar. No system Node.js is required.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::path::BaseDirectory;
use tauri::{Manager, RunEvent, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
const POLL_INTERVAL: Duration = Duration::from_millis(200);
const MAX_PORT_ATTEMPTS: u32 = 5;

/// Ask the OS for a free loopback port. The listener is dropped immediately,
/// so the race window is small; the caller retries with a fresh port on failure.
fn pick_free_port() -> Result<u16, String> {
    let port = TcpListener::bind("127.0.0.1:0")
        .map_err(|e| format!("failed to bind loopback for port selection: {e}"))?
        .local_addr()
        .map_err(|e| format!("failed to read selected port: {e}"))?
        .port();
    Ok(port)
}

/// Check whether a loopback port is currently free.
fn port_is_free(port: u16) -> bool {
    TcpListener::bind(format!("127.0.0.1:{port}")).is_ok()
}

/// Sticky port selection. Web storage (IndexedDB, localStorage, Cache API) is
/// scoped to the origin, so a random port every launch would orphan all cached
/// data. Reuse the port recorded in the app data dir when it is still free;
/// otherwise allocate a fresh one and record it.
fn pick_sticky_port(app: &tauri::AppHandle) -> Result<u16, String> {
    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("failed to resolve app data dir: {e}"))?;
    let marker = data_dir.join("server-port.json");

    if let Ok(text) = std::fs::read_to_string(&marker) {
        if let Ok(port) = text.trim().parse::<u16>() {
            if port != 0 && port_is_free(port) {
                println!("maic-desktop: reusing sticky port {port}");
                return Ok(port);
            }
        }
    }

    let port = pick_free_port()?;
    // Best effort: a stale marker is harmless (next launch retries).
    let _ = std::fs::create_dir_all(&data_dir)
        .and_then(|_| std::fs::write(&marker, port.to_string()));
    println!("maic-desktop: allocated fresh port {port}");
    Ok(port)
}

fn fatal(app: &tauri::AppHandle, message: &str) -> ! {
    eprintln!("maic-desktop fatal: {message}");
    // We run on the main thread during setup, so blocking is fine.
    let _ = app
        .dialog()
        .message(message.to_string())
        .title("MAIC Desktop")
        .kind(MessageDialogKind::Error)
        .blocking_show();
    std::process::exit(1);
}

/// Locate the extracted server tree, extracting server.tar.gz on first launch
/// (or when the bundled build differs from what's on disk).
fn ensure_server(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    // Dev builds run straight from the source tree, where prepare-server
    // leaves an unpacked server/ dir — use it directly if present.
    let dev_dir = app
        .path()
        .resolve("resources/server", BaseDirectory::Resource)
        .map_err(|e| format!("failed to resolve resources: {e}"))?;
    let dev_meta = dev_dir.join(".build-meta.json");
    if dev_meta.exists() {
        let server_js = dev_dir.join("server.js");
        if server_js.exists() {
            println!("maic-desktop: using dev server tree at {}", dev_dir.display());
            return Ok(dev_dir);
        }
    }

    let tarball = app
        .path()
        .resolve("resources/server.tar.gz", BaseDirectory::Resource)
        .map_err(|e| format!("failed to resolve bundled server.tar.gz: {e}"))?;
    if !tarball.exists() {
        return Err(format!(
            "bundled server not found (looked for {} and {}). Rebuild with: node scripts/prepare-server.mjs",
            dev_dir.join("server.js").display(),
            tarball.display(),
        ));
    }

    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("failed to resolve app data dir: {e}"))?;
    let staged_meta = read_bundled_meta(&tarball).unwrap_or_else(|_| "{}".to_string());
    let marker = data_dir.join(".build-meta.json");
    let current = std::fs::read_to_string(&marker).unwrap_or_default();
    let server_dir = data_dir.join("server");

    if current != staged_meta || !server_dir.join("server.js").exists() {
        println!("maic-desktop: extracting server runtime (first launch or update)…");
        std::fs::create_dir_all(&data_dir)
            .map_err(|e| format!("failed to create app data dir: {e}"))?;
        // Remove any previous tree so stale files can't shadow the new build.
        let _ = std::fs::remove_dir_all(&server_dir);
        let status = Command::new("tar")
            .arg("-xzf")
            .arg(&tarball)
            .arg("-C")
            .arg(&data_dir)
            .status()
            .map_err(|e| format!("failed to run tar for server extraction (is tar installed?): {e}"))?;
        if !status.success() {
            return Err(format!("server extraction failed (tar exit: {status})"));
        }
        std::fs::write(&marker, &staged_meta)
            .map_err(|e| format!("failed to write build marker: {e}"))?;
    } else {
        println!("maic-desktop: reusing extracted server runtime");
    }
    Ok(server_dir)
}

/// Read .build-meta.json out of the tarball without extracting it.
fn read_bundled_meta(tarball: &std::path::Path) -> Result<String, String> {
    let out = Command::new("tar")
        .arg("-xzOf")
        .arg(tarball)
        .arg("server/.build-meta.json")
        .output()
        .map_err(|e| format!("failed to inspect server.tar.gz: {e}"))?;
    if !out.status.success() {
        return Err("server.tar.gz has no build meta".to_string());
    }
    String::from_utf8(out.stdout).map_err(|e| format!("bad build meta encoding: {e}"))
}

/// Copy the bundled Node sidecar out of the app bundle into the app data dir.
///
/// Why: on macOS, any executable living inside `.app/Contents/MacOS/` is
/// enrolled by LaunchServices as a Foreground app under our bundle id — and
/// since the server never opens a window, its Dock tile bounces forever.
/// A copy living outside the bundle (plus the ad-hoc signature applied at
/// stage time) registers as BackgroundOnly and stays out of the Dock.
fn ensure_sidecar(app: &tauri::AppHandle, staged_meta: &str) -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("failed to locate app binary: {e}"))?;
    let dir = exe
        .parent()
        .ok_or_else(|| "app binary has no parent dir".to_string())?;
    let bundled: PathBuf = std::fs::read_dir(dir)
        .map_err(|e| format!("failed to list app dir: {e}"))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("openmaic-node") && p != &exe)
                .unwrap_or(false)
        })
        .ok_or_else(|| {
            "bundled node sidecar not found next to the app binary. Rebuild with: node scripts/prepare-server.mjs".to_string()
        })?;

    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("failed to resolve app data dir: {e}"))?;
    let bin_dir = data_dir.join("bin");
    let ext = bundled
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    let staged = bin_dir.join(format!("openmaic-node{ext}"));
    let marker = bin_dir.join(".sidecar-meta.json");
    let current = std::fs::read_to_string(&marker).unwrap_or_default();

    if current != staged_meta || !staged.exists() {
        std::fs::create_dir_all(&bin_dir)
            .map_err(|e| format!("failed to create bin dir: {e}"))?;
        std::fs::copy(&bundled, &staged).map_err(|e| format!("failed to stage sidecar: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perm = std::fs::metadata(&staged)
                .map_err(|e| format!("failed to stat staged sidecar: {e}"))?
                .permissions();
            perm.set_mode(0o755);
            std::fs::set_permissions(&staged, perm)
                .map_err(|e| format!("failed to chmod staged sidecar: {e}"))?;
        }
        std::fs::write(&marker, staged_meta)
            .map_err(|e| format!("failed to write sidecar marker: {e}"))?;
        println!("maic-desktop: staged sidecar outside bundle");
    }
    Ok(staged)
}

fn wait_for_health(port: u16) -> bool {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    while Instant::now() < deadline {
        if std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok() {
            let url = format!("http://127.0.0.1:{port}/api/health");
            if let Ok(true) = http_get_ok(&url) {
                return true;
            }
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    false
}

/// Minimal blocking HTTP GET returning true on 2xx. No extra crates.
fn http_get_ok(url: &str) -> Result<bool, ()> {
    use std::io::{Read, Write};

    let (host, port, path) = parse_url(url).ok_or(())?;
    let mut stream = std::net::TcpStream::connect(format!("{host}:{port}")).map_err(|_| ())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .map_err(|_| ())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(3)))
        .map_err(|_| ())?;
    write!(
        stream,
        "GET {path} HTTP/1.0\r\nHost: {host}\r\nConnection: close\r\n\r\n"
    )
    .map_err(|_| ())?;
    let mut buf = vec![0u8; 4096];
    let mut raw = Vec::new();
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    let head = String::from_utf8_lossy(&raw);
    let status_line = head.lines().next().unwrap_or("");
    Ok(status_line.contains(" 200 ") || status_line.contains(" 200"))
}

fn parse_url(url: &str) -> Option<(String, u16, String)> {
    let rest = url.strip_prefix("http://")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].to_string()),
        None => (rest, "/".to_string()),
    };
    let (host, port) = match authority.rfind(':') {
        Some(i) => (authority[..i].to_string(), authority[i + 1..].parse().ok()?),
        None => (authority.to_string(), 80),
    };
    Some((host, port, path))
}

/// Spawn the sidecar and block until /api/health is green.
/// Returns the base URL plus the child handle (killed on app exit).
fn start_server(
    app: &tauri::AppHandle,
    server_dir: &std::path::Path,
    node_bin: &std::path::Path,
) -> Result<(String, Child), String> {
    let server_js = server_dir.join("server.js");
    if !server_js.exists() {
        return Err(format!(
            "server.js missing at {}. Rebuild with: node scripts/prepare-server.mjs",
            server_js.display()
        ));
    }

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
    let mut last_err = String::new();
    for _ in 0..MAX_PORT_ATTEMPTS {
        let port = match first.take() {
            Some(p) => p,
            None => pick_free_port()?,
        };
        let child = Command::new(node_bin)
            .arg(server_js.to_string_lossy().to_string())
            .env("PORT", port.to_string())
            .env("HOSTNAME", "127.0.0.1")
            .env("NODE_ENV", "production")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();

        match child {
            Ok(mut child) => {
                // Drain pipes so a chatty server log can't block on a full buffer.
                if let Some(out) = child.stdout.take() {
                    std::thread::spawn(move || {
                        use std::io::BufRead;
                        for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
                            println!("[server] {line}");
                        }
                    });
                }
                if let Some(err) = child.stderr.take() {
                    std::thread::spawn(move || {
                        use std::io::BufRead;
                        for line in std::io::BufReader::new(err).lines().map_while(Result::ok) {
                            eprintln!("[server] {line}");
                        }
                    });
                }

                if wait_for_health(port) {
                    // Record the port that actually serves, so the next launch
                    // reuses it and the origin (IndexedDB/localStorage) stays put.
                    if let Ok(data_dir) = app.path().app_data_dir() {
                        let _ = std::fs::write(data_dir.join("server-port.json"), port.to_string());
                    }
                    return Ok((format!("http://127.0.0.1:{port}/"), child));
                }
                last_err = format!("server on port {port} did not become healthy in time");
                let _ = child.kill();
                // Try another port.
            }
            Err(e) => {
                last_err = format!("failed to spawn bundled node (port busy?): {e}");
            }
        }
    }
    Err(format!(
        "could not start the bundled MAIC server after {MAX_PORT_ATTEMPTS} attempts. {last_err}"
    ))
}

/// App state holding the server child so it can be killed on exit.
struct ServerChild(Mutex<Option<Child>>);

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            // Bundled build marker (also used to version the staged sidecar copy).
            let tarball = app
                .path()
                .resolve("resources/server.tar.gz", BaseDirectory::Resource)
                .map(|t| read_bundled_meta(&t).unwrap_or_else(|_| "{}".to_string()))
                .unwrap_or_else(|_| "{}".to_string());
            let server_dir = match ensure_server(app.handle()) {
                Ok(dir) => dir,
                Err(msg) => fatal(app.handle(), &msg),
            };
            let node_bin = match ensure_sidecar(app.handle(), &tarball) {
                Ok(bin) => bin,
                Err(msg) => fatal(app.handle(), &msg),
            };
            let (url, child) = match start_server(app.handle(), &server_dir, &node_bin) {
                Ok(pair) => {
                    println!("maic-desktop: serving {}", pair.0);
                    pair
                }
                Err(msg) => fatal(app.handle(), &msg),
            };
            app.manage(ServerChild(Mutex::new(Some(child))));
            WebviewWindowBuilder::new(app, "main", WebviewUrl::External(url.parse().map_err(
                |e| format!("invalid server url: {e}"),
            )?))
            .title("MAIC Desktop")
            .inner_size(1280.0, 800.0)
            .build()?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::Destroyed = event {
                if let Some(state) = window.app_handle().try_state::<ServerChild>() {
                    if let Ok(mut guard) = state.0.lock() {
                        if let Some(mut child) = guard.take() {
                            let _ = child.kill();
                        }
                    }
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("failed to build maic-desktop")
        .run(|app, event| {
            if let RunEvent::Exit = event {
                if let Some(state) = app.try_state::<ServerChild>() {
                    if let Ok(mut guard) = state.0.lock() {
                        if let Some(mut child) = guard.take() {
                            let _ = child.kill();
                        }
                    }
                }
            }
        });
}
