//! Splash window and boot-stage reporting. The stage wording lives in
//! `STRINGS` inside splash.html so it can follow the webview's locale; the
//! shell only carries the key.

use tauri::{App, Manager, WebviewUrl, WebviewWindowBuilder};

pub(crate) const STATUS_CHECKING: &str = "checking";
pub(crate) const STATUS_EXTRACTING: &str = "extracting";
pub(crate) const STATUS_REUSING: &str = "reusing";
pub(crate) const STATUS_VERIFYING: &str = "verifying";
pub(crate) const STATUS_STAGING: &str = "staging";
pub(crate) const STATUS_PROBING: &str = "probing";

/// Escape a string for embedding inside a JS string literal: raw quotes,
/// backslashes, or line terminators would break the literal.
pub(crate) fn js_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ => out.push(c),
        }
    }
    out
}

/// Show the splash window before anything else can go wrong. First launch
/// extracts ~200 MB before the server can answer, and that period needs a
/// visible UI.
///
/// The page is compiled in and reaches the document two ways that render
/// identical content: release loads it through the asset protocol; dev uses
/// about:blank plus an initialization script that document.writes the page
/// (WebviewUrl::App would resolve to the Next dev server in `tauri dev`, a
/// 404). The initialization script runs synchronously at document creation,
/// so no separate eval is needed and it cannot race the page load.
pub(crate) fn create_splash_window(app: &App) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(debug_assertions)]
    const SPLASH_HTML: &str = include_str!("../frontend-dist/splash.html");
    #[cfg(debug_assertions)]
    let init_js = format!(
        "document.open();document.write({});document.close();",
        js_string(SPLASH_HTML)
    );
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
    Ok(())
}

/// Push a boot stage to the splash window (best effort). Evals can be dropped
/// while the page is still loading, so the build-target footer rides along with
/// every push: whichever one first reaches the document paints the whole page.
pub(crate) fn splash_status(app: &tauri::AppHandle, key: &str, port: Option<u16>) {
    if let Some(splash) = app.get_webview_window("splash") {
        let port_js = match port {
            Some(p) => format!("\"{p}\""),
            None => "undefined".to_string(),
        };
        let _ = splash.eval(format!(
            "window.__maicStatus && window.__maicStatus({},{});\
             window.__maicTarget && window.__maicTarget({})",
            js_string(key),
            port_js,
            js_string(&build_target_label())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splash_defines_every_status_key_the_shell_sends() {
        // The wording lives in splash.html and the shell only carries keys, so a
        // rename on one side alone would silently print a raw key in the window.
        const SPLASH: &str = include_str!("../frontend-dist/splash.html");
        for key in [
            STATUS_CHECKING,
            STATUS_EXTRACTING,
            STATUS_REUSING,
            STATUS_VERIFYING,
            STATUS_STAGING,
            STATUS_PROBING,
            "failed",
        ] {
            assert!(
                SPLASH.contains(&format!("{key}:")),
                "splash.html has no STRINGS entry for {key:?}"
            );
        }
        // `probing` is the one parameterized stage: both locales need the slot,
        // or the splash prints a literal "{port}". The page's own substitution
        // call mentions it as well, so that is three occurrences.
        assert!(
            SPLASH.matches("{port}").count() >= 3,
            "probing lost its {{port}} slot"
        );
    }

    #[test]
    fn target_label_names_the_build_target() {
        assert_eq!(target_label("macos", "aarch64"), "macOS (Apple Silicon)");
        assert_eq!(target_label("macos", "x86_64"), "macOS (Intel Chip)");
        assert_eq!(target_label("windows", "x86_64"), "Windows (x86_64)");
        assert_eq!(target_label("windows", "aarch64"), "Windows (Arm64)");
        assert_eq!(target_label("linux", "x86_64"), "Linux (x86_64)");
        // An arch nobody has named yet is reported as the raw constant.
        assert_eq!(target_label("linux", "arm"), "Linux (arm)");
        let live = build_target_label();
        assert!(
            live.starts_with("macOS (")
                || live.starts_with("Windows (")
                || live.starts_with("Linux ("),
            "unrecognised target family: {live}"
        );
    }
}
