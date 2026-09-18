// main.rs — MAIC Desktop shell.
//
// Boot flow:
//  1. `setup` (main thread) shows the splash window immediately, then spawns
//     a worker thread for the heavy work below.
//  2. Worker: ensure the bundled server tree is extracted to the app data dir
//     (shipped as server.tar.gz — a plain directory snapshot; it carries no
//     symlinks, so extraction needs no post-processing). Skipped when the
//     staged .build-meta.json already matches.
//  3. Pick a free loopback port from the OS.
//  4. Spawn the bundled Node sidecar running the Next.js standalone server
//     with PORT/HOSTNAME pointed at it.
//  5. Poll /api/health until it responds 200 (or time out with a fatal dialog).
//  6. Close the splash window and open the main window against the local server.
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

/// Preferred loopback port for the bundled server. Fresh installs (and any
/// launch without a usable sticky marker) take this when it is free, so the
/// origin — and therefore IndexedDB/localStorage — is identical across
/// machines. Falls back to a random free port when busy.
const PREFERRED_PORT: u16 = 31846;

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

/// Port choice order. Pure logic, unit-tested:
/// 1. PREFERRED_PORT when free (one stable origin across machines/installs),
/// 2. the recorded sticky port when still free,
/// 3. a fresh random port.
///
/// Note step 1 migrates old installs to the preferred port on next launch;
/// web storage tied to the previous origin is orphaned once (same as today
/// when the sticky port is taken).
fn select_port(sticky: Option<u16>, preferred_free: bool, sticky_free: bool) -> Option<u16> {
    if preferred_free {
        return Some(PREFERRED_PORT);
    }
    if let Some(p) = sticky {
        if p != 0 && sticky_free {
            return Some(p);
        }
    }
    None
}

/// Sticky port selection. Web storage (IndexedDB, localStorage, Cache API) is
/// scoped to the origin, so a random port every launch would orphan all cached
/// data. Prefer PREFERRED_PORT when free; reuse the port recorded in the app
/// data dir when it is still free; otherwise allocate a fresh one and record it.
fn pick_sticky_port(app: &tauri::AppHandle) -> Result<u16, String> {
    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("failed to resolve app data dir: {e}"))?;
    let marker = data_dir.join("server-port.json");

    let mut sticky: Option<u16> = None;
    if let Ok(text) = std::fs::read_to_string(&marker) {
        if let Ok(port) = text.trim().parse::<u16>() {
            sticky = Some(port);
        }
    }
    let sticky_free = sticky.is_some_and(|p| p != 0 && port_is_free(p));

    let port = match select_port(sticky, port_is_free(PREFERRED_PORT), sticky_free) {
        Some(p) => {
            if Some(p) == sticky {
                println!("maic-desktop: reusing sticky port {p}");
            } else {
                println!("maic-desktop: using preferred port {p}");
            }
            p
        }
        None => {
            let fresh = pick_free_port()?;
            println!("maic-desktop: allocated fresh port {fresh}");
            fresh
        }
    };
    // Best effort: a stale marker is harmless (next launch retries).
    let _ =
        std::fs::create_dir_all(&data_dir).and_then(|_| std::fs::write(&marker, port.to_string()));
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
            println!(
                "maic-desktop: using dev server tree at {}",
                dev_dir.display()
            );
            verify_server_tree(&dev_dir)?;
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
        stage_fresh_server(&tarball, &data_dir, &server_dir, &staged_meta)?;
    } else {
        println!("maic-desktop: reusing extracted server runtime");
        // A reused tree is not necessarily healthy: files can go missing after
        // staging (cleaner tools, AV quarantine, a crash mid-extraction) while
        // the marker still matches. Re-verify on every launch — cheap — and
        // re-extract once when broken instead of crash-looping the server.
        if let Err(e) = verify_server_tree(&server_dir) {
            eprintln!("maic-desktop: staged server failed validation ({e}); re-extracting");
            stage_fresh_server(&tarball, &data_dir, &server_dir, &staged_meta)?;
        }
    }
    Ok(server_dir)
}

