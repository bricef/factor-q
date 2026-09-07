//! A daemon never outlives the test binary that started it (#630).
//!
//! **The incident.** The devbox was found carrying 42 `fqd` processes
//! from worktrees that no longer existed — 1.3 GB resident, each holding
//! an edge listener, all reparented to PID 1, all spawned inside one
//! three-second window 27 hours earlier. Every daemon-spawning suite
//! relied on `impl Drop` to stop its daemon, and `Drop` does not run when
//! the process it lives in dies by signal: an OOM kill (exit 137, twice
//! in one sprint), or any hard stop of the test binary.
//!
//! Every other test in this crate would pass with the guard removed —
//! they all take the normal path, where `Drop` is enough. This one does
//! not. It reproduces the failure exactly: a process spawns a daemon
//! through the fixture, reaches steady state, and is then `SIGKILL`ed
//! with no chance to run anything. The daemon must be gone a moment
//! later, and the only thing that can achieve that is
//! `PR_SET_PDEATHSIG`.
//!
//! **Why the broker belongs to the parent.** A daemon whose broker
//! vanishes may exit on its own, which would make a dead daemon prove
//! nothing. The broker here is started by the *outer* test and outlives
//! the killed helper, so the daemon's only reason to die is the guard.
//!
//! **How to check the guard still guards:** comment out the
//! `PR_SET_PDEATHSIG` line in `fq_test_support::child::arm_death_guard`
//! and run this file. It must fail.
//!
//! The manual version of the same experiment, for a leak hunt: run one
//! integration test binary, `kill -9` it mid-run, and count survivors by
//! exe path — `for p in $(pgrep -x fqd); do readlink /proc/$p/exe; done`,
//! keeping only paths under this worktree's `target/`. Never by name:
//! the dogfood daemon is also called `fqd`.

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use fq_test_support::TestChild;

/// Set on the re-invoked test binary to select helper mode. Its absence
/// is what makes [`helper_stands_a_daemon_up_and_parks`] a no-op in a
/// normal run.
const HELPER_ENV: &str = "FQ_TEST_ORPHAN_HELPER";
/// The broker the helper's daemon dials — the outer test's, so it
/// outlives the helper.
const BROKER_ENV: &str = "FQ_TEST_ORPHAN_BROKER";
/// The scratch dir the outer test prepared for the helper's daemon.
const SCRATCH_ENV: &str = "FQ_TEST_ORPHAN_SCRATCH";

/// The file the helper writes its daemon's PID to once that daemon is at
/// steady state.
///
/// A file rather than the helper's stdout: libtest buffers a running
/// test's output and does not release it until the test *finishes*, and
/// this helper deliberately never finishes — piping its stdout yields
/// nothing at all while it parks. Renamed into place, so the parent
/// never reads a half-written number.
const PID_FILE: &str = "fqd.pid";

/// The window the daemon gets to die in. Generous for a `SIGKILL` the
/// kernel delivers at reparenting time, and far shorter than the many
/// seconds a broker-loss exit would take — so a pass here cannot be a
/// daemon that noticed something else.
const REAP_WINDOW: Duration = Duration::from_secs(1);

