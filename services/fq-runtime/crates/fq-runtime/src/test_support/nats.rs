//! A private `nats-server` per test (#233).
//!
//! Tests used to share the developer's dev broker via `FQ_NATS_URL`. That
//! made them lie to each other and to us:
//!
//! - `fq.control.down` is a **global** subject, so one test's `fq down`
//!   reached every daemon on the broker — including strays left behind by an
//!   interrupted run, which then poisoned every later run.
//! - JetStream streams were shared, so state leaked between tests even though
//!   each had its own projection DB.
//! - Worst: with `FQ_NATS_URL` unset the tests **skipped and reported green**.
//!
//! Each [`NatsServer`] is its own process with its own port and its own
//! JetStream store, so none of that is reachable. An orphan left by a hard
//! kill is harmless — it holds a random port nobody else asks for.
//!
//! The binary is pinned by `.nats-version` at the repo root and installed by
//! `just install-nats`; `FQ_TEST_NATS_SERVER` overrides the path. A missing
//! binary is a hard failure, never a skip — that is the whole point.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

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
        let child = Command::new(&bin)
            .arg("--port")
            .arg("-1")
            .arg("--ports_file_dir")
            .arg(dir.join("ports"))
            .arg("-js")
            .arg("-sd")
            .arg(dir.join("js"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|e| {
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

    /// `{"nats":["nats://127.0.0.1:45393","nats://[::1]:45393"]}` — take the
    /// first entry, which is the IPv4 client URL. Returns `None` until the
    /// file exists and parses, since it is written non-atomically.
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
///
/// Replaces the old `require_nats()`, which read `FQ_NATS_URL` and *skipped*
/// when it was unset. Nothing skips now.
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
        let a = test_nats();
        let b = test_nats();
        assert_ne!(a.url(), b.url(), "each test must get its own broker");
    }
}
