//! Splash window and boot-stage reporting. The stage wording lives in
//! `STRINGS` inside splash.html so it can follow the webview's locale; the
//! shell only carries the key.

use std::sync::OnceLock;

use tauri::{App, Manager, WebviewUrl, WebviewWindowBuilder};

pub(crate) const STATUS_CHECKING: &str = "checking";
pub(crate) const STATUS_EXTRACTING: &str = "extracting";
pub(crate) const STATUS_REUSING: &str = "reusing";
pub(crate) const STATUS_VERIFYING: &str = "verifying";
pub(crate) const STATUS_STAGING: &str = "staging";
pub(crate) const STATUS_PROBING: &str = "probing";

/// Render `s` as a complete JavaScript string literal — including the
/// surrounding quotes — with quotes, backslashes, and line terminators
/// escaped. Callers splice the result straight into generated JS; a version
/// that returned only the escaped content kept producing SyntaxErrors
/// whenever a splice site forgot the quotes.
pub(crate) fn js_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ => out.push(c),
        }
    }
    out.push('"');
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

/// The bundled OpenMAIC server version, set once from the build marker
/// before the first status push; `None` leaves the splash version line
/// empty (old bundles, dev without a staged marker).
static SERVER_VERSION: OnceLock<String> = OnceLock::new();

pub(crate) fn set_server_version(version: Option<String>) {
    if let Some(v) = version {
        let _ = SERVER_VERSION.set(v);
    }
}

