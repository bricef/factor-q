//! The daemon must shut down *gracefully* on SIGTERM — the signal a
//! process manager, `docker stop`, or a deploy script sends by default.
//! Without an explicit handler, Rust's default SIGTERM disposition
//! terminates the process abruptly (exit-by-signal 143), orphaning the
//! worker registration and any in-flight invocation. `run_daemon` installs
//! `wait_for_shutdown_signal`, which maps SIGTERM onto the same clean
//! shutdown path as Ctrl-C; this test pins that behaviour.
//!
//! ADR-0027 groundwork: catching SIGTERM is the precondition for a
//! supervised stop / graceful-drain deploy. This test is the regression
//! guard for the signal-capture half.
//!
//! Needs a live NATS broker (the daemon connects on startup), so it is
//! gated on `FQ_NATS_URL` exactly like the other runtime integration
//! tests and skips when it is unset. `just ci` in the runtime workspace
//! exports it (dev broker on :4222); CI brings that broker up.

#![cfg(unix)]

use std::io::ErrorKind;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn fq_binary() -> &'static str {
    env!("CARGO_BIN_EXE_fq")
}

/// A unique, isolated scratch dir so the daemon's projection DB / cache
/// never touches the developer's real state and parallel runs don't clash.
fn unique_scratch() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("fq-sigterm-{}-{}", std::process::id(), nanos));
    std::fs::create_dir_all(dir.join("cache")).unwrap();
    std::fs::create_dir_all(dir.join("agents")).unwrap();
    dir
}

#[test]
fn daemon_shuts_down_gracefully_on_sigterm() {
    let Ok(nats_url) = std::env::var("FQ_NATS_URL") else {
        eprintln!("skipping daemon_shuts_down_gracefully_on_sigterm: FQ_NATS_URL not set");
        return;
    };

    let scratch = unique_scratch();
    let log_path = scratch.join("daemon.log");
    let log = std::fs::File::create(&log_path).expect("create daemon log");
    let log_err = log.try_clone().expect("clone daemon log handle");

    let mut child = Command::new(fq_binary())
        .arg("run")
        // Everything via env so the test never reads a real fq.toml.
        .env("FQ_CONFIG", "/nonexistent/fq.toml")
        .env("FQ_NATS_URL", &nats_url)
        .env("FQ_CACHE_DIR", scratch.join("cache"))
        .env("FQ_AGENTS_DIR", scratch.join("agents"))
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .spawn()
        .expect("spawn fq run");

    // Wait for the daemon to reach its steady state (the point past which
    // the shutdown select is armed). Fail loudly if it dies during startup.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut ready = false;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("poll fq run") {
            let log = std::fs::read_to_string(&log_path).unwrap_or_default();
            panic!("daemon exited during startup with {status:?}\n--- log ---\n{log}");
        }
        if std::fs::read_to_string(&log_path)
            .unwrap_or_default()
            .contains("Runtime ready")
        {
            ready = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(ready, "daemon never reached 'Runtime ready' within 30s");

    // SIGTERM — the signal under test.
    let rc = unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    assert_eq!(
        rc,
        0,
        "kill(SIGTERM) failed: {}",
        std::io::Error::last_os_error()
    );

    // It must exit cleanly and promptly, not be killed by the signal.
    let status = wait_with_timeout(&mut child, Duration::from_secs(15))
        .expect("daemon did not exit within 15s of SIGTERM (graceful shutdown hung?)");

    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&scratch);

    assert!(
        status.success(),
        "expected clean exit(0) on SIGTERM, got {status:?} \
         (signal = {:?} — 15/SIGTERM means the abrupt default disposition is back)\n--- log ---\n{log}",
        status.signal(),
    );
    assert!(
        log.contains("Received SIGTERM, shutting down..."),
        "graceful-shutdown message missing — SIGTERM may not have driven the clean path\n--- log ---\n{log}",
    );
}

/// Poll `try_wait` until the child exits or the timeout elapses. Returns
/// `None` on timeout (caller treats that as a hung shutdown).
fn wait_with_timeout(
    child: &mut std::process::Child,
    timeout: Duration,
) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => panic!("try_wait failed: {e}"),
        }
    }
}
