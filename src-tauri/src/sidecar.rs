//! Node sidecar staging: a copy outside the .app bundle, probed and washed
//! for Gatekeeper/LaunchServices on macOS.

use std::path::PathBuf;
use std::process::Command;

use tauri::Manager;

#[cfg(windows)]
use crate::proc::hide_console;
use crate::splash::{splash_status, STATUS_STAGING};

#[cfg(unix)]
fn make_executable(path: &std::path::Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mut perm = std::fs::metadata(path)
        .map_err(|e| format!("failed to stat staged sidecar: {e}"))?
        .permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(path, perm)
        .map_err(|e| format!("failed to chmod staged sidecar: {e}"))?;
    Ok(())
}

/// Copy the bundled Node sidecar out of the app bundle into the app data dir.
///
/// Why: on macOS, any executable living inside `.app/Contents/MacOS/` is
/// enrolled by LaunchServices as a Foreground app under our bundle id — and
/// since the server never opens a window, its Dock tile bounces forever.
/// A copy living outside the bundle registers as BackgroundOnly and stays
/// out of the Dock.
pub(crate) fn ensure_sidecar(app: &tauri::AppHandle, staged_meta: &str) -> Result<PathBuf, String> {
    splash_status(app, STATUS_STAGING, None);
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
            "bundled node sidecar not found at {} (expected {expected:?} next to the app binary). Rebuild with: pnpm prepare:server",
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
        make_executable(&staged)?;
        #[cfg(target_os = "macos")]
        {
            // fs::copy preserves the com.apple.provenance marker, and the
            // staged copy gets SIGKILLed on exec. A copy round-trip sheds the
            // enforcement (same trick as prepare-server's staging). The binary
            // keeps its official Node.js signature; the round-trip preserves it.
            let tmp = bin_dir.join(format!("openmaic-node{ext}.stage"));
            std::fs::copy(&staged, &tmp)
                .map_err(|e| format!("failed to wash staged sidecar: {e}"))?;
            std::fs::rename(&tmp, &staged)
                .map_err(|e| format!("failed to wash staged sidecar: {e}"))?;
            make_executable(&staged)?;
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
