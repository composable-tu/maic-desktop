//! Server-tree staging: extraction to the app data dir, boot verification,
//! and build provenance.

use std::path::PathBuf;
use std::process::Command;

use tauri::path::BaseDirectory;
use tauri::Manager;

#[cfg(windows)]
use crate::proc::hide_console;
use crate::splash::{splash_status, STATUS_EXTRACTING, STATUS_REUSING, STATUS_VERIFYING};

/// Locate the extracted server tree, extracting server.tar.gz on first launch
/// (or when the bundled build differs from what's on disk).
pub(crate) fn ensure_server(app: &tauri::AppHandle, staged_meta: &str) -> Result<PathBuf, String> {
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
            splash_status(app, STATUS_VERIFYING, None);
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
            "bundled server not found (looked for {} and {}). Rebuild with: pnpm prepare:server",
            dev_dir.join("server.js").display(),
            tarball.display(),
        ));
    }

    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("failed to resolve app data dir: {e}"))?;
    let marker = data_dir.join(".build-meta.json");
    let current = std::fs::read_to_string(&marker).unwrap_or_default();
    let server_dir = data_dir.join("server");

    if current != staged_meta || !server_dir.join("server.js").exists() {
        splash_status(app, STATUS_EXTRACTING, None);
        stage_fresh_server(app, &tarball, &data_dir, &server_dir, staged_meta)?;
    } else {
        println!("maic-desktop: reusing extracted server runtime");
        splash_status(app, STATUS_REUSING, None);
        // A reused tree is not necessarily healthy: files can go missing after
        // staging (cleaner tools, AV quarantine, a crash mid-extraction) while
        // the marker still matches. Re-verify on every launch — cheap — and
        // re-extract once when broken instead of crash-looping the server.
        if let Err(e) = verify_server_tree(&server_dir) {
            eprintln!("maic-desktop: staged server failed validation ({e}); re-extracting");
            splash_status(app, STATUS_EXTRACTING, None);
            stage_fresh_server(app, &tarball, &data_dir, &server_dir, staged_meta)?;
        }
    }
    Ok(server_dir)
}

/// Extract the tarball and stage a fresh server tree, then verify it can boot
/// and record the marker. Used for first launch, updates, and one-shot repair
/// of a damaged staged tree.
pub(crate) fn stage_fresh_server(
    app: &tauri::AppHandle,
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
    splash_status(app, STATUS_VERIFYING, None);
    // Fail fast with a precise message instead of a deep MODULE_NOT_FOUND.
    verify_server_tree(server_dir)?;
    std::fs::write(data_dir.join(".build-meta.json"), staged_meta)
        .map_err(|e| format!("failed to write build marker: {e}"))?;
    Ok(())
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

/// Verify the staged tree can boot: `@swc/helpers` must be reachable the way
/// Node reaches it — by walking up from `next`'s own directory — or the
/// failure surfaces much later as a bare `MODULE_NOT_FOUND` with no hint.
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
    // Deliberately no fallback here: a `@swc/helpers` reachable only through
    // pnpm's `.pnpm/node_modules` hoist bridge means `next` itself cannot
    // resolve its helpers — exactly the tree Node dies on. It must fail.
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

/// Extract the OpenMAIC server version from `.build-meta.json` content.
/// `None` hides the splash version line (old bundles, dev without a staged
/// marker).
pub(crate) fn server_version_from_meta(meta: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(meta)
        .ok()?
        .get("openmaicVersion")?
        .as_str()
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// Read .build-meta.json out of the tarball without extracting it.
pub(crate) fn read_bundled_meta(tarball: &std::path::Path) -> Result<String, String> {
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

#[cfg(test)]
mod verify_tree_tests {
    use super::*;
    use std::fs;

    #[test]
    fn server_version_parses_from_meta() {
        assert_eq!(
            server_version_from_meta(r#"{"openmaicVersion":"1.0.3"}"#).as_deref(),
            Some("1.0.3")
        );
        assert_eq!(server_version_from_meta("{}"), None);
        assert_eq!(server_version_from_meta(r#"{"openmaicVersion":""}"#), None);
        assert_eq!(server_version_from_meta(r#"{"openmaicVersion":42}"#), None);
        assert_eq!(server_version_from_meta("not json"), None);
    }

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
