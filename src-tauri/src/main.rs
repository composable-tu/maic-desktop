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
        extract_tarball(&tarball, &data_dir)?;
        #[cfg(windows)]
        restore_links(&server_dir)?;
        std::fs::write(&marker, &staged_meta)
            .map_err(|e| format!("failed to write build marker: {e}"))?;
    } else {
        println!("maic-desktop: reusing extracted server runtime");
    }
    Ok(server_dir)
}

/// Unpack server.tar.gz into `dest`. Captures stderr so failures report the
/// tar backend's own message instead of a bare exit code.
fn extract_tarball(tarball: &std::path::Path, dest: &std::path::Path) -> Result<(), String> {
    let out = Command::new("tar")
        .arg("-xzf")
        .arg(tarball)
        .arg("-C")
        .arg(dest)
        .output()
        .map_err(|e| format!("failed to run tar for server extraction: {e}"))?;
    if !out.status.success() {
        let detail = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(format!(
            "server extraction failed (tar exit: {}{})",
            out.status,
            if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            },
        ));
    }
    Ok(())
}

/// Restore symlinks from the server/.links.json manifest.
///
/// Background: the Windows tar backend (bsdtar) silently drops symlink
/// entries, so the extracted tree is missing the pnpm isolated-deps links
/// Node needs (e.g. node_modules/next -> .pnpm/…). Plain symlinks require
/// privileges on Windows, but directory junctions (`mklink /J`) do not —
/// and Node resolves junctions the same way. All manifest links point
/// inside the server tree; file links are materialized as plain copies.
#[cfg(windows)]
fn restore_links(server_dir: &std::path::Path) -> Result<(), String> {
    let manifest_path = server_dir.join(".links.json");
    let text = std::fs::read_to_string(&manifest_path).map_err(|e| {
        format!(
            "link manifest missing at {}: {e}",
            manifest_path.display()
        )
    })?;
    let links = parse_links_manifest(&text)?;
    let mut restored = 0u32;
    let mut copied = 0u32;
    for (link_rel, target_rel) in links {
        let link = join_rel(server_dir, &link_rel)?;
        let target = join_rel(server_dir, &target_rel)?;
        // tar may have materialized the entry as a real file/dir already.
        if link.exists() || std::fs::symlink_metadata(&link).is_ok() {
            continue;
        }
        if let Some(parent) = link.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
        }
        match link_plan(&link, &target)? {
            LinkAction::Junction => {
                // Junctions need no privileges; use an absolute target so the
                // link survives regardless of the process working directory.
                let status = Command::new("cmd")
                    .args(["/C", "mklink", "/J"])
                    .arg(&link)
                    .arg(&target)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::piped())
                    .status()
                    .map_err(|e| format!("failed to run mklink for {}: {e}", link.display()))?;
                if !status.success() {
                    return Err(format!(
                        "failed to create junction {} -> {} (mklink exit: {status})",
                        link.display(),
                        target.display()
                    ));
                }
                restored += 1;
            }
            LinkAction::CopyFile => {
                std::fs::copy(&target, &link).map_err(|e| {
                    format!(
                        "failed to materialize {} from {}: {e}",
                        link.display(),
                        target.display()
                    )
                })?;
                copied += 1;
            }
        }
    }
    println!("maic-desktop: restored {restored} junctions, materialized {copied} files");
    Ok(())
}

/// How a manifest link should be restored on disk.
#[cfg(any(windows, test))]
#[derive(Debug, PartialEq, Eq)]
enum LinkAction {
    /// Target is a directory: create a junction.
    Junction,
    /// Target is a file: copy its bytes.
    CopyFile,
}

/// Classify a (link, target) pair. Pure logic, unit-tested on all platforms.
#[cfg(any(windows, test))]
fn link_plan(link: &std::path::Path, target: &std::path::Path) -> Result<LinkAction, String> {
    // tar may have materialized the entry as a real file/dir already.
    if link.exists() || std::fs::symlink_metadata(link).is_ok() {
        return Err(format!("link already exists: {}", link.display()));
    }
    if target.is_dir() {
        Ok(LinkAction::Junction)
    } else if target.is_file() {
        Ok(LinkAction::CopyFile)
    } else {
        Err(format!(
            "link target missing: {} -> {}",
            link.display(),
            target.display()
        ))
    }
}

