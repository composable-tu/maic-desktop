//! Health probing for the bundled local server.

use std::process::Child;
use std::time::{Duration, Instant};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Outcome of waiting for the server to become healthy.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum HealthOutcome {
    /// /api/health answered 200.
    Healthy,
    /// Something accepted TCP connections but /api/health never went green.
    ListeningButUnhealthy,
    /// Nothing ever accepted TCP connections — the server likely exited.
    NoListener,
}

pub(crate) fn wait_for_health(port: u16, child: &mut Child) -> HealthOutcome {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    let mut ever_listened = false;
    while Instant::now() < deadline {
        if std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok() {
            ever_listened = true;
            if health_ok(port) {
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

/// Blocking GET /api/health on the loopback server; true on HTTP 200.
/// Hand-rolled (no extra crates); the timeouts keep a wedged server from
/// stalling the boot loop.
fn health_ok(port: u16) -> bool {
    use std::io::{Read, Write};

    let Ok(mut stream) = std::net::TcpStream::connect(("127.0.0.1", port)) else {
        return false;
    };
    if stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .is_err()
        || stream
            .set_write_timeout(Some(Duration::from_secs(3)))
            .is_err()
    {
        return false;
    }
    if write!(
        stream,
        "GET /api/health HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
    )
    .is_err()
    {
        return false;
    }
    let mut buf = vec![0u8; 4096];
    let mut raw = Vec::new();
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&raw)
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        == Some("200")
}
