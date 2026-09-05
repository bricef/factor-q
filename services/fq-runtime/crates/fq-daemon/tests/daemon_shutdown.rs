//! Subprocess tests for the daemon's shutdown paths — a signal
//! (SIGINT/SIGTERM) and a graceful `fq down` (ADR-0027).
//!
//! **SIGTERM:** what a process manager / `docker stop` / an orchestrator
//! sends to stop a service — now triggers a **graceful drain** (ADR-0027),
//! not just a clean infra shutdown: in-flight invocations suspend at a step
//! boundary and the daemon exits cleanly, rather than the abrupt default
//! disposition (exit-by-signal 143) that orphans the worker and abandons
//! in-flight work. (Ctrl-C stays a fast stop.)
//!
//! **Drain:** `fq down` invokes the `control.down` command on the daemon's
//! authenticated edge; the handler flips the same shared drain signal
//! (in-flight invocations suspend at a step boundary, the dispatcher stops
//! consuming), and the daemon waits up to `drain_deadline_ms` before exiting
//! cleanly — the RPC equivalent of the SIGTERM path.
//!
//! It was a core-NATS publish on `fq.control.down` until cohort 4.3, which is
//! why each test spawns its **own** `nats-server` (#233): the subject was
//! global, so on a shared broker one test's control message reached another
//! test's daemon, and these ran under a process-wide lock. The isolation is
//! kept — a daemon still needs a broker of its own for its streams — but the
//! stop itself is point-to-point by construction now: it is addressed to one
//! daemon's edge, which is also why every `fq down` here has to **pair** with
//! the daemon it means to stop. The pinned nats binary comes from
//! `just install-nats` (`FQ_TEST_NATS_SERVER`).

#![cfg(unix)]

use std::io::ErrorKind;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The daemon is its own binary now: `fq` cannot start one, so a test
/// that needs a running daemon spawns `fqd` directly.
fn fqd_binary() -> &'static str {
    env!("CARGO_BIN_EXE_fqd")
}

/// A client introduced to one daemon's edge: the pairing store it wrote
/// to, and the config naming the address it was paired with.
///
/// `fq down` is an authenticated command now (plan Phase 4, verb 4), so a
/// test that means to stop a daemon has to be able to reach it. Both
/// halves are scratch: `XDG_CONFIG_HOME` so `fq connect` never writes the
/// developer's real `connections.toml`, and a config of its own because
/// the daemon asked for port 0 and only its log knows what it got.
struct Pairing {
    xdg: tempfile::TempDir,
    config: std::path::PathBuf,
}

/// Read the daemon's log for its edge address, its state dir for the
/// admin token and fingerprint it wrote at first run, then pair with it
/// — what an operator (or a script) does by hand, once.
fn pair_with(scratch: &std::path::Path) -> Pairing {
    let text = std::fs::read_to_string(scratch.join("daemon.log")).expect("read daemon log");
    let addr = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("- edge is listening on "))
        .expect("edge addr in the daemon log")
        .trim()
        .to_string();
    let token = fq_test_support::admin_token(&scratch.join("state"));
    let fingerprint = fq_test_support::edge_fingerprint(&scratch.join("state"));

    let config = scratch.join("client.toml");
    std::fs::write(&config, format!("[edge]\nbind = \"{addr}\"\n")).expect("client config");
    let xdg = tempfile::tempdir().expect("xdg dir");
    let connect = Command::new(fq_client_binary())
        .args([
            "connect",
            &addr,
            "--token",
            &token,
            "--fingerprint",
            &fingerprint,
        ])
        .env("FQ_CLI_CONFIG", &config)
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("RUST_LOG", "off")
        .stdin(Stdio::piped())
        .output()
        .expect("run fq connect");
    assert!(
        connect.status.success(),
        "fq connect failed:\n{}",
        String::from_utf8_lossy(&connect.stderr)
    );
    Pairing { xdg, config }
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
    // The edge is on by default; an ephemeral port keeps the parallel
    // daemon-spawning tests from fighting over the fixed default bind.
    std::fs::write(dir.join("fq.toml"), "[edge]\nbind = \"127.0.0.1:0\"\n").unwrap();
    dir
}