/// Parse the .links.json manifest into (link, target) pairs.
/// Minimal hand parser: entries are exactly {"link": "…", "target": "…"}.
/// (No serde_json Value parsing: keeps the manifest path dependency-free.)
#[cfg(any(windows, test))]
fn parse_links_manifest(text: &str) -> Result<Vec<(String, String)>, String> {
    fn unescape(s: &str) -> Result<String, String> {
        let mut out = String::with_capacity(s.len());
        let mut it = s.chars();
        while let Some(c) = it.next() {
            if c != '\\' {
                out.push(c);
                continue;
            }
            match it.next() {
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('/') => out.push('/'),
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('u') => {
                    let hex: String = it.by_ref().take(4).collect();
                    let cp = u32::from_str_radix(&hex, 16)
                        .map_err(|_| format!("bad \\u escape in {s:?}"))?;
                    out.push(char::from_u32(cp).ok_or_else(|| format!("bad codepoint in {s:?}"))?);
                }
                other => return Err(format!("bad escape in {s:?}: {other:?}")),
            }
        }
        Ok(out)
    }

    fn field(obj: &str, key: &str) -> Result<String, String> {
        let needle = format!("\"{key}\"");
        let k = obj
            .find(&needle)
            .ok_or_else(|| format!("entry missing {key}: {obj:?}"))?;
        let rest = obj[k + needle.len()..].trim_start();
        let rest = rest
            .strip_prefix(':')
            .ok_or_else(|| format!("entry missing colon after {key}: {obj:?}"))?
            .trim_start();
        let body = rest
            .strip_prefix('"')
            .ok_or_else(|| format!("entry {key} is not a string: {obj:?}"))?;
        let mut end = None;
        let mut prev_backslash = false;
        for (i, c) in body.char_indices() {
            if c == '"' && !prev_backslash {
                end = Some(i);
                break;
            }
            prev_backslash = c == '\\' && !prev_backslash;
        }
        let end = end.ok_or_else(|| format!("unterminated {key}: {obj:?}"))?;
        unescape(&body[..end])
    }

    let text = text.trim();
    if !text.starts_with('[') || !text.ends_with(']') {
        return Err("link manifest is not a JSON array".to_string());
    }
    // Split top-level {...} objects (manifest entries are flat).
    let mut entries = Vec::new();
    let mut depth = 0usize;
    let mut in_str = false;
    let mut prev_backslash = false;
    let mut start = None;
    for (i, c) in text.char_indices() {
        if in_str {
            if c == '"' && !prev_backslash {
                in_str = false;
            }
            prev_backslash = c == '\\' && !prev_backslash;
            continue;
        }
        match c {
            '"' => {
                in_str = true;
                prev_backslash = false;
            }
            '{' => {
                if depth == 0 {
                    start = Some(i);
                }
                depth += 1;
            }
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    if let Some(s) = start.take() {
                        entries.push(&text[s..=i]);
                    }
                }
            }
            _ => {}
        }
    }
    entries
        .into_iter()
        .map(|e| Ok((field(e, "link")?, field(e, "target")?)))
        .collect()
}

/// Join a manifest-relative POSIX path onto a base dir, rejecting escapes.
#[cfg(any(windows, test))]
fn join_rel(base: &std::path::Path, rel: &str) -> Result<PathBuf, String> {
    if rel.is_empty() {
        return Err("empty path in link manifest".to_string());
    }
    let mut out = base.to_path_buf();
    for part in rel.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return Err(format!("unsafe path in link manifest: {rel:?}"));
        }
        // Reject Windows-absolute paths and drive prefixes smuggled in.
        if part.contains(':') || part.contains('\\') {
            return Err(format!("unsafe path in link manifest: {rel:?}"));
        }
        out.push(part);
    }
    Ok(out)
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

/// Outcome of waiting for the server to become healthy.
#[derive(Debug, PartialEq, Eq)]
enum HealthOutcome {
    /// /api/health answered 200.
    Healthy,
    /// Something accepted TCP connections but /api/health never went green.
    ListeningButUnhealthy,
    /// Nothing ever accepted TCP connections — the server likely exited.
    NoListener,
}