/// Extract the tarball and stage a fresh server tree, then verify it can boot
/// and record the marker. Used for first launch, updates, and one-shot repair
/// of a damaged staged tree.
fn stage_fresh_server(
    tarball: &std::path::Path,
    data_dir: &std::path::Path,
    server_dir: &std::path::Path,
    staged_meta: &str,
) -> Result<(), String> {
    println!("maic-desktop: extracting server runtime (first launch or update)…");
    std::fs::create_dir_all(data_dir).map_err(|e| format!("failed to create app data dir: {e}"))?;
    // Remove any previous tree so stale files can't shadow the new build.
    // Fail loudly here: extracting over a half-removed tree produces a corrupt
    // server that dies later with a confusing module error.
    if server_dir.exists() {
        std::fs::remove_dir_all(server_dir).map_err(|e| {
            format!(
                "failed to clear previous server at {} (is another instance running?): {e}",
                server_dir.display()
            )
        })?;
    }
    extract_tarball(tarball, data_dir)?;
    // Fail fast with a precise message instead of a deep MODULE_NOT_FOUND.
    verify_server_tree(server_dir)?;
    std::fs::write(data_dir.join(".build-meta.json"), staged_meta)
        .map_err(|e| format!("failed to write build marker: {e}"))?;
    Ok(())
}