#[test]
fn daemon_shuts_down_gracefully_on_sigterm() {
    let server = fq_test_support::NatsServer::start();
    let nats_url = server.url().to_string();

    let scratch = unique_scratch();
    let log_path = scratch.join("daemon.log");
    let log = std::fs::File::create(&log_path).expect("create daemon log");
    let log_err = log.try_clone().expect("clone daemon log handle");

    let mut child = Command::new(fqd_binary())
        // The scratch fq.toml plus env overrides — the test never
        // reads a real config.
        .env("FQ_DAEMON_CONFIG", scratch.join("fq.toml"))
        .env("FQ_NATS_URL", &nats_url)
        .env("FQ_CACHE_DIR", scratch.join("cache"))
        .env("FQ_STATE_DIR", scratch.join("state"))
        .env("FQ_AGENTS_DIR", scratch.join("agents"))
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .spawn()
        .expect("spawn fqd");

    // Wait for the daemon to reach its steady state (the point past which
    // the shutdown select is armed). Fail loudly if it dies during startup.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut ready = false;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("poll fqd") {
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
    // the sweep to flip to `stale`.
    let workers = worker_statuses(&scratch.join("cache"));

    let _ = std::fs::remove_dir_all(&scratch);

    assert!(
        workers.iter().any(|status| status == "shutdown"),
        "worker was not deregistered on graceful shutdown — \
         expected a `shutdown` row: {workers:?}",
    );
}