fn scratch_dir(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("fq-orphan-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(dir.join("cache")).expect("cache dir");
    std::fs::create_dir_all(dir.join("agents")).expect("agents dir");
    // An ephemeral edge port keeps this off the fixed default bind, like
    // every other daemon-spawning suite here.
    std::fs::write(dir.join("fq.toml"), "[edge]\nbind = \"127.0.0.1:0\"\n").expect("fq.toml");
    dir
}

/// True while `pid` is a live process still running `exe`.
///
/// Two questions in one, and both are load-bearing. `kill(pid, 0)`
/// answers neither: it succeeds for a **zombie**, and it succeeds for
/// whatever unrelated process the kernel has since given that PID to.
/// `/proc/<pid>/exe` settles the identity, and the state field of
/// `/proc/<pid>/stat` settles the liveness.
fn running_as(pid: u32, exe: &Path) -> bool {
    if !std::fs::read_link(format!("/proc/{pid}/exe")).is_ok_and(|link| link == exe) {
        return false;
    }
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // The `comm` field is parenthesised and may itself contain spaces and
    // parentheses, so the state is the first field after the *last* ')'.
    let Some((_, after_comm)) = stat.rsplit_once(')') else {
        return false;
    };
    !matches!(after_comm.split_whitespace().next(), Some("Z") | Some("X"))
}

#[test]
fn a_hard_killed_test_binary_takes_its_daemon_with_it() {
    if std::env::var_os(HELPER_ENV).is_some() {
        return; // helper mode runs the other test, not this one
    }
    let fqd = Path::new(env!("CARGO_BIN_EXE_fqd"));
    // Owned here on purpose: the helper's daemon must lose its parent and
    // nothing else.
    let broker = fq_test_support::NatsServer::start();
    let scratch = scratch_dir("outer");

    // The helper's stderr is kept, not discarded: when it fails to stand
    // a daemon up, its panic is the only account of why.
    let helper_err_path = scratch.join("helper.err");
    let helper_err = std::fs::File::create(&helper_err_path).expect("helper stderr log");
    let mut helper = TestChild::builder(std::env::current_exe().expect("current exe"))
        .args([
            "--exact",
            "helper_stands_a_daemon_up_and_parks",
            "--test-threads",
            "1",
        ])
        .env(HELPER_ENV, "1")
        .env(BROKER_ENV, broker.url())
        .env(SCRATCH_ENV, &scratch)
        .stdout(Stdio::null())
        .stderr(Stdio::from(helper_err))
        .spawn();

    let pid_file = scratch.join(PID_FILE);
    let deadline = Instant::now() + Duration::from_secs(90);
    let daemon_pid: u32 = loop {
        if let Ok(text) = std::fs::read_to_string(&pid_file) {
            break text.trim().parse().expect("the helper wrote a PID");
        }
        if let Some(status) = helper.try_wait().expect("poll the helper") {
            panic!(
                "the helper exited with {status:?} before reporting a daemon\n\
                 --- helper stderr ---\n{}",
                std::fs::read_to_string(&helper_err_path).unwrap_or_default()
            );
        }
        assert!(
            Instant::now() < deadline,
            "the helper never reported a daemon at steady state\n\
             --- helper stderr ---\n{}",
            std::fs::read_to_string(&helper_err_path).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(50));
    };

    assert!(
        running_as(daemon_pid, fqd),
        "pid {daemon_pid} should be the helper's live daemon before we kill anything"
    );

    // The failure this test exists for: the owner dies with no chance to
    // run a destructor, a signal handler, or anything else.
    helper.signal(libc::SIGKILL).expect("SIGKILL the helper");
    let _ = helper.wait();

    let reap_by = Instant::now() + REAP_WINDOW;
    while running_as(daemon_pid, fqd) {
        assert!(
            Instant::now() < reap_by,
            "fqd {daemon_pid} outlived the hard-killed process that started it — \
             this is #630: is PR_SET_PDEATHSIG still armed in fq_test_support::child?"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    drop(broker);
    let _ = std::fs::remove_dir_all(&scratch);
}

/// Helper mode: stand a daemon up through the fixture, say which PID it
/// got, and park until killed. A no-op in a normal run — only the
/// re-invocation above sets [`HELPER_ENV`].
#[test]
fn helper_stands_a_daemon_up_and_parks() {
    let Some(_) = std::env::var_os(HELPER_ENV) else {
        return;
    };
    let broker = std::env::var(BROKER_ENV).expect("the parent names its broker");
    let scratch =
        PathBuf::from(std::env::var_os(SCRATCH_ENV).expect("the parent names a scratch dir"));

    let log_path = scratch.join("daemon.log");
    let log = std::fs::File::create(&log_path).expect("create daemon log");
    let log_err = log.try_clone().expect("clone log handle");
    let mut daemon = TestChild::builder(env!("CARGO_BIN_EXE_fqd"))
        .env("FQ_DAEMON_CONFIG", scratch.join("fq.toml"))
        .env("FQ_NATS_URL", &broker)
        .env("FQ_CACHE_DIR", scratch.join("cache"))
        .env("FQ_STATE_DIR", scratch.join("state"))
        .env("FQ_AGENTS_DIR", scratch.join("agents"))
        .env("RUST_LOG", "off")
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .spawn();

    // Steady state before we report: a daemon killed while still booting
    // would prove nothing about the guard.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(status) = daemon.try_wait().expect("poll fqd") {
            let text = std::fs::read_to_string(&log_path).unwrap_or_default();
            panic!("fqd exited during startup with {status:?}\n--- log ---\n{text}");
        }
        if std::fs::read_to_string(&log_path)
            .unwrap_or_default()
            .contains("Runtime ready")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "fqd never reached 'Runtime ready'"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    let pid_file = scratch.join(PID_FILE);
    let staging = scratch.join("fqd.pid.partial");
    std::fs::write(&staging, daemon.id().to_string()).expect("write the PID");
    std::fs::rename(&staging, &pid_file).expect("publish the PID");

    // Park. The parent kills this process; nothing here gets to tidy up,
    // which is the whole point.
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}