/// Suppress the console window for helper child processes on Windows.
///
/// The shell is a GUI-subsystem app (`windows_subsystem = "windows"`), but
/// every console-subsystem child it spawns (`tar.exe`, `cmd.exe`,
/// `openmaic-node.exe`) gets a fresh visible console by default — piping
/// stdio does not prevent that. `CREATE_NO_WINDOW` runs them silently while
/// keeping exit codes and captured output intact. No-op on other platforms.
#[cfg(windows)]
fn hide_console(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

/// Unpack server.tar.gz into `dest`. Captures stderr so failures report the
/// tar backend's own message instead of a bare exit code.
fn extract_tarball(tarball: &std::path::Path, dest: &std::path::Path) -> Result<(), String> {
    let mut cmd = Command::new("tar");
    cmd.arg("-xzf").arg(tarball).arg("-C").arg(dest);
    #[cfg(windows)]
    hide_console(&mut cmd);
    let out = cmd
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

/// Verify the server tree is bootable before spawning: `server.js` exists, the
/// `next` package carries `dist/`, and `@swc/helpers` is reachable the way Node
/// reaches it — by walking up from `next`'s own directory. Without this, a
/// pruned extraction surfaces much later as a bare
/// `Cannot find module '@swc/helpers/_/...'` with no hint what is missing.
fn verify_server_tree(server_dir: &std::path::Path) -> Result<(), String> {
    let server_js = server_dir.join("server.js");
    if !server_js.is_file() {
        return Err(format!(
            "server tree incomplete: {} missing",
            server_js.display()
        ));
    }
    let next_dir = server_dir.join("node_modules").join("next");
    if !next_dir.is_dir() {
        return Err(format!(
            "server tree incomplete: {} missing (extraction incomplete?)",
            next_dir.display()
        ));
    }
    if !next_dir.join("dist").join("server").is_dir() {
        return Err(format!(
            "server tree incomplete: next dist missing under {}",
            next_dir.display()
        ));
    }
    // Deliberately no fallback here: an earlier version also accepted a
    // `@swc/helpers` reached through pnpm's `.pnpm/node_modules` hoist bridge,
    // which let a tree where `next` itself could not resolve its helpers pass
    // verification — precisely the broken bundle that shipped as the Windows
    // MODULE_NOT_FOUND crash.
    let mut helpers_dir = None;
    let mut cur = Some(next_dir.as_path());
    while let Some(dir) = cur {
        let cand = dir.join("node_modules").join("@swc").join("helpers");
        if cand.is_dir() {
            helpers_dir = Some(cand);
            break;
        }
        cur = dir.parent();
    }
    let helpers_dir = helpers_dir.ok_or_else(|| {
        format!(
            "server tree incomplete: @swc/helpers not resolvable by walking up from {} (Node would die with MODULE_NOT_FOUND)",
            next_dir.display()
        )
    })?;
    let pkg_path = helpers_dir.join("package.json");
    let pkg_text = std::fs::read_to_string(&pkg_path).map_err(|_| {
        format!(
            "server tree incomplete: {} has no readable package.json",
            helpers_dir.display()
        )
    })?;
    if !pkg_text.contains("\"./_/_interop_require_default\"") {
        return Err(format!(
            "@swc/helpers exports map missing ./_/_interop_require_default in {}",
            pkg_path.display()
        ));
    }
    let esm = helpers_dir.join("esm").join("_interop_require_default.js");
    let cjs = helpers_dir.join("cjs").join("_interop_require_default.cjs");
    if !esm.is_file() && !cjs.is_file() {
        return Err(format!(
            "@swc/helpers runtime files missing under {} (need esm/_interop_require_default.js or cjs/_interop_require_default.cjs — Next standalone tracing may have pruned them)",
            helpers_dir.display()
        ));
    }
    Ok(())
}

/// Read .build-meta.json out of the tarball without extracting it.
fn read_bundled_meta(tarball: &std::path::Path) -> Result<String, String> {
    let mut cmd = Command::new("tar");
    cmd.arg("-xzOf").arg(tarball).arg("server/.build-meta.json");
    #[cfg(windows)]
    hide_console(&mut cmd);
    let out = cmd
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
    // Exact per-OS filename: the bundler strips the target triple from
    // externalBin binaries and places `<name>[.exe]` beside the app binary.
    // A prefix scan risks grabbing a stale/partial file; fail loudly instead.
    #[cfg(windows)]
    let expected = "openmaic-node.exe";
    #[cfg(not(windows))]
    let expected = "openmaic-node";
    let bundled = dir.join(expected);
    if !bundled.is_file() {
        return Err(format!(
            "bundled node sidecar not found at {} (expected {expected:?} next to the app binary). Rebuild with: node scripts/prepare-server.mjs",
            bundled.display(),
        ));
    }

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

    // A stale-but-marker-matching binary (e.g. quarantined copy that gets
    // SIGKILLed on exec) must not be trusted: probe it before use.
    // Note: --version can pass while server.js gets SIGKILLed (Gatekeeper
    // judges GUI-app children more strictly), so on macOS we additionally
    // re-wash every launch — a local 112MB copy round-trip takes ~1s.
    if current == staged_meta && staged.exists() && !sidecar_is_usable(&staged) {
        eprintln!("maic-desktop: staged sidecar failed exec probe, re-staging");
        let _ = std::fs::remove_file(&staged);
    }
    #[cfg(target_os = "macos")]
    let wash_each_launch = true;
    #[cfg(not(target_os = "macos"))]
    let wash_each_launch = false;

    if current != staged_meta || !staged.exists() {
        std::fs::create_dir_all(&bin_dir).map_err(|e| format!("failed to create bin dir: {e}"))?;
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
        #[cfg(target_os = "macos")]
        {
            // fs::copy preserves the com.apple.provenance marker, and the
            // staged copy gets SIGKILLed on exec. A copy round-trip sheds the
            // enforcement (same trick as prepare-server's staging). The binary
            // is already ad-hoc signed at stage time; the round-trip keeps it.
            let tmp = bin_dir.join(format!("openmaic-node{ext}.stage"));
            std::fs::copy(&staged, &tmp)
                .map_err(|e| format!("failed to wash staged sidecar: {e}"))?;
            std::fs::rename(&tmp, &staged)
                .map_err(|e| format!("failed to wash staged sidecar: {e}"))?;
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
        }
        std::fs::write(&marker, staged_meta)
            .map_err(|e| format!("failed to write sidecar marker: {e}"))?;
        println!("maic-desktop: staged sidecar outside bundle");
    } else if wash_each_launch {
        // Marker matches but Gatekeeper judges GUI-app children per-exec:
        // re-wash so a fresh enforcement decision can't SIGKILL the server.
        // Local copy round-trip, ~1s for 112MB.
        #[cfg(target_os = "macos")]
        {
            let tmp = bin_dir.join(format!("openmaic-node{ext}.stage"));
            if std::fs::copy(&staged, &tmp).is_ok() && std::fs::rename(&tmp, &staged).is_ok() {
                println!("maic-desktop: re-washed staged sidecar");
            }
        }
    }
    Ok(staged)
}

/// Probe whether the staged sidecar can actually be executed (short-lived
/// `--version` run). Catches quarantined/SIGKILLed copies that exist on disk
/// and match the version marker but die instantly when spawned — without
/// this, boot burns all port attempts waiting on a stillborn server.
fn sidecar_is_usable(bin: &std::path::Path) -> bool {
    let mut cmd = Command::new(bin);
    cmd.arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    hide_console(&mut cmd);
    match cmd.output() {
        Ok(out) => out.status.success(),
        Err(_) => false,
    }
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

fn wait_for_health(port: u16, child: &mut Child) -> HealthOutcome {
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
        // The server process already exited (e.g. MODULE_NOT_FOUND on
        // startup): don't burn the full timeout on this attempt. Without
        // this, a crash-on-startup loops 5 attempts x 60s stuck on
        // "Starting local server…" before surfacing the real error.
        // (Health is checked first, so hitching onto another live instance's
        // port still counts as healthy.)
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(_) => break,
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
) -> Result<(String, Child), String> {
    let server_js = server_dir.join("server.js");
    if !server_js.exists() {
        return Err(format!(
            "server.js missing at {}. Rebuild with: node scripts/prepare-server.mjs",
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
            let mut cmd = Command::new(node_bin);
            cmd.arg(server_js.to_string_lossy().to_string())
                .env("PORT", port.to_string())
                .env("HOSTNAME", "127.0.0.1")
                .env("NODE_ENV", "production")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            #[cfg(windows)]
            hide_console(&mut cmd);
            let child = cmd.spawn();

            match child {
                Ok(mut child) => {
                    // Keep the tail of server output: on failure it goes into
                    // the fatal dialog, so a bug report carries the real Node
                    // error.
                    let log: std::sync::Arc<Mutex<String>> =
                        std::sync::Arc::new(Mutex::new(String::new()));
                    // Drain pipes so a chatty server log can't block on a full
                    // buffer.
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
                                let staged_meta = read_bundled_meta(&tarball)
                                    .unwrap_or_else(|_| "{}".to_string());
                                stage_fresh_server(&tarball, &data_dir, server_dir, &staged_meta)?;
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
struct ServerChild(Mutex<Option<Child>>);

/// Heavy boot work off the main thread: extract, stage, serve, then swap
/// the splash window for the real one. Runs on a worker thread; UI updates
/// go through the AppHandle (send-safe).
fn boot_in_background(app: &tauri::AppHandle) {
    // Bundled build marker (also used to version the staged sidecar copy).
    let tarball = app
        .path()
        .resolve("resources/server.tar.gz", BaseDirectory::Resource)
        .map(|t| read_bundled_meta(&t).unwrap_or_else(|_| "{}".to_string()))
        .unwrap_or_else(|_| "{}".to_string());
    splash_status(app, "Preparing local server…");
    let server_dir = match ensure_server(app) {
        Ok(dir) => dir,
        Err(msg) => return boot_failed(app, &msg),
    };
    let node_bin = match ensure_sidecar(app, &tarball) {
        Ok(bin) => bin,
        Err(msg) => return boot_failed(app, &msg),
    };
    splash_status(app, "Starting local server…");
    let (url, child) = match start_server(app, &server_dir, &node_bin) {
        Ok(pair) => {
            println!("maic-desktop: serving {}", pair.0);
            pair
        }
        Err(msg) => return boot_failed(app, &msg),
    };
    app.manage(ServerChild(Mutex::new(Some(child))));
    let parsed: tauri::Url = match url.parse().map_err(|e| format!("invalid server url: {e}")) {
        Ok(u) => u,
        Err(msg) => return boot_failed(app, &msg),
    };
    // Windows MUST be created on the main thread: building the main window
    // here (worker thread) silently fails, leaving no windows at all — the
    // runtime then exits and takes the healthy server down with it. That is
    // exactly the "splash then nothing" symptom.
    let handle = app.clone();
    if let Err(e) = app.run_on_main_thread(move || {
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
    }) {
        boot_failed(app, &format!("failed to schedule main window: {e}"));
    }
}

/// Push a status line to the splash window (best effort). Evals can be dropped
/// while the page is still loading, so the build-target footer rides along with
/// every push: whichever one first reaches the document paints the whole page.
fn splash_status(app: &tauri::AppHandle, text: &str) {
    if let Some(splash) = app.get_webview_window("splash") {
        let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");
        let _ = splash.eval(format!(
            "window.__maicStatus && window.__maicStatus(\"{escaped}\");\
             window.__maicTarget && window.__maicTarget({:?})",
            build_target_label()
        ));
    }
}

/// Name the platform this executable was built for, in the wording the
/// download page uses. Built from compile-time constants, so it reports the
/// installer's target rather than the machine's OS — which is the distinction
/// that matters (an x64 build running under emulation on Windows on ARM keeps
/// saying "x64").
fn target_label(os: &str, arch: &str) -> String {
    let os = match os {
        "macos" => "macOS",
        "windows" => "Windows",
        "linux" => "Linux",
        other => other,
    };
    let arch = match (os, arch) {
        ("macOS", "aarch64") => "Apple Silicon",
        ("macOS", "x86_64") => "Intel Chip",
        (_, "aarch64") => "Arm64",
        (_, "x86_64") => "x86_64",
        (_, other) => other,
    };
    format!("{os} ({arch})")
}

fn build_target_label() -> String {
    target_label(std::env::consts::OS, std::env::consts::ARCH)
}

/// Show the fatal error inside the splash window instead of exiting blindly.
/// The message includes the captured server log tail when available.
fn boot_failed(app: &tauri::AppHandle, message: &str) {
    eprintln!("maic-desktop fatal: {message}");
    if let Some(splash) = app.get_webview_window("splash") {
        // Order matters: show the message first, then the dialog — the user
        // lands on a window that explains the failure either way.
        let escaped = message
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n");
        let _ = splash.eval(format!(
            "window.__maicFatal && window.__maicFatal(\"{escaped}\")"
        ));
        let _ = app
            .dialog()
            .message(message.to_string())
            .title("MAIC Desktop")
            .kind(MessageDialogKind::Error)
            .blocking_show();
        return;
    }
    fatal(app, message);
}

/// Whether destroying the window with this label should tear down the server.
/// Only "main" owns the server lifetime: "splash" closes during the
/// splash -> main handoff while the server must keep running for main.
fn owns_server(label: &str) -> bool {
    label == "main"
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            // Show a splash window immediately: first launch extracts ~200 MB
            // before the server can answer, which used to look like a dead
            // launch (no window for a minute or more).
            // Heavy work runs on a worker thread; the splash navigates to the
            // server once /api/health is green, or shows the fatal error.
            //
            // The page is compiled in and injected as an initialization
            // script: it runs synchronously at document creation, so there is
            // no eval-timing race (eval right after build may be dropped while
            // about:blank isn't ready, which produced the blank window).
            // WebviewUrl::App is avoided: in `tauri dev` it resolves to the
            // dev server (404) and asset-protocol quirks differ per OS.
            // Release: load the compiled-in page through the asset protocol
            // (reliable, no timing race). Dev: WebviewUrl::App would resolve
            // to the Next dev server (404), so use about:blank plus an
            // initialization script that document.writes the same page.
            // Both paths render identical content.
            const SPLASH_HTML: &str = include_str!("../frontend-dist/splash.html");
            // initialization_script takes plain JS: document.write the page.
            // Escape for a JS string literal.
            let mut init_js = String::with_capacity(SPLASH_HTML.len() + 64);
            init_js.push_str("document.open();document.write(\"");
            for c in SPLASH_HTML.chars() {
                match c {
                    '"' => init_js.push_str("\\\""),
                    '\\' => init_js.push_str("\\\\"),
                    '\n' => init_js.push_str("\\n"),
                    '\r' => {}
                    _ => init_js.push(c),
                }
            }
            init_js.push_str("\");document.close();");
            #[cfg(not(debug_assertions))]
            let splash_url = WebviewUrl::App("splash.html".into());
            #[cfg(debug_assertions)]
            let splash_url = WebviewUrl::External(
                "about:blank"
                    .parse()
                    .map_err(|e| format!("bad blank url: {e}"))?,
            );
            let mut builder = WebviewWindowBuilder::new(app, "splash", splash_url)
                .title("MAIC Desktop")
                .inner_size(420.0, 300.0)
                .center()
                .resizable(false)
                .decorations(false)
                .visible(true)
                // Native window background before the webview paints its first
                // frame (covers the white flash, esp. on WebView2).
                .background_color(tauri::window::Color(0, 0, 0, 255));
            #[cfg(debug_assertions)]
            {
                builder = builder.initialization_script(&init_js);
            }
            builder.build()?;

            let handle = app.handle().clone();
            std::thread::spawn(move || boot_in_background(&handle));
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::Destroyed = event {
                // Splash closes during the splash -> main handoff while the
                // server must stay alive for the main window. Only tear the
                // server down when main itself is destroyed (RunEvent::Exit
                // below is the final backup). Without this guard, closing
                // the splash kills the healthy server and main opens blank.
                if !owns_server(window.label()) {
                    return;
                }
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

    #[test]
    fn only_main_owns_server_lifetime() {
        assert!(owns_server("main"));
        // Closing any other window (notably the splash during the
        // splash -> main handoff) must not kill the server.
        assert!(!owns_server("splash"));
        assert!(!owns_server(""));
    }

    #[test]
    fn target_label_reads_like_a_download_page() {
        assert_eq!(target_label("macos", "aarch64"), "macOS (Apple Silicon)");
        assert_eq!(target_label("macos", "x86_64"), "macOS (Intel)");
        assert_eq!(target_label("windows", "x86_64"), "Windows (x64)");
        assert_eq!(target_label("windows", "aarch64"), "Windows (ARM64)");
        assert_eq!(target_label("linux", "x86_64"), "Linux (x64)");
        // An arch nobody has named yet stays literal rather than being guessed.
        assert_eq!(target_label("linux", "arm"), "Linux (arm)");
        // Tripwire: a new build target must be given human wording here, not
        // leak a raw triple fragment into the splash.
        let live = build_target_label();
        assert!(
            live.contains('(') && !live.contains("x86_64") && !live.contains("aarch64"),
            "unmapped target label: {live}"
        );
    }

    #[test]
    fn preferred_port_wins_over_sticky() {
        // Free preferred port always wins (migrates old installs to 31846).
        assert_eq!(select_port(None, true, false), Some(PREFERRED_PORT));
        assert_eq!(select_port(Some(1234), true, true), Some(PREFERRED_PORT));
        // Busy preferred: reuse the sticky port while free, else fresh.
        assert_eq!(select_port(Some(53588), false, true), Some(53588));
        assert_eq!(select_port(Some(0), false, false), None);
        assert_eq!(select_port(Some(53588), false, false), None);
        assert_eq!(select_port(None, false, false), None);
        assert_eq!(PREFERRED_PORT, 31846);
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

#[cfg(test)]
mod verify_tree_tests {
    use super::*;
    use std::fs;

    #[test]
    fn verify_passes_on_complete_tree() {
        let dir = std::env::temp_dir().join(format!("maic-verify-ok-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // The shipped tree is hoisted and link-free: `next` sits in
        // node_modules/next and finds its helpers in the same node_modules.
        fs::write(dir.join("server.js"), "x").unwrap();
        let next = dir.join("node_modules").join("next");
        fs::create_dir_all(next.join("dist").join("server")).unwrap();
        let helpers = dir.join("node_modules").join("@swc").join("helpers");
        fs::create_dir_all(helpers.join("cjs")).unwrap();
        fs::write(
            helpers.join("package.json"),
            r#"{"exports": {"./_/_interop_require_default": {"default": "./cjs/x.cjs"}}}"#,
        )
        .unwrap();
        fs::write(
            helpers.join("cjs").join("_interop_require_default.cjs"),
            "c",
        )
        .unwrap();
        assert!(verify_server_tree(&dir).is_ok());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_fails_without_next() {
        let dir = std::env::temp_dir().join(format!("maic-verify-no-next-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("server.js"), "x").unwrap();
        let err = verify_server_tree(&dir).unwrap_err();
        assert!(err.contains("next"), "bad error: {err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_fails_without_helpers_files() {
        let dir = std::env::temp_dir().join(format!("maic-verify-no-hlp-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("server.js"), "x").unwrap();
        let next = dir.join("node_modules").join("next");
        fs::create_dir_all(next.join("dist").join("server")).unwrap();
        let helpers = dir.join("node_modules").join("@swc").join("helpers");
        fs::create_dir_all(&helpers).unwrap();
        fs::write(helpers.join("package.json"), r#"{"exports": {}}"#).unwrap();
        let err = verify_server_tree(&dir).unwrap_err();
        assert!(err.contains("@swc/helpers"), "bad error: {err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_fails_on_the_broken_windows_tree() {
        // Regression guard for the shipped-Windows crash: `node_modules/next`
        // was a real directory (its link never reached the manifest), so Node
        // resolved it lexically and never walked into the pnpm store where its
        // helpers live. Verification passed anyway because it accepted the
        // `.pnpm/node_modules` hoist bridge. It must fail.
        let dir = std::env::temp_dir().join(format!("maic-verify-bridge-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("server.js"), "x").unwrap();
        let next = dir.join("node_modules").join("next");
        fs::create_dir_all(next.join("dist").join("server")).unwrap();
        let bridge = dir
            .join("node_modules")
            .join(".pnpm")
            .join("node_modules")
            .join("@swc")
            .join("helpers");
        fs::create_dir_all(bridge.join("cjs")).unwrap();
        fs::write(
            bridge.join("package.json"),
            r#"{"exports": {"./_/_interop_require_default": {"default": "./cjs/x.cjs"}}}"#,
        )
        .unwrap();
        fs::write(bridge.join("cjs").join("_interop_require_default.cjs"), "c").unwrap();
        let err = verify_server_tree(&dir).unwrap_err();
        assert!(
            err.contains("@swc/helpers") && err.contains("not resolvable"),
            "bad error: {err}"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
