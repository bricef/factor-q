//! The edge listener is the instance lock, and no failed start leaves a
//! worker row behind (review B5, issue #550).
//!
//! Each test injects a real fault into a real `fqd` subprocess and
//! asserts on the *shared state*, not on the exit code: an exit code
//! only says the daemon knew it had failed, where the whole finding is
//! about what it left behind for the next operator to find. So the
//! assertions are the control-plane worker table and the event stream.
//!
//! **The faults.**
//!
//! - *The address is already held.* A `TcpListener` in this process
//!   takes the port first, which is exactly what a predecessor that has
//!   not finished draining does. The daemon must lose at `bind(2)` —
//!   before the broker connection, the stores, the registration or
//!   recovery's `system.recovery` publish.
//! - *Two daemons, one state directory.* The first is started on an
//!   ephemeral port and its address read back from its own log; the
//!   second is pointed at that address and the same directories. The
//!   second must refuse, and the first must be undisturbed.
//! - *A failure after registration.* An agent naming a model no
//!   provider prices fails the coverage guarantee (ADR-0004), which is
//!   checked after the worker has registered. The row must read
//!   `shutdown`, not sit `alive` for the sweep to call `stale`.
//!
//! Each daemon gets its own `nats-server` for the same reason
//! `daemon_shutdown.rs` does. The pinned binary comes from
//! `just install-nats` (`FQ_TEST_NATS_SERVER`).

#![cfg(unix)]

use std::io::ErrorKind;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn fqd_binary() -> &'static str {
    env!("CARGO_BIN_EXE_fqd")
}

/// A scratch directory with a config naming `bind`. Everything the
/// daemon writes lands here and nowhere near a developer's real state.
fn scratch_with_bind(tag: &str, bind: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("fq-lock-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(dir.join("cache")).unwrap();
    std::fs::create_dir_all(dir.join("agents")).unwrap();
    std::fs::write(dir.join("fq.toml"), format!("[edge]\nbind = \"{bind}\"\n")).unwrap();
    dir
}

fn spawn_daemon(scratch: &std::path::Path, nats_url: &str, log_name: &str) -> std::process::Child {
    let log = std::fs::File::create(scratch.join(log_name)).expect("create daemon log");
    let log_err = log.try_clone().expect("clone daemon log handle");
    Command::new(fqd_binary())
        .env("FQ_DAEMON_CONFIG", scratch.join("fq.toml"))
        .env("FQ_NATS_URL", nats_url)
        .env("FQ_CACHE_DIR", scratch.join("cache"))
        .env("FQ_STATE_DIR", scratch.join("state"))
        .env("FQ_AGENTS_DIR", scratch.join("agents"))
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .spawn()
        .expect("spawn fqd")
}

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

/// Wait for a line to appear in the daemon's log, failing loudly if the
/// daemon dies first. Waiting on the observable rather than on a fixed
/// sleep (#433's caveat).
fn wait_for_log(
    child: &mut std::process::Child,
    log: &std::path::Path,
    needle: &str,
    timeout: Duration,
) -> String {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("poll fqd") {
            let text = std::fs::read_to_string(log).unwrap_or_default();
            panic!("daemon exited with {status:?} before logging {needle:?}\n--- log ---\n{text}");
        }
        let text = std::fs::read_to_string(log).unwrap_or_default();
        if text.contains(needle) {
            return text;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let text = std::fs::read_to_string(log).unwrap_or_default();
    panic!("daemon never logged {needle:?} within {timeout:?}\n--- log ---\n{text}");
}

/// Every worker row's status, read straight from the control-plane
/// store. `None` when the store was never even created — which is
/// itself the assertion for a daemon that failed at bind.
fn worker_statuses(cache: &std::path::Path) -> Option<Vec<String>> {
    let paths = fq_runtime::db::RuntimeDbPaths::under(cache);
    if !paths.control_plane.exists() {
        return None;
    }
    Some(
        tokio::runtime::Runtime::new()
            .expect("worker-roster runtime")
            .block_on(async {
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
            }),
    )
}

/// Every `fq.system.*` subject the broker saw, drained from a core-NATS
/// subscription opened before the daemon was started.
struct SystemWatch {
    handle: std::thread::JoinHandle<Vec<String>>,
    stop: std::sync::mpsc::Sender<()>,
}

impl SystemWatch {
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
                        .subscribe("fq.system.>".to_string())
                        .await
                        .expect("subscribe fq.system.>");
                    ready_tx.send(()).expect("signal ready");
                    let mut seen = Vec::new();
                    loop {
                        tokio::select! {
                            msg = futures::StreamExt::next(&mut sub) => match msg {
                                Some(Ok(event)) => seen.push(event.subject()),
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
                    seen
                })
        });
        ready_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("system watch never subscribed");
        Self { handle, stop }
    }

    fn finish(self) -> Vec<String> {
        let _ = self.stop.send(());
        self.handle.join().expect("system watch thread")
    }
}