/// Every worker row's status, read straight from the daemon's
/// control-plane store.
///
/// This used to shell out to `fq workers list --json`, which read the
/// same file in-process. Verb 21 now speaks the daemon's edge (plan
/// Phase 4), and these assertions are made *after* the daemon has
/// exited — there is no edge left to ask, by construction. The
/// coordination row was always the thing under test; the CLI was only
/// ever the lens.
fn worker_statuses(cache: &std::path::Path) -> Vec<String> {
    tokio::runtime::Runtime::new()
        .expect("worker-roster runtime")
        .block_on(async {
            let paths = fq_runtime::db::RuntimeDbPaths::under(cache);
            let store =
                fq_runtime::control_plane::store::ControlPlaneStore::open(&paths.control_plane)
                    .await
                    .expect("open control-plane store");
            store
                .list_workers()
                .await
                .expect("list workers")
                .into_iter()
                .map(|worker| worker.status.as_str().to_string())
                .collect()
        })
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

/// `fq down` makes a running daemon drain in-flight work to a step
/// boundary, deregister its worker, and exit — and the command
/// *confirms* the exit by waiting for the daemon's `fq.system.shutdown`
/// event (issue #63). Idle daemon here, so the bounded drain finds
/// nothing to suspend and the stop completes at once.
#[test]
fn daemon_stops_and_confirms_on_fq_down() {
    let server = fq_test_support::NatsServer::start();
    let nats_url = server.url().to_string();

    let scratch = unique_scratch();
    let log_path = scratch.join("daemon.log");
    let log = std::fs::File::create(&log_path).expect("create daemon log");
    let log_err = log.try_clone().expect("clone daemon log handle");

    let mut child = Command::new(fqd_binary())
        .env("FQ_DAEMON_CONFIG", scratch.join("fq.toml"))
        .env("FQ_NATS_URL", &nats_url)
        .env("FQ_CACHE_DIR", scratch.join("cache"))
        .env("FQ_STATE_DIR", scratch.join("state"))
        .env("FQ_AGENTS_DIR", scratch.join("agents"))
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .spawn()
        .expect("spawn fqd");

    // Wait for the daemon to reach steady state.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut ready = false;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("poll fqd") {
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
    // (exit 0 only once the daemon's edge has stopped answering).
    let pairing = pair_with(&scratch);
    let down = Command::new(fq_client_binary())
        .arg("down")
        .env("FQ_CLI_CONFIG", &pairing.config)
        .env("XDG_CONFIG_HOME", pairing.xdg.path())
        .env("FQ_NATS_URL", &nats_url)
        .env("FQ_CACHE_DIR", scratch.join("cache"))
        .env("FQ_STATE_DIR", scratch.join("state"))
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
    let workers = worker_statuses(&scratch.join("cache"));

    let _ = std::fs::remove_dir_all(&scratch);

    assert!(
        down.status.success(),
        "`fq down` failed (should exit 0 after confirming the daemon stopped):          stdout={down_out}\nstderr={down_err}"
    );
    assert!(
        down_out.contains("Daemon stopped (requested mode=drain)"),
        "`fq down` did not confirm the daemon stopped:\n{down_out}"
    );
    assert!(
        status.success(),
        "expected clean exit(0) after down, got {status:?}\n--- log ---\n{log}"
    );
    assert!(
        log.contains("down requested"),
        "daemon did not observe the down command\n--- log ---\n{log}"
    );
    assert!(
        log.contains("trigger dispatcher stopped cleanly"),
        "dispatcher did not stop cleanly on down\n--- log ---\n{log}"
    );
    assert!(
        workers.iter().any(|status| status == "shutdown"),
        "worker was not deregistered on `fq down` — expected a `shutdown` row: {workers:?}"
    );
}

/// `fq down --now` stops the daemon without draining — clean teardown +
/// worker deregister + immediate exit, the proper command replacing
/// `pkill -INT` (issue #63). Confirmed via the same shutdown-event wait.
#[test]
fn daemon_stops_now_on_fq_down_now() {
    let server = fq_test_support::NatsServer::start();
    let nats_url = server.url().to_string();

    let scratch = unique_scratch();
    let log_path = scratch.join("daemon.log");
    let log = std::fs::File::create(&log_path).expect("create daemon log");
    let log_err = log.try_clone().expect("clone daemon log handle");

    let mut child = Command::new(fqd_binary())
        .env("FQ_DAEMON_CONFIG", scratch.join("fq.toml"))
        .env("FQ_NATS_URL", &nats_url)
        .env("FQ_CACHE_DIR", scratch.join("cache"))
        .env("FQ_STATE_DIR", scratch.join("state"))
        .env("FQ_AGENTS_DIR", scratch.join("agents"))
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .spawn()
        .expect("spawn fqd");

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut ready = false;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("poll fqd") {
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

    let pairing = pair_with(&scratch);
    let down = Command::new(fq_client_binary())
        .args(["down", "--now"])
        .env("FQ_CLI_CONFIG", &pairing.config)
        .env("XDG_CONFIG_HOME", pairing.xdg.path())
        .env("FQ_NATS_URL", &nats_url)
        .env("FQ_CACHE_DIR", scratch.join("cache"))
        .env("FQ_STATE_DIR", scratch.join("state"))
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
        down_out.contains("Daemon stopped (requested mode=now)"),
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

/// `fq down` with no daemon running must fail *fast*, and it is worth
/// recording why that got easy. It used to publish into a subject nobody
/// owned and then watch for a worker heartbeat for ~20s to tell "no daemon"
/// from "a daemon that is stopping", against a ~130s drain-deadline ceiling
/// — the liveness gate this test was written to guard (issue #63 review
/// follow-up). A command has no such ambiguity: there is nothing to dial,
/// so the refusal is immediate and the guard fails closed by construction.
/// The bound is kept regardless, because what it protects is an operator
/// (or a deploy script) not being blocked by a stop that cannot happen.
///
/// `XDG_CONFIG_HOME` is scratch deliberately: without it the test reads the
/// developer's real pairing store and could dial a daemon on this machine.
#[test]
fn fq_down_fast_fails_when_no_daemon_running() {
    let server = fq_test_support::NatsServer::start();
    let nats_url = server.url().to_string();

    let scratch = unique_scratch();
    let xdg = tempfile::tempdir().expect("xdg dir");

    // No daemon is spawned — nothing is listening.
    let started = Instant::now();
    let down = Command::new(fq_client_binary())
        .arg("down")
        .env("FQ_CLI_CONFIG", "/nonexistent/fq.toml")
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("FQ_NATS_URL", &nats_url)
        .env("FQ_CACHE_DIR", scratch.join("cache"))
        .env("FQ_STATE_DIR", scratch.join("state"))
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
    assert!(out.is_empty(), "fatal error must not pollute stdout: {out}");
    assert!(
        err.contains("no running `fqd`"),
        "expected a 'no daemon' error on stderr, got:\n{err}"
    );
    // Fast-fail: nowhere near the ~130s ceiling the stop wait is bounded
    // by. Generous slack for CI.
    assert!(
        elapsed < Duration::from_secs(60),
        "`fq down` did not fast-fail with no daemon (took {elapsed:?})"
    );
}

/// The client binary. `CARGO_BIN_EXE_*` only names binaries of the
/// package the test lives in, and `fq` is `fq-cli`'s — but both land in
/// the same target directory, so the daemon's own path names it.
#[allow(dead_code)]
fn fq_client_binary() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_fqd")).with_file_name("fq")
}

/// Every `fq.system.shutdown` reason the broker saw, collected from a
/// core-NATS subscription opened before the stop was asked for.
///
/// The projection cannot answer this: the projection consumer is
/// stopped before the shutdown event is published, by construction —
/// the event is the last thing the daemon does. So the assertion has to
/// come off the wire.
struct ShutdownWatch {
    handle: std::thread::JoinHandle<Vec<String>>,
    stop: std::sync::mpsc::Sender<()>,
}

impl ShutdownWatch {
    fn start(nats_url: &str) -> Self {
        let url = nats_url.to_string();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (stop, stop_rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            tokio::runtime::Runtime::new()
                .expect("watch runtime")
                .block_on(async move {
                    let bus = fq_runtime::EventBus::connect(&url)
                        .await
                        .expect("connect NATS");
                    let mut sub = bus
                        .subscribe("fq.system.shutdown".to_string())
                        .await
                        .expect("subscribe fq.system.shutdown");
                    ready_tx.send(()).expect("signal ready");
                    let mut reasons = Vec::new();
                    loop {
                        tokio::select! {
                            msg = futures::StreamExt::next(&mut sub) => match msg {
                                Some(Ok(event)) => {
                                    if let fq_runtime::events::EventPayload::SystemShutdown(p) =
                                        &event.payload
                                    {
                                        reasons.push(p.reason.clone());
                                    }
                                }
                                Some(Err(_)) => continue,
                                None => break,
                            },
                            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                                if stop_rx.try_recv().is_ok() {
                                    break;
                                }
                            }
                        }
                    }
                    reasons
                })
        });
        ready_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("shutdown watch never subscribed");
        Self { handle, stop }
    }

    fn finish(self) -> Vec<String> {
        // The event is published after the edge is gone; give the
        // subscription a beat to see it before tearing the watch down.
        std::thread::sleep(Duration::from_millis(500));
        let _ = self.stop.send(());
        self.handle.join().expect("shutdown watch thread")
    }
}

