//! Test-only helpers shared across factor-q's independent workspaces (#233).
//!
//! [`NatsServer`] is a private `nats-server` per test. Tests used to share the
//! developer's dev broker via `FQ_NATS_URL`, which made them lie to each other
//! and to us:
//!
//! - `fq.control.*` subjects are **global**, so one test's `fq down` / `fq
//!   drain` reached every daemon on the broker — including strays left behind
//!   by an interrupted run, which then poisoned every later run.
//! - JetStream streams were shared, so state leaked between tests even though
//!   each had its own scratch dir.
//! - Worst: with `FQ_NATS_URL` unset the tests **skipped and reported green**.
//!
//! Each [`NatsServer`] is its own process with its own port and its own
//! JetStream store, so none of that is reachable. An orphan left by a hard kill
//! is harmless — it holds a random loopback-only port nobody else asks for.
//!
//! This lives in a standalone crate rather than any one service's
//! `#[cfg(test)]` module because integration tests (a separate compilation
//! unit) and other workspaces (fq-store) cannot see `cfg(test)` items — they
//! dev-depend on this crate instead.
//!
//! The binary is pinned by `.nats-version` at the repo root and installed by
//! `just install-nats`; `FQ_TEST_NATS_SERVER` overrides the path. A missing
//! binary is a hard failure, never a skip — that is the whole point.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Configure `cmd` to lead a fresh process group inherited by its descendants.
///
/// On Linux, the direct child also receives `SIGKILL` if its spawning thread
/// dies. Call this before spawning the command.
#[cfg(unix)]
pub fn spawn_grouped(cmd: &mut tokio::process::Command) {
    // SAFETY: this closure runs after fork and before exec. `setpgid` and
    // Linux's `prctl` are async-signal-safe syscalls, and no allocation occurs.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            #[cfg(target_os = "linux")]
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Send `SIGKILL` to the process group led by `child`.
///
/// Call this before waiting for the child, because a reaped child has no PID.
#[cfg(unix)]
pub fn kill_group(child: &tokio::process::Child) {
    if let Some(pid) = child.id() {
        // SAFETY: `killpg` accepts any process-group ID. Failure (including an
        // already-exited group) is intentionally harmless during teardown.
        unsafe {
            libc::killpg(pid as i32, libc::SIGKILL);
        }
    }
}

/// Disambiguates two servers started within the same nanosecond.
static SEQ: AtomicU64 = AtomicU64::new(0);

/// How long to wait for the server to publish its ports file. Generous: this
/// is a local process start, so exceeding it means something is wrong, not
/// slow, and the panic below says so.
const READY_TIMEOUT: Duration = Duration::from_secs(10);

/// A running `nats-server`, private to one test. Killed on drop.
pub struct NatsServer {
    child: Child,
    url: String,
    dir: PathBuf,
}

impl NatsServer {
    /// Start a server on an ephemeral port with JetStream enabled, and block
    /// until it is accepting connections.
    ///
    /// Panics — rather than skips — if the binary is missing or never comes
    /// up. A test that cannot get a broker has not passed.
    pub fn start() -> Self {
        let bin =
            std::env::var("FQ_TEST_NATS_SERVER").unwrap_or_else(|_| "nats-server".to_string());

        // Unique per server: the ports file is globbed out of here, and
        // JetStream gets a store of its own so nothing is shared.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "fq-nats-{}-{}-{}",
            std::process::id(),
            nanos,
            SEQ.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir_all(dir.join("ports")).expect("create ports dir");
        std::fs::create_dir_all(dir.join("js")).expect("create jetstream dir");

        // `--port -1` lets NATS pick a free port; `--ports_file_dir` makes it
        // publish the choice once it is listening. That pairing is both the
        // race-free port discovery and the readiness signal — binding :0
        // ourselves and handing the number over would be a TOCTOU race.
        //
        // `-a 127.0.0.1` because the default bind is 0.0.0.0 and the server
        // is unauthenticated with JetStream enabled: on a box with a public
        // interface that would expose every test broker — and any orphan a
        // hard kill leaves behind — to the network. Loopback also pins the
        // ports file to loopback URLs, so taking its first entry stops
        // depending on nats-server's interface ordering.
        let mut cmd = Command::new(&bin);
        cmd.arg("-a")
            .arg("127.0.0.1")
            .arg("--port")
            .arg("-1")
            .arg("--ports_file_dir")
            .arg(dir.join("ports"))
            .arg("-js")
            .arg("-sd")
            .arg(dir.join("js"))
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        // Drop kills the server, but a SIGKILLed test runner never runs
        // Drop. On Linux the kernel delivers SIGKILL to the server when the
        // thread that spawned it dies, so interrupted runs cannot accumulate
        // orphaned brokers. Per-thread on purpose: every shape here starts
        // the server from a thread that outlives the guard — the test's own
        // thread (#[test], current-thread #[tokio::test]) or a runtime
        // worker that lives until the runtime drops (multi_thread). Do not
        // call this from spawn_blocking: those threads idle out mid-test
        // and would take the broker with them.
        #[cfg(target_os = "linux")]
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(|| {
                // Post-fork, pre-exec; prctl is async-signal-safe and the
                // setting survives the exec.
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let child = cmd.spawn().unwrap_or_else(|e| {
            panic!(
                "could not start `{bin}`: {e}\n\
                     The test broker is a pinned nats-server (.nats-version).\n\
                     Install it with `just install-nats`, or point \
                     FQ_TEST_NATS_SERVER at one."
            )
        });

        let mut server = Self {
            child,
            url: String::new(),
            dir,
        };
        server.url = server.await_url();
        server
    }

    /// The client URL, e.g. `nats://127.0.0.1:45393`. No auth: a private
    /// server needs none, which keeps the URL free of token userinfo.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Block until the ports file appears and read the client URL out of it.
    fn await_url(&mut self) -> String {
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            // Glob rather than build `nats-server_<pid>.ports` from
            // `child.id()`: the pid the server writes is the one *it* sees,
            // which need not match ours across a pid namespace. The directory
            // is private to this server, so any ports file in it is ours.
            if let Some(url) = self.read_ports_file() {
                return url;
            }
            if let Ok(Some(status)) = self.child.try_wait() {
                panic!("nats-server exited before publishing its port: {status}");
            }
            if Instant::now() >= deadline {
                panic!(
                    "nats-server did not publish a ports file within {READY_TIMEOUT:?} \
                     (dir: {})",
                    self.dir.display()
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// `{"nats":["nats://127.0.0.1:45393"]}` — the `-a 127.0.0.1` bind makes
    /// loopback the only entry, so taking the first is deterministic. Returns
    /// `None` until the file exists and parses, since it is written
    /// non-atomically.
    fn read_ports_file(&self) -> Option<String> {
        for entry in std::fs::read_dir(self.dir.join("ports")).ok()?.flatten() {
            let Ok(body) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&body) else {
                continue; // written non-atomically: a partial read just retries
            };
            if let Some(url) = parsed
                .get("nats")
                .and_then(|v| v.get(0))
                .and_then(|v| v.as_str())
            {
                return Some(url.to_string());
            }
        }
        None
    }
}

impl Drop for NatsServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Start a private broker for this test. The returned guard must be held for
/// the test's lifetime — dropping it kills the server.
pub fn test_nats() -> NatsServer {
    NatsServer::start()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn private_broker_starts_and_accepts_a_connection() {
        let server = test_nats();
        assert!(server.url().starts_with("nats://"), "url: {}", server.url());
        // The readiness contract: once start() returns, it is connectable.
        let client = async_nats::connect(server.url()).await.expect("connect");
        assert_eq!(
            client.connection_state(),
            async_nats::connection::State::Connected
        );
    }

    #[tokio::test]
    async fn two_servers_are_isolated_from_each_other() {
        use futures::StreamExt as _;

        let a = test_nats();
        let b = test_nats();
        assert_ne!(a.url(), b.url(), "each test must get its own broker");

        // Distinct URLs alone don't prove isolation — show a message
        // published on one broker never reaches a subscriber on the other.
        let ca = async_nats::connect(a.url()).await.expect("connect a");
        let cb = async_nats::connect(b.url()).await.expect("connect b");
        let mut sub = cb.subscribe("isolation.probe").await.expect("subscribe b");
        ca.publish("isolation.probe", "leak?".into())
            .await
            .expect("publish a");
        ca.flush().await.expect("flush a");
        let crossed = tokio::time::timeout(Duration::from_millis(250), sub.next()).await;
        assert!(
            crossed.is_err(),
            "a message crossed between private brokers: {crossed:?}"
        );
    }

    /// The PDEATHSIG contract: when the thread that spawned the broker
    /// dies without running Drop (a hard-killed test runner), the kernel
    /// reaps the broker. `mem::forget` stands in for the never-ran Drop;
    /// the scratch dir it leaks is the price of the test.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn broker_dies_with_its_spawning_thread() {
        let url = std::thread::spawn(|| {
            let server = test_nats();
            let url = server.url().to_string();
            std::mem::forget(server);
            url
        })
        .join()
        .expect("spawn thread");

        // The thread is gone; SIGKILL delivery is immediate, but give the
        // socket a moment to close before declaring the orphan immortal.
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if async_nats::connect(&url).await.is_err() {
                return; // dead, as required
            }
            assert!(
                Instant::now() < deadline,
                "broker at {url} outlived its spawning thread"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}