/// A daemon that cannot take its address must exit having done nothing
/// at all: no worker row, no `system.recovery`, not even a
/// `system.startup`.
#[test]
fn a_held_address_stops_the_daemon_before_it_touches_anything() {
    let server = fq_test_support::NatsServer::start();
    let nats_url = server.url().to_string();
    let watch = SystemWatch::start(&nats_url);

    // The predecessor that has not let go: a real socket on a real
    // port, held for the whole test.
    let squatter = std::net::TcpListener::bind("127.0.0.1:0").expect("hold the edge address");
    let addr = squatter.local_addr().unwrap().to_string();

    let scratch = scratch_with_bind("held", &addr);
    let mut child = spawn_daemon(&scratch, &nats_url, "daemon.log");
    let status = wait_with_timeout(&mut child, Duration::from_secs(30))
        .expect("a daemon that cannot bind must exit, not hang");
    let log = std::fs::read_to_string(scratch.join("daemon.log")).unwrap_or_default();
    let workers = worker_statuses(&scratch.join("cache"));
    let subjects = watch.finish();
    let _ = std::fs::remove_dir_all(&scratch);

    assert!(
        !status.success(),
        "a daemon that could not bind must exit non-zero\n--- log ---\n{log}"
    );
    assert!(
        log.contains(&addr),
        "the failure must name the address an operator has to free\n--- log ---\n{log}"
    );
    assert!(
        workers.is_none(),
        "the daemon registered a worker (or opened the control plane at all) despite \
         losing the address: {workers:?}"
    );
    assert!(
        subjects.is_empty(),
        "a daemon that never started published lifecycle events: {subjects:?} — \
         `system.recovery` here means recovery took ownership of invocations for a \
         process that is not running"
    );
}

/// Two daemons, one state directory. The second loses the address and
/// refuses to start; the first does not notice.
#[test]
fn a_second_daemon_on_one_state_dir_refuses_and_leaves_the_first_running() {
    let server = fq_test_support::NatsServer::start();
    let nats_url = server.url().to_string();

    let scratch = scratch_with_bind("twins", "127.0.0.1:0");
    let mut first = spawn_daemon(&scratch, &nats_url, "daemon.log");
    let log = wait_for_log(
        &mut first,
        &scratch.join("daemon.log"),
        "Runtime ready",
        Duration::from_secs(30),
    );
    let addr = log
        .lines()
        .find_map(|l| l.trim().strip_prefix("- edge is listening on "))
        .expect("edge addr in the daemon log")
        .trim()
        .to_string();

    // The second daemon: same stores, same address, started while the
    // first is serving.
    std::fs::write(
        scratch.join("fq.toml"),
        format!("[edge]\nbind = \"{addr}\"\n"),
    )
    .unwrap();
    let mut second = spawn_daemon(&scratch, &nats_url, "second.log");
    let second_status = wait_with_timeout(&mut second, Duration::from_secs(30))
        .expect("the second daemon must exit rather than share the store");
    let second_log = std::fs::read_to_string(scratch.join("second.log")).unwrap_or_default();

    // The first is still serving: it has not exited, and its roster
    // still holds exactly one live worker — its own.
    let first_alive = first.try_wait().expect("poll first daemon").is_none();
    let workers = worker_statuses(&scratch.join("cache")).unwrap_or_default();

    let _ = first.kill();
    let _ = first.wait();
    let _ = std::fs::remove_dir_all(&scratch);

    assert!(
        !second_status.success(),
        "the second daemon started on a store another daemon owns\n--- log ---\n{second_log}"
    );
    assert!(
        second_log.contains(&addr),
        "the refusal must name the contested address\n--- log ---\n{second_log}"
    );
    assert!(
        first_alive,
        "the first daemon exited when a second one was started against its store"
    );
    assert_eq!(
        workers.iter().filter(|s| *s == "alive").count(),
        1,
        "expected exactly the first daemon's row to be alive: {workers:?}"
    );
}

/// The B5 half that is not about binding: a step that fails *after*
/// the worker has registered.
///
/// The injected fault is the pricing coverage guarantee (ADR-0004) —
/// an agent naming a model no provider prices — because it is a real
/// startup refusal that sits between the registration and the
/// teardown, which is precisely the region that used to leak rows. The
/// guard is what makes this pass for every other step in that region
/// too; its contract is unit-tested in `worker_registration/tests.rs`.
#[test]
fn a_failure_after_registration_leaves_the_worker_shutdown() {
    let server = fq_test_support::NatsServer::start();
    let nats_url = server.url().to_string();

    let scratch = scratch_with_bind("unpriced", "127.0.0.1:0");
    std::fs::write(
        scratch.join("agents").join("unpriced.md"),
        "---\nname: unpriced\nmodel: no-such-provider/no-such-model\nbudget: 1.0\n---\n\nAgent.",
    )
    .unwrap();

    let mut child = spawn_daemon(&scratch, &nats_url, "daemon.log");
    let status = wait_with_timeout(&mut child, Duration::from_secs(60))
        .expect("a daemon that fails its pricing guarantee must exit");
    let log = std::fs::read_to_string(scratch.join("daemon.log")).unwrap_or_default();
    let workers = worker_statuses(&scratch.join("cache"));
    let _ = std::fs::remove_dir_all(&scratch);

    assert!(
        !status.success(),
        "an uncovered model must refuse the start\n--- log ---\n{log}"
    );
    let workers = workers.expect("the worker registered before the failure, so the store exists");
    assert!(
        !workers.is_empty(),
        "this test is only meaningful if the daemon got as far as registering: {workers:?}\
         \n--- log ---\n{log}"
    );
    assert!(
        workers.iter().all(|status| status == "shutdown"),
        "a post-registration failure left a row that is not `shutdown` — it will age \
         into `stale` and be reported as a crash: {workers:?}\n--- log ---\n{log}"
    );
}
