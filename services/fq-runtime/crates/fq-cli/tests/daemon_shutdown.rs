//! Subprocess tests for the daemon's shutdown paths — a signal
//! (SIGINT/SIGTERM) and a graceful `fq drain` (ADR-0027).
//!
//! **SIGTERM:** what a process manager / `docker stop` / an orchestrator
//! sends to stop a service — now triggers a **graceful drain** (ADR-0027),
//! not just a clean infra shutdown: in-flight invocations suspend at a step
//! boundary and the daemon exits cleanly, rather than the abrupt default
//! disposition (exit-by-signal 143) that orphans the worker and abandons
//! in-flight work. (Ctrl-C stays a fast stop.)
//!
//! **Drain:** `fq drain` publishes on `fq.control.drain`; the daemon's
//! control-drain listener flips the same shared drain signal (in-flight
//! invocations suspend at a step boundary, the dispatcher stops consuming),
//! waits up to `drain_deadline_ms`, then exits cleanly — the NATS-transport
//! equivalent of the SIGTERM path.
//!
//! These tests are **serialized** by a shared lock: each spawns a full
//! `fq run` daemon that subscribes to the *global* `fq.control.drain`
//! subject, so a drain from one would otherwise reach the other's daemon.
//!
//! Both need a live NATS broker, so they are gated on `FQ_NATS_URL` and
//! skip when it is unset. `just ci` in the runtime workspace exports it
//! (dev broker on :4222); CI brings that broker up.

#![cfg(unix)]

use std::io::ErrorKind;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Serializes the daemon-shutdown tests: each spawns a full `fq run`
/// daemon on the shared broker, and `fq.control.drain` is a global
/// subject, so two must never be up at once.
static SHUTDOWN_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
    let _guard = SHUTDOWN_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

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

    assert!(
        status.success(),
        "expected clean exit(0) on SIGTERM, got {status:?} \
         (signal = {:?} — 15/SIGTERM means the abrupt default disposition is back)\n--- log ---\n{log}",
        status.signal(),
    );
    assert!(
        log.contains("Received SIGTERM, draining..."),
        "SIGTERM did not take the drain path\n--- log ---\n{log}",
    );
    assert!(
        log.contains("Draining"),
        "SIGTERM did not run the bounded drain wait (ADR-0027)\n--- log ---\n{log}",
    );

    // A graceful shutdown must also *deregister* the worker: its
    // coordination row should read `shutdown`, not linger `alive` for
    // the sweep to flip to `stale`. Checked through the product's own
    // read-only view over the same cache DB (no NATS needed).
    let workers = Command::new(fq_binary())
        .args(["workers", "list", "--json"])
        .env("FQ_CONFIG", "/nonexistent/fq.toml")
        .env("FQ_NATS_URL", &nats_url)
        .env("FQ_CACHE_DIR", scratch.join("cache"))
        .env("FQ_AGENTS_DIR", scratch.join("agents"))
        .output()
        .expect("run fq workers list");
    let workers_out = String::from_utf8_lossy(&workers.stdout).into_owned();

    let _ = std::fs::remove_dir_all(&scratch);

    assert!(
        workers.status.success(),
        "`fq workers list --json` failed: {}\n{workers_out}",
        String::from_utf8_lossy(&workers.stderr),
    );
    assert!(
        workers_out.contains("shutdown"),
        "worker was not deregistered on graceful shutdown — \
         expected a `shutdown` status in `fq workers list`:\n{workers_out}",
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

/// `fq drain` makes a running daemon stop consuming, drain in-flight work
/// at a step boundary, and exit cleanly — no signal needed (ADR-0027).
/// Here the daemon is idle, so the bounded wait finds nothing to suspend
/// and the drain completes at once; the suspend/resume of live invocations
/// is proven at the unit level by the drain-equivalence property test.
#[test]
fn daemon_drains_and_exits_on_fq_drain() {
    let Ok(nats_url) = std::env::var("FQ_NATS_URL") else {
        eprintln!("skipping daemon_drains_and_exits_on_fq_drain: FQ_NATS_URL not set");
        return;
    };
    let _guard = SHUTDOWN_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let scratch = unique_scratch();
    let log_path = scratch.join("daemon.log");
    let log = std::fs::File::create(&log_path).expect("create daemon log");
    let log_err = log.try_clone().expect("clone daemon log handle");

    let mut child = Command::new(fq_binary())
        .arg("run")
        .env("FQ_CONFIG", "/nonexistent/fq.toml")
        .env("FQ_NATS_URL", &nats_url)
        .env("FQ_CACHE_DIR", scratch.join("cache"))
        .env("FQ_AGENTS_DIR", scratch.join("agents"))
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .spawn()
        .expect("spawn fq run");

    // Wait for the daemon to reach steady state.
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

    // Ask it to drain — a separate `fq drain` invocation on the same broker.
    let drain = Command::new(fq_binary())
        .arg("drain")
        .env("FQ_CONFIG", "/nonexistent/fq.toml")
        .env("FQ_NATS_URL", &nats_url)
        .env("FQ_CACHE_DIR", scratch.join("cache"))
        .env("FQ_AGENTS_DIR", scratch.join("agents"))
        .output()
        .expect("run fq drain");
    assert!(
        drain.status.success(),
        "`fq drain` failed: {}",
        String::from_utf8_lossy(&drain.stderr)
    );

    // The daemon must drain and exit on its own — no signal sent.
    let status = wait_with_timeout(&mut child, Duration::from_secs(15))
        .expect("daemon did not exit within 15s of `fq drain` (drain hung?)");
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&scratch);

    assert!(
        status.success(),
        "expected clean exit(0) after drain, got {status:?}\n--- log ---\n{log}"
    );
    assert!(
        log.contains("drain requested"),
        "daemon did not observe the drain control message\n--- log ---\n{log}"
    );
    assert!(
        log.contains("Draining"),
        "daemon did not run the bounded drain wait\n--- log ---\n{log}"
    );
    assert!(
        log.contains("trigger dispatcher stopped cleanly"),
        "dispatcher did not stop cleanly on drain\n--- log ---\n{log}"
    );
}