/// Push a boot stage to the splash window (best effort). Evals can be dropped
/// while the page is still loading, so the footer (server version and build
/// target) rides along with every push: whichever one first reaches the
/// document paints the whole page.
pub(crate) fn splash_status(app: &tauri::AppHandle, key: &str, port: Option<u16>) {
    if let Some(splash) = app.get_webview_window("splash") {
        let port_js = match port {
            Some(p) => js_string(&p.to_string()),
            None => "undefined".to_string(),
        };
        let version_js = match SERVER_VERSION.get() {
            Some(v) => format!(
                "window.__maicVersion && window.__maicVersion({});",
                js_string(v)
            ),
            None => String::new(),
        };
        let _ = splash.eval(format!(
            "{version_js}\
             window.__maicStatus && window.__maicStatus({},{});\
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

/// Insert the runtime OS version after the OS name: the arch half stays the
/// build target, the version says which OS release the app runs on.
fn with_version(label: &str, version: Option<&str>) -> String {
    match version {
        Some(v) => match label.split_once(" (") {
            // `arch` keeps the label's own closing paren.
            Some((os, arch)) => format!("{os} {v} ({arch}"),
            None => format!("{label} {v}"),
        },
        None => label.to_string(),
    }
}

fn build_target_label() -> String {
    with_version(
        &target_label(std::env::consts::OS, std::env::consts::ARCH),
        os_version().as_deref(),
    )
}

/// Best-effort runtime OS version for the footer. Cached: the label is
/// rebuilt on every status push.
fn os_version() -> Option<String> {
    static CACHE: OnceLock<Option<String>> = OnceLock::new();
    CACHE.get_or_init(compute_os_version).clone()
}

fn compute_os_version() -> Option<String> {
    match std::env::consts::OS {
        "macos" => true_macos_version(),
        "windows" => {
            let (major, build) = true_windows_version()?;
            // The caller contract guarantees major == 10 (Windows 10/11);
            // anything older keeps the plain label.
            (major == 10).then(|| windows_marketing_version(build).to_string())
        }
        // Other dev targets have no shipped bundles; keep the plain label.
        _ => None,
    }
}

/// macOS version via `sysctlbyname("kern.osproductversion")` — the same
/// value `sw_vers -productVersion` prints, read directly from libSystem
/// without spawning a process.
#[cfg(target_os = "macos")]
fn true_macos_version() -> Option<String> {
    use std::ffi::c_char;

    const NAME: &[u8] = b"kern.osproductversion\0";
    let mut buf = [0u8; 16];
    let mut len = buf.len();
    // SAFETY: `name` is NUL-terminated, and `buf`/`len` describe writable
    // space far larger than any version string.
    if unsafe {
        libc::sysctlbyname(
            NAME.as_ptr().cast::<c_char>(),
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(len);
    sanitized_version(std::str::from_utf8(&buf[..end]).ok()?)
}

#[cfg(not(target_os = "macos"))]
fn true_macos_version() -> Option<String> {
    None
}

/// True Windows version via `RtlGetVersion` — the one API that does not lie:
/// `GetVersionExW` reports 6.2 from Windows 8.1 onward unless the manifest
/// opts in, and `cmd /c ver` output is localized (zh-CN prints 版本). No
/// process spawn.
#[cfg(windows)]
fn true_windows_version() -> Option<(u32, u32)> {
    use windows_sys::Wdk::System::SystemServices::RtlGetVersion;
    use windows_sys::Win32::System::SystemInformation::OSVERSIONINFOW;

    let mut info = OSVERSIONINFOW {
        dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32,
        dwMajorVersion: 0,
        dwMinorVersion: 0,
        dwBuildNumber: 0,
        dwPlatformId: 0,
        szCSDVersion: [0; 128],
    };
    // SAFETY: `info` is a valid, correctly sized OSVERSIONINFOW; the call
    // only fills the struct.
    if unsafe { RtlGetVersion(&mut info) } == 0 {
        Some((info.dwMajorVersion, info.dwBuildNumber))
    } else {
        None
    }
}

#[cfg(not(windows))]
fn true_windows_version() -> Option<(u32, u32)> {
    None
}

/// Build 22000 is where Windows 11 starts; below it, Windows 10.
fn windows_marketing_version(build: u32) -> &'static str {
    if build >= 22000 {
        "11"
    } else {
        "10"
    }
}

/// Trust only digits and dots — this text is spliced into the footer label.
fn sanitized_version(text: &str) -> Option<String> {
    let v = text.trim();
    (!v.is_empty() && v.chars().all(|c| c.is_ascii_digit() || c == '.')).then(|| v.to_string())
}

/// Map `cmd /c ver` output ("Microsoft Windows [Version 10.0.26100.2314]") to
/// the marketing version every Windows user recognises.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_string_produces_a_quoted_literal() {
        assert_eq!(js_string("plain"), "\"plain\"");
        // Quotes, backslashes, and line terminators are escaped, so the
        // spliced result stays a single valid JS literal — an unquoted
        // splice site here turned every shell eval into a SyntaxError.
        assert_eq!(js_string("a\"b\\c\nd\re"), "\"a\\\"b\\\\c\\nd\\re\"");
        assert!(!js_string("a\nb").contains('\n'));
    }

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
            (live.starts_with("macOS") || live.starts_with("Windows") || live.starts_with("Linux"))
                && live.contains(" ("),
            "unrecognised target family: {live}"
        );
    }

    #[test]
    fn with_version_inserts_the_runtime_os_version() {
        assert_eq!(
            with_version("macOS (Apple Silicon)", Some("15.1")),
            "macOS 15.1 (Apple Silicon)"
        );
        assert_eq!(
            with_version("Windows (x86_64)", Some("11")),
            "Windows 11 (x86_64)"
        );
        // No version available: the build-target label stands alone.
        assert_eq!(
            with_version("macOS (Apple Silicon)", None),
            "macOS (Apple Silicon)"
        );
        assert_eq!(with_version("Linux (arm)", None), "Linux (arm)");
    }

    #[test]
    fn os_version_inputs_are_sanitized() {
        assert_eq!(sanitized_version("15.1\n"), Some("15.1".into()));
        assert_eq!(sanitized_version("15.1.1"), Some("15.1.1".into()));
        assert_eq!(sanitized_version(""), None);
        assert_eq!(sanitized_version("rm -rf /"), None);
        assert_eq!(sanitized_version("1;2"), None);
    }

    #[test]
    fn windows_marketing_version_maps_the_build() {
        assert_eq!(windows_marketing_version(26100), "11");
        assert_eq!(windows_marketing_version(22000), "11");
        assert_eq!(windows_marketing_version(19045), "10");
        assert_eq!(windows_marketing_version(10240), "10");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_version_is_discovered_live() {
        // The sysctl read runs against the real host; the sanitized digits
        // guarantee the label stays splice-safe.
        let v = true_macos_version().expect("kern.osproductversion must resolve on macOS");
        assert!(v.chars().all(|c| c.is_ascii_digit() || c == '.'));
    }
}