/// Wait for a line to appear in the daemon's log, and report how long
/// it took.
///
/// **The log is read before the child is polled, and once more after
/// the child has exited.** An idle daemon can print the line and exit
/// inside one poll interval, and a helper that asked `try_wait` first
/// would then report "exited before logging" about a line already
/// sitting in the file — which is exactly how these tests went red on
/// CI and stayed green on a slower developer box. Reaching the line is
/// the observable; the process still being alive afterwards is not
/// something the caller may assume.
fn wait_for_log_line(
    child: &mut std::process::Child,
    log: &std::path::Path,
    needle: &str,
    timeout: Duration,
) -> Duration {
    let started = Instant::now();
    loop {
        if std::fs::read_to_string(log)
            .unwrap_or_default()
            .contains(needle)
        {
            return started.elapsed();
        }
        if let Some(status) = child.try_wait().expect("poll fqd") {
            // One last read: the line may have been written between the
            // read above and the exit observed here.
            let text = std::fs::read_to_string(log).unwrap_or_default();
            if text.contains(needle) {
                return started.elapsed();
            }
            panic!("daemon exited with {status:?} before logging {needle:?}\n--- log ---\n{text}");
        }
        if started.elapsed() >= timeout {
            let text = std::fs::read_to_string(log).unwrap_or_default();
            panic!("daemon never logged {needle:?} within {timeout:?}\n--- log ---\n{text}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn spawn_ready_daemon(
    scratch: &std::path::Path,
    nats_url: &str,
) -> (std::process::Child, std::path::PathBuf) {
    let log_path = scratch.join("daemon.log");
    let log = std::fs::File::create(&log_path).expect("create daemon log");
    let log_err = log.try_clone().expect("clone daemon log handle");
    let mut child = Command::new(fqd_binary())
        .env("FQ_DAEMON_CONFIG", scratch.join("fq.toml"))
        .env("FQ_NATS_URL", nats_url)
        .env("FQ_CACHE_DIR", scratch.join("cache"))
        .env("FQ_STATE_DIR", scratch.join("state"))
        .env("FQ_AGENTS_DIR", scratch.join("agents"))
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .spawn()
        .expect("spawn fqd");
    wait_for_log_line(
        &mut child,
        &log_path,
        "Runtime ready",
        Duration::from_secs(30),
    );
    (child, log_path)
}

/// Review B9: the drain deadline is spent on the drain.
///
/// The observable is the ORDER of the teardown lines. The dispatcher —
/// the only task with in-flight work to suspend — used to be joined
/// *last*, after up to eight sequential five-second joins of consumers
/// and sweepers that have nothing to suspend, all charged against the
/// same deadline. It is joined first now, and the heartbeat producer
/// stops after it, so a worker that is still executing steps is still
/// on the roster.
#[test]
fn the_drain_is_joined_before_the_infrastructure_teardown() {
    let server = fq_test_support::NatsServer::start();
    let nats_url = server.url().to_string();
    let scratch = unique_scratch();
    let (mut child, log_path) = spawn_ready_daemon(&scratch, &nats_url);

    let rc = unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    assert_eq!(rc, 0, "kill(SIGTERM) failed");

    // The wait must start promptly: nothing is allowed to run ahead of
    // it and eat the deadline.
    let to_drain = wait_for_log_line(
        &mut child,
        &log_path,
        "Draining — waiting up to",
        Duration::from_secs(10),
    );
    let status = wait_with_timeout(&mut child, Duration::from_secs(20))
        .expect("daemon did not exit within 20s of SIGTERM");
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&scratch);

    assert!(
        status.success(),
        "expected a clean exit\n--- log ---\n{log}"
    );
    assert!(
        to_drain < Duration::from_secs(5),
        "the drain wait started {to_drain:?} after the signal; something ran ahead of it"
    );

    let at = |needle: &str| {
        log.find(needle)
            .unwrap_or_else(|| panic!("teardown never logged {needle:?}\n--- log ---\n{log}"))
    };
    let dispatcher = at("trigger dispatcher stopped cleanly");
    assert!(
        dispatcher < at("projection consumer stopped cleanly"),
        "the projection consumer was joined before the drain finished — the drain \
         deadline is being spent on infrastructure again\n--- log ---\n{log}"
    );
    assert!(
        dispatcher < at("heartbeat producer stopped cleanly"),
        "the heartbeat producer stopped before the drain finished, so a worker still \
         executing steps would look silent to the stale sweep\n--- log ---\n{log}"
    );
}

/// Issue #509, end to end: a second SIGTERM during a stop never costs
/// the clean teardown.
///
/// This is the promise the three doc comments used to make in the
/// opposite direction — "a second SIGTERM is absorbed", and before
/// that, wrongly, "restores the default disposition". Neither is what
/// an operator needs: the first leaves SIGKILL as the only way out of a
/// drain, and the second would skip the deregistration and the
/// `system.shutdown` event, which are the two things the caught signal
/// exists to guarantee. What must hold is that the daemon still exits
/// 0, still deregisters, and still says so.
///
/// The escalation's own timing — a second signal ending a wait that is
/// still running — is proved deterministically in
/// `hosted::teardown::tests`, with a stub holding the drain open; an
/// idle daemon's drain finishes too fast to race here.
#[test]
fn a_second_sigterm_never_costs_the_clean_teardown() {
    let server = fq_test_support::NatsServer::start();
    let nats_url = server.url().to_string();
    let watch = ShutdownWatch::start(&nats_url);
    let scratch = unique_scratch();
    let (mut child, log_path) = spawn_ready_daemon(&scratch, &nats_url);

    let pid = child.id() as i32;
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    // Send the second as soon as the first has been *observed* — the
    // earliest point at which it can land inside the drain wait, which
    // is what an operator watching a stuck stop does. An idle daemon's
    // drain can still finish first; that is why the assertions below
    // are the invariant rather than the escalation itself, and why the
    // escalation's timing is proved in `hosted::teardown::tests`
    // instead, with a stub that holds the drain open.
    let _ = wait_for_log_line(
        &mut child,
        &log_path,
        "Received SIGTERM, draining",
        Duration::from_secs(10),
    );
    // A second SIGTERM at a process that has already exited is an
    // ESRCH, not a failure of this test.
    unsafe { libc::kill(pid, libc::SIGTERM) };

    let status = wait_with_timeout(&mut child, Duration::from_secs(20))
        .expect("daemon did not exit within 20s of the second SIGTERM");
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    let workers = worker_statuses(&scratch.join("cache"));
    let reasons = watch.finish();
    let _ = std::fs::remove_dir_all(&scratch);

    assert!(
        status.success(),
        "a second SIGTERM must not turn a clean stop into a signal death: {status:?} \
         (signal = {:?})\n--- log ---\n{log}",
        status.signal(),
    );
    assert!(
        workers.iter().any(|status| status == "shutdown"),
        "the worker was not deregistered after a second SIGTERM: {workers:?}"
    );
    assert!(
        reasons
            .iter()
            .any(|r| r == "sigterm" || r.starts_with("sigterm_escalated_by_")),
        "no system.shutdown was published after a second SIGTERM: {reasons:?}"
    );
}

/// The shutdown select's own dispatcher arm winning must still reach
/// the end of the teardown.
///
/// That arm consumes the `JoinHandle`, and a `JoinHandle` polled after
/// completion panics — in the daemon's **main** task, which would skip
/// the MCP shutdown, the worker row's settle and the `system.shutdown`
/// publish. The shape predates this PR but the region was rewritten
/// here and nothing drove the arm.
///
/// The fault: the trigger stream is deleted out from under the running
/// daemon, so the dispatcher's message stream ends and it returns —
/// with no drain in progress, which is the `task_failed` variant and
/// the one that reaches `join_dispatcher`. The event stream is a
/// different stream, so the daemon can still publish its way out, which
/// is what makes the assertion possible.
///
/// The worker row is deliberately NOT `shutdown` here: a hosted task
/// died and took the runtime with it, and leaving the row for the stale
/// sweep is the honest signal that this daemon did not exit cleanly.
/// The `system.shutdown` event carrying `clean = false` is the proof
/// that the teardown ran to its end rather than panicking through it.
#[test]
fn the_teardown_completes_when_the_dispatcher_arm_wins() {
    let server = fq_test_support::NatsServer::start();
    let nats_url = server.url().to_string();
    let watch = ShutdownWatch::start(&nats_url);
    let scratch = unique_scratch();
    let (mut child, log_path) = spawn_ready_daemon(&scratch, &nats_url);

    // Pull the trigger stream out from under the dispatcher.
    tokio::runtime::Runtime::new()
        .expect("stream-delete runtime")
        .block_on(async {
            let bus = fq_runtime::EventBus::connect(&nats_url)
                .await
                .expect("connect NATS");
            bus.jetstream()
                .delete_stream(fq_runtime::bus::TRIGGER_STREAM_NAME)
                .await
                .expect("delete the trigger stream");
        });

    let status = wait_with_timeout(&mut child, Duration::from_secs(30)).unwrap_or_else(|| {
        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        panic!("daemon did not exit after its dispatcher died\n--- log ---\n{log}")
    });
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    let reasons = watch.finish();
    let _ = std::fs::remove_dir_all(&scratch);

    assert!(
        !status.success(),
        "a dead dispatcher must be a non-zero exit\n--- log ---\n{log}"
    );
    assert_eq!(
        status.signal(),
        None,
        "the daemon died by signal rather than exiting — a panic in the main task \
         aborts the teardown\n--- log ---\n{log}"
    );
    assert!(
        reasons.iter().any(|r| r == "task_failed"),
        "the teardown never reached its `system.shutdown` publish: {reasons:?} — the \
         dispatcher's own JoinHandle was polled twice\n--- log ---\n{log}"
    );
    assert!(
        !log.contains("JoinHandle polled after completion"),
        "the main task panicked joining an already-consumed handle\n--- log ---\n{log}"
    );
}
