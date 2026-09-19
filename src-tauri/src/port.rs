//! Loopback port selection for the bundled server.

use std::net::TcpListener;

use tauri::Manager;

/// Preferred loopback port for the bundled server. Fresh installs (and any
/// launch without a usable sticky marker) take this when it is free, so the
/// origin — and therefore IndexedDB/localStorage — is identical across
/// machines. Falls back to a random free port when busy.
const PREFERRED_PORT: u16 = 31846;

/// Ask the OS for a free loopback port. The listener is dropped immediately,
/// so the race window is small; the caller retries with a fresh port on failure.
pub(crate) fn pick_free_port() -> Result<u16, String> {
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

/// Pure decision logic behind `pick_sticky_port`, split out for unit tests.
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
/// data: prefer PREFERRED_PORT when free — which migrates old installs and
/// orphans their previous-origin storage once, the same orphaning that already
/// happens when the sticky port is taken — reuse the recorded sticky port
/// while it is free, otherwise allocate and record a fresh one.
pub(crate) fn pick_sticky_port(app: &tauri::AppHandle) -> Result<u16, String> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