fn wait_for_health(port: u16) -> HealthOutcome {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    let mut ever_listened = false;
    while Instant::now() < deadline {
        if std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok() {
            ever_listened = true;
            let url = format!("http://127.0.0.1:{port}/api/health");
            if let Ok(true) = http_get_ok(&url) {
                return HealthOutcome::Healthy;
            }
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    if ever_listened {
        HealthOutcome::ListeningButUnhealthy
    } else {
        HealthOutcome::NoListener
    }
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
                // Keep the tail of server output: on failure it goes into the
                // fatal dialog, so a bug report carries the real Node error.
                let log: std::sync::Arc<Mutex<String>> =
                    std::sync::Arc::new(Mutex::new(String::new()));
                // Drain pipes so a chatty server log can't block on a full buffer.
                if let Some(out) = child.stdout.take() {
                    let log = std::sync::Arc::clone(&log);
                    std::thread::spawn(move || {
                        use std::io::BufRead;
                        for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
                            println!("[server] {line}");
                            push_log(&log, &line);
                        }
                    });
                }
                if let Some(err) = child.stderr.take() {
                    let log = std::sync::Arc::clone(&log);
                    std::thread::spawn(move || {
                        use std::io::BufRead;
                        for line in std::io::BufReader::new(err).lines().map_while(Result::ok) {
                            eprintln!("[server] {line}");
                            push_log(&log, &line);
                        }
                    });
                }

                match wait_for_health(port) {
                    HealthOutcome::Healthy => {
                        // Record the port that actually serves, so the next launch
                        // reuses it and the origin (IndexedDB/localStorage) stays put.
                        if let Ok(data_dir) = app.path().app_data_dir() {
                            let _ = std::fs::write(data_dir.join("server-port.json"), port.to_string());
                        }
                        return Ok((format!("http://127.0.0.1:{port}/"), child));
                    }
                    outcome => {
                        // Give the drain threads a moment to flush the exit error.
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
                        // Try another port.
                    }
                }
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

/// Append a line to the shared tail buffer, keeping roughly the last 4 KB.
fn push_log(log: &Mutex<String>, line: &str) {
    if let Ok(mut guard) = log.lock() {
        guard.push_str(line);
        guard.push('\n');
        const KEEP: usize = 4096;
        if guard.len() > KEEP * 2 {
            let drop = guard.len() - KEEP;
            let cut = guard[drop..].find('\n').map(|i| drop + i + 1).unwrap_or(drop);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn parse_manifest_round_trip() {
        let text = r#"[
  {"link": "node_modules/next", "target": ".pnpm/next@16.3.3/node_modules/next"},
  {"link": "node_modules/.pnpm/node_modules/has \"quote\"\\x", "target": "a/b"}
]"#;
        let pairs = parse_links_manifest(text).expect("parse");
        assert_eq!(pairs.len(), 2);
        assert_eq!(
            pairs[0],
            (
                "node_modules/next".to_string(),
                ".pnpm/next@16.3.3/node_modules/next".to_string()
            )
        );
        assert_eq!(pairs[1].0, "node_modules/.pnpm/node_modules/has \"quote\"\\x");
    }

    #[test]
    fn parse_manifest_rejects_garbage() {
        assert!(parse_links_manifest("not json").is_err());
        assert!(parse_links_manifest("[{}]").is_err());
        assert!(parse_links_manifest(r#"[{"link": 1, "target": "x"}]"#).is_err());
        assert!(parse_links_manifest("[]").expect("empty").is_empty());
    }

    #[test]
    fn join_rel_blocks_escapes() {
        let base = std::path::Path::new("/data/server");
        assert_eq!(
            join_rel(base, "node_modules/next").unwrap(),
            base.join("node_modules/next")
        );
        for evil in ["", ".", "..", "a/../../etc", "C:/win", "a\\b", "a:b"] {
            assert!(join_rel(base, evil).is_err(), "should reject {evil:?}");
        }
    }

    #[test]
    fn link_plan_classifies_targets() {
        let dir = std::env::temp_dir().join(format!("maic-plan-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("store/pkg")).unwrap();
        fs::write(dir.join("store/pkg/index.js"), "x").unwrap();

        // Directory target -> junction.
        let plan = link_plan(&dir.join("node_modules/next"), &dir.join("store/pkg"));
        assert!(plan.is_ok(), "plan failed: {plan:?}");
        assert_eq!(plan.unwrap(), LinkAction::Junction);

        // File target -> copy.
        let plan = link_plan(&dir.join("node_modules/a.js"), &dir.join("store/pkg/index.js"));
        assert!(plan.is_ok(), "plan failed: {plan:?}");
        assert_eq!(plan.unwrap(), LinkAction::CopyFile);

        // Missing target -> error mentioning both paths.
        let err = link_plan(&dir.join("node_modules/gone"), &dir.join("store/nope")).unwrap_err();
        assert!(err.contains("gone") && err.contains("nope"), "bad error: {err}");

        // Existing link path -> error (tar already materialized it).
        fs::write(dir.join("node_modules_taken"), "y").unwrap_or_else(|_| {
            fs::create_dir_all(dir.join("nm")).unwrap();
            fs::write(dir.join("nm/taken"), "y").unwrap();
        });
        let taken = if dir.join("node_modules_taken").exists() {
            dir.join("node_modules_taken")
        } else {
            dir.join("nm/taken")
        };
        assert!(link_plan(&taken, &dir.join("store/pkg")).is_err());

        let _ = fs::remove_dir_all(&dir);
    }
}


#[cfg(test)]
mod log_tail_tests {
    use super::*;

    #[test]
    fn tail_keeps_last_lines() {
        let log = Mutex::new(String::new());
        for i in 0..200 {
            push_log(&log, &format!("line {i:03} padding-padding-padding-padding"));
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