/// `fq down` makes a running daemon drain in-flight work to a step
/// boundary, deregister its worker, and exit — and the command
/// *confirms* the exit by waiting for the daemon's `fq.system.shutdown`
/// event (issue #63). Idle daemon here, so the bounded drain finds
/// nothing to suspend and the stop completes at once.
#[test]
fn daemon_stops_and_confirms_on_fq_down() {
    let Ok(nats_url) = std::env::var("FQ_NATS_URL") else {
        eprintln!("skipping daemon_stops_and_confirms_on_fq_down: FQ_NATS_URL not set");
        return;
    };
    let _guard = SHUTDOWN_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let scratch = unique_scratch();
    let log_path = scratch.join("daemon.log");
    let log = std::fs::File::create(&log_path).expect("create daemon log");
    let log_err = log.try_clone().expect("clone daemon log handle");

    let mut child = Command::new(fq_binary())
        .arg("run")
        .env("FQ_CONFIG", "/nonexistent/fq.toml")
        .env("FQ_NATS_URL", &nats_url)
        .env("FQ_CACHE_DIR", scratch.join("cache"))
        .env("FQ_AGENTS_DIR", scratch.join("agents"))
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .spawn()
        .expect("spawn fq run");

    // Wait for the daemon to reach steady state.
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

    // `fq down` should stop the daemon AND confirm the exit itself
    // (exit 0 only after it observes fq.system.shutdown).
    let down = Command::new(fq_binary())
        .arg("down")
        .env("FQ_CONFIG", "/nonexistent/fq.toml")
        .env("FQ_NATS_URL", &nats_url)
        .env("FQ_CACHE_DIR", scratch.join("cache"))
        .env("FQ_AGENTS_DIR", scratch.join("agents"))
        .output()
        .expect("run fq down");
    let down_out = String::from_utf8_lossy(&down.stdout).into_owned();
    let down_err = String::from_utf8_lossy(&down.stderr).into_owned();

    // The daemon must have exited on its own — no signal sent.
    let status = wait_with_timeout(&mut child, Duration::from_secs(15))
        .expect("daemon did not exit within 15s of `fq down` (down hung?)");
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();

    // A clean `fq down` deregisters the worker: its coordination row
    // must read `shutdown`, not linger `alive`.
    let workers = Command::new(fq_binary())
        .args(["workers", "list", "--json"])
        .env("FQ_CONFIG", "/nonexistent/fq.toml")
        .env("FQ_NATS_URL", &nats_url)
        .env("FQ_CACHE_DIR", scratch.join("cache"))
        .env("FQ_AGENTS_DIR", scratch.join("agents"))
        .output()
        .expect("run fq workers list");
    let workers_out = String::from_utf8_lossy(&workers.stdout).into_owned();

    let _ = std::fs::remove_dir_all(&scratch);

    assert!(
        down.status.success(),
        "`fq down` failed (should exit 0 after confirming the daemon stopped):          stdout={down_out}\nstderr={down_err}"
    );
    assert!(
        down_out.contains("Daemon stopped"),
        "`fq down` did not confirm the daemon stopped:\n{down_out}"
    );
    assert!(
        status.success(),
        "expected clean exit(0) after down, got {status:?}\n--- log ---\n{log}"
    );
    assert!(
        log.contains("down requested"),
        "daemon did not observe the down control message\n--- log ---\n{log}"
    );
    assert!(
        workers_out.contains("shutdown"),
        "worker was not deregistered on `fq down` — expected a `shutdown` status:\n{workers_out}"
    );
}

/// `fq down --now` stops the daemon without draining — clean teardown +
/// worker deregister + immediate exit, the proper command replacing
/// `pkill -INT` (issue #63). Confirmed via the same shutdown-event wait.
#[test]
fn daemon_stops_now_on_fq_down_now() {
    let Ok(nats_url) = std::env::var("FQ_NATS_URL") else {
        eprintln!("skipping daemon_stops_now_on_fq_down_now: FQ_NATS_URL not set");
        return;
    };
    let _guard = SHUTDOWN_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let scratch = unique_scratch();
    let log_path = scratch.join("daemon.log");
    let log = std::fs::File::create(&log_path).expect("create daemon log");
    let log_err = log.try_clone().expect("clone daemon log handle");

    let mut child = Command::new(fq_binary())
        .arg("run")
        .env("FQ_CONFIG", "/nonexistent/fq.toml")
        .env("FQ_NATS_URL", &nats_url)
        .env("FQ_CACHE_DIR", scratch.join("cache"))
        .env("FQ_AGENTS_DIR", scratch.join("agents"))
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .spawn()
        .expect("spawn fq run");

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

    let down = Command::new(fq_binary())
        .args(["down", "--now"])
        .env("FQ_CONFIG", "/nonexistent/fq.toml")
        .env("FQ_NATS_URL", &nats_url)
        .env("FQ_CACHE_DIR", scratch.join("cache"))
        .env("FQ_AGENTS_DIR", scratch.join("agents"))
        .output()
        .expect("run fq down --now");
    let down_out = String::from_utf8_lossy(&down.stdout).into_owned();
    let down_err = String::from_utf8_lossy(&down.stderr).into_owned();

    let status = wait_with_timeout(&mut child, Duration::from_secs(15))
        .expect("daemon did not exit within 15s of `fq down --now`");
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&scratch);

    assert!(
        down.status.success(),
        "`fq down --now` failed: stdout={down_out}\nstderr={down_err}"
    );
    assert!(
        down_out.contains("Daemon stopped"),
        "`fq down --now` did not confirm the daemon stopped:\n{down_out}"
    );
    assert!(
        status.success(),
        "expected clean exit(0) after down --now, got {status:?}\n--- log ---\n{log}"
    );
    assert!(
        log.contains("down requested (--now)"),
        "daemon did not take the --now (no-drain) path\n--- log ---\n{log}"
    );
}

/// `fq down` with no daemon running must fail *fast*: a live daemon
/// heartbeats within ~10s, so with no heartbeat at all `fq down` reports
/// "no daemon" inside its ~20s liveness window instead of blocking out the
/// full drain-deadline ceiling (~130s). Regression guard for the issue #63
/// review follow-up.
#[test]
fn fq_down_fast_fails_when_no_daemon_running() {
    let Ok(nats_url) = std::env::var("FQ_NATS_URL") else {
        eprintln!("skipping fq_down_fast_fails_when_no_daemon_running: FQ_NATS_URL not set");
        return;
    };
    let _guard = SHUTDOWN_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let scratch = unique_scratch();

    // No `fq run` daemon is spawned — nothing is listening on NATS.
    let started = Instant::now();
    let down = Command::new(fq_binary())
        .arg("down")
        .env("FQ_CONFIG", "/nonexistent/fq.toml")
        .env("FQ_NATS_URL", &nats_url)
        .env("FQ_CACHE_DIR", scratch.join("cache"))
        .env("FQ_AGENTS_DIR", scratch.join("agents"))
        .output()
        .expect("run fq down");
    let elapsed = started.elapsed();
    let out = String::from_utf8_lossy(&down.stdout).into_owned();
    let err = String::from_utf8_lossy(&down.stderr).into_owned();
    let _ = std::fs::remove_dir_all(&scratch);

    assert!(
        !down.status.success(),
        "`fq down` must fail when no daemon is running:\nstdout={out}\nstderr={err}"
    );
    // The CLI logs its error through `tracing` (to stdout, like its other
    // output), not to stderr — so accept it on either stream.
    assert!(
        out.contains("no running `fq run` daemon") || err.contains("no running `fq run` daemon"),
        "expected a 'no daemon' error, got:\nstdout={out}\nstderr={err}"
    );
    // Fast-fail: well inside the ~20s liveness window, nowhere near the
    // full ~130s drain-deadline ceiling. Generous slack for CI.
    assert!(
        elapsed < Duration::from_secs(60),
        "`fq down` did not fast-fail with no daemon (took {elapsed:?})"
    );
}
