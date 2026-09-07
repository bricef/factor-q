//! A spawned test child that cannot outlive the test binary that started
//! it (#630).
//!
//! **The incident.** Twenty-odd integration tests under
//! `fq-daemon/tests/` started the daemon with a bare
//! `Command::new(env!("CARGO_BIN_EXE_fqd"))` and relied on `impl Drop`
//! (or an ad-hoc `kill`) to stop it. `Drop` is a *language* mechanism: it
//! runs on return and on unwind, and not at all when the process it lives
//! in is killed by a signal. So an OOM kill (exit 137, seen twice in one
//! sprint when two gates built concurrently), a `SIGKILL` from a stopped
//! agent, or any hard stop of the test binary left every daemon it had
//! started running forever, reparented to PID 1. The devbox was found
//! carrying 42 of them — 1.3 GB resident, each holding an edge listener —
//! 27 hours after the run that spawned them, from worktrees that no
//! longer existed.
//!
//! `std::process::Child` will never do this for us: its own `Drop` is
//! documented to *not* kill the child.
//!
//! **The fix, in three parts.**
//!
//! 1. [`PR_SET_PDEATHSIG`](https://man7.org/linux/man-pages/man2/prctl.2.html)
//!    = `SIGKILL` in `pre_exec`, so the kernel — not our code — reaps the
//!    child when the spawning thread dies. This is the part `Drop` cannot
//!    do, because there is no code path left to run.
//! 2. A `Drop` that still stops the child politely on the normal path
//!    (`SIGTERM`, a grace period, then `SIGKILL`), in *one* implementation
//!    rather than the five near-copies that let the omission spread.
//! 3. A one-shot stray report at first spawn: any process already running
//!    the binary we are about to start is named on stderr. Never killed —
//!    a parallel run's daemon is not ours to end — but a leak that was
//!    invisible for 27 hours now announces itself on the next run.
//!
//! **The `PDEATHSIG` pitfall.** The signal fires when the spawning
//! *thread* exits, not when the process does. A child spawned from a
//! thread that ends early (a `spawn_blocking` worker that idles out, a
//! scoped thread, a `OnceLock` initialiser that happens to run on a
//! short-lived thread) dies with that thread — mid-test, for no visible
//! reason. Every daemon spawn in this tree is on a thread that outlives
//! the guard: a `#[test]` body, a current-thread `#[tokio::test]` body,
//! or a `multi_thread` runtime worker, which lives until the runtime is
//! dropped at the end of the test. **Do not** call [`TestChild::builder`]
//! from `spawn_blocking`, from a scoped thread, or from a shared
//! `OnceLock`/`lazy_static` fixture without moving the spawn onto a
//! thread that owns the child's whole life.
//!
//! **Not a process group.** The child stays in the test binary's process
//! group on purpose. GNU `timeout` (the standing way these gates are run)
//! signals the whole group, which delivers the daemon a graceful
//! `SIGTERM` it knows how to drain; `setpgid` would take that away and
//! leave only the `SIGKILL` path. The group is the polite route and
//! `PDEATHSIG` is the backstop, and they compose.

use std::ffi::OsStr;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

/// How long a dropped child gets to honour `SIGTERM` before `SIGKILL`.
/// Long enough for `fqd`'s drain to finish on a loaded box, short enough
/// that a wedged child cannot hang the suite the way an unbounded
/// `wait()` would.
const TERM_GRACE: Duration = Duration::from_secs(5);

/// Builder for [`TestChild`]. Mirrors the slice of [`Command`] the test
/// tree actually uses, so a spawn site reads the way it always did —
/// what it no longer gets to do is skip the guard.
pub struct TestChildBuilder {
    program: PathBuf,
    command: Command,
}

impl TestChildBuilder {
    /// Add one argument.
    #[must_use]
    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.command.arg(arg);
        self
    }

    /// Add several arguments.
    #[must_use]
    pub fn args(mut self, args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Self {
        self.command.args(args);
        self
    }

    /// Set one environment variable.
    #[must_use]
    pub fn env(mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.command.env(key, value);
        self
    }

    /// Run the child in `dir`.
    #[must_use]
    pub fn current_dir(mut self, dir: impl AsRef<Path>) -> Self {
        self.command.current_dir(dir);
        self
    }

    /// Configure the child's stdin.
    #[must_use]
    pub fn stdin(mut self, cfg: impl Into<Stdio>) -> Self {
        self.command.stdin(cfg);
        self
    }

    /// Configure the child's stdout.
    #[must_use]
    pub fn stdout(mut self, cfg: impl Into<Stdio>) -> Self {
        self.command.stdout(cfg);
        self
    }

    /// Configure the child's stderr.
    #[must_use]
    pub fn stderr(mut self, cfg: impl Into<Stdio>) -> Self {
        self.command.stderr(cfg);
        self
    }

    /// Spawn the child under the death guard.
    ///
    /// Panics with the program path if the spawn fails — a test that
    /// cannot start the process it is about to assert on has not passed.
    #[must_use]
    pub fn spawn(mut self) -> TestChild {
        report_strays_once(&self.program);
        arm_death_guard(&mut self.command);
        let child = self
            .command
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {}: {e}", self.program.display()));
        TestChild {
            child: Some(child),
            program: self.program,
        }
    }

    /// Run the child to completion and collect its output.
    ///
    /// The guard still matters here: `output()` blocks, so a child that
    /// never exits would otherwise outlive a hard-killed test binary
    /// exactly like a spawned one.
    #[must_use]
    pub fn output(mut self) -> Output {
        report_strays_once(&self.program);
        arm_death_guard(&mut self.command);
        self.command
            .output()
            .unwrap_or_else(|e| panic!("run {}: {e}", self.program.display()))
    }
}

/// A running child process that dies with this test binary.
///
/// Exposes the slice of [`Child`] the tests use. Once the child has been
/// reaped — by [`wait`](Self::wait) or by a [`try_wait`](Self::try_wait)
/// that reported an exit — the guard disarms itself, so `Drop` can never
/// signal a PID the kernel has since handed to somebody else.
pub struct TestChild {
    child: Option<Child>,
    program: PathBuf,
}

impl TestChild {
    /// Start building a child that runs `program`.
    #[must_use]
    pub fn builder(program: impl AsRef<Path>) -> TestChildBuilder {
        let program = program.as_ref().to_path_buf();
        let mut command = Command::new(&program);
        // A test's environment is the developer's environment. Tests set
        // what they mean to set; inheriting the rest is deliberate (the
        // suites rely on RUST_BACKTRACE, PATH and the like) and matches
        // what these sites did before the fixture existed.
        command.stdin(Stdio::null());
        TestChildBuilder { program, command }
    }

    /// The child's PID, for a test that signals it by hand.
    ///
    /// Panics once the child has been reaped: the PID is meaningless
    /// then, and silently returning a stale one is how a teardown ends
    /// up killing an unrelated process.
    #[must_use]
    pub fn id(&self) -> u32 {
        self.alive().id()
    }

    /// Poll for exit without blocking.
    pub fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        let Some(child) = self.child.as_mut() else {
            return Ok(None);
        };
        let status = child.try_wait()?;
        if status.is_some() {
            self.child = None; // reaped: the guard has nothing left to do
        }
        Ok(status)
    }

    /// Block until the child exits.
    pub fn wait(&mut self) -> std::io::Result<ExitStatus> {
        let status = self.alive_mut().wait();
        if status.is_ok() {
            self.child = None;
        }
        status
    }

    /// Send `signal` to the child. `Ok(())` means the kernel accepted it,
    /// not that the child acted on it.
    pub fn signal(&self, signal: i32) -> std::io::Result<()> {
        // SAFETY: `kill` on a live child PID this process owns. The child
        // is unreaped (`alive` panics otherwise), so the PID is still
        // ours and cannot have been recycled.
        let rc = unsafe { libc::kill(self.id() as libc::pid_t, signal) };
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    /// Take the child's piped stdout, for a test that reads it.
    #[must_use]
    pub fn take_stdout(&mut self) -> Option<std::process::ChildStdout> {
        self.child.as_mut().and_then(|c| c.stdout.take())
    }

    /// Take the child's piped stdin, for a test that writes to it.
    #[must_use]
    pub fn take_stdin(&mut self) -> Option<std::process::ChildStdin> {
        self.child.as_mut().and_then(|c| c.stdin.take())
    }

    /// Wait up to `timeout` for the child to exit, returning `None` if it
    /// outlasts that — after killing it, so the caller's failure message
    /// is not also a leak.
    pub fn wait_timeout(&mut self, timeout: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) => {
                    if Instant::now() >= deadline {
                        self.terminate();
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => panic!("try_wait on {}: {e}", self.program.display()),
            }
        }
    }

    /// `SIGTERM`, a grace period, then `SIGKILL` — and reap either way.
    /// Idempotent: a child already reaped is left alone.
    pub fn terminate(&mut self) {
        let Some(child) = self.child.as_mut() else {
            return;
        };
        // SAFETY: the child is unreaped, so its PID is still ours. A
        // failed `kill` (it exited a moment ago) is fine during teardown.
        unsafe {
            libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
        }
        let deadline = Instant::now() + TERM_GRACE;
        loop {
            if matches!(child.try_wait(), Ok(Some(_))) {
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        self.child = None;
    }

    fn alive(&self) -> &Child {
        match self.child.as_ref() {
            Some(child) => child,
            None => panic!("{} has already been reaped", self.program.display()),
        }
    }

    fn alive_mut(&mut self) -> &mut Child {
        match self.child.as_mut() {
            Some(child) => child,
            None => panic!("{} has already been reaped", self.program.display()),
        }
    }
}

impl Drop for TestChild {
    fn drop(&mut self) {
        self.terminate();
    }
}

/// Arm the guard on `command`: the kernel kills the child when the thread
/// that forked it dies.
#[cfg(target_os = "linux")]
fn arm_death_guard(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;

    let parent = std::process::id() as libc::pid_t;
    // SAFETY: the closure runs between fork and exec, where only
    // async-signal-safe calls are legal. `prctl`, `getppid` and `_exit`
    // all are, and nothing here allocates. `PR_SET_PDEATHSIG` survives
    // the exec (it is cleared only by fork, and by exec of a set-user-ID
    // binary — `fqd` is neither).
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            // The race the guard cannot cover on its own: if the parent
            // died between fork and now, the death signal it would have
            // sent is already in the past and will never arrive. Losing
            // our parent is exactly the condition we were armed for, so
            // act on it directly.
            if libc::getppid() != parent {
                libc::_exit(1);
            }
            Ok(())
        });
    }
}

/// Everywhere else there is no parent-death signal; `Drop` is the whole
/// guarantee. The suites that spawn daemons are Linux-only in practice
/// (the pinned broker binary), so this costs nothing today and keeps the
/// crate compiling if that changes.
#[cfg(not(target_os = "linux"))]
fn arm_death_guard(_command: &mut Command) {}

/// Report — once per program, per test binary — any **orphaned** process
/// already running `program`.
///
/// Orphaned, not merely present: cargo runs a crate's test binaries
/// concurrently, so at any moment several live runs legitimately have a
/// daemon up, and reporting those would be noise that trains the reader
/// to ignore the line. A leak has a signature — the 42 orphans of #630
/// were all reparented to PID 1 — and that is what this looks for.
///
/// Deliberately *not* a kill: a daemon that got there another way is not
/// ours to end, and the failure mode of a wrong kill is far worse than
/// the failure mode of a wrong warning.
fn report_strays_once(program: &Path) {
    use std::sync::{Mutex, OnceLock};
    static SEEN: OnceLock<Mutex<Vec<PathBuf>>> = OnceLock::new();
    let mut seen = SEEN
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if seen.iter().any(|p| p == program) {
        return;
    }
    seen.push(program.to_path_buf());
    drop(seen);
    report_strays(program);
}

#[cfg(target_os = "linux")]
fn report_strays(program: &Path) {
    let strays = orphans_running(program);
    if strays.is_empty() {
        return;
    }
    // Straight to the file descriptor: libtest captures the `eprintln!`
    // macro per test and shows it only when that test fails, which is
    // precisely when nobody is looking for somebody else's leak.
    let _ = writeln!(
        std::io::stderr(),
        "warning: {} orphaned process(es) are running {} — pid(s) {strays:?}, \
         all reparented to PID 1. A previous run leaked them. Nothing was \
         killed; see #630 and kill by exe path, never by name.",
        strays.len(),
        program.display(),
    );
}

/// Every PID whose executable is exactly `program` and whose parent is
/// PID 1.
///
/// The exe path is the only reliable identity here: the dogfood daemon
/// and every other worktree's daemon are all called `fqd`, and acting on
/// a name is how the wrong process gets hit.
#[cfg(target_os = "linux")]
fn orphans_running(program: &Path) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let me = std::process::id();
    let mut strays = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        if pid == me {
            continue;
        }
        // Unreadable (another user's, or exited under us) is not news.
        if !std::fs::read_link(entry.path().join("exe")).is_ok_and(|exe| exe == program) {
            continue;
        }
        if parent_pid(pid) == Some(1) {
            strays.push(pid);
        }
    }
    strays
}

/// The parent PID from `/proc/<pid>/stat` — the fourth field, after a
/// `comm` that may itself contain spaces and parentheses, hence the split
/// on the *last* `')'`.
#[cfg(target_os = "linux")]
fn parent_pid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, after_comm) = stat.rsplit_once(')')?;
    after_comm.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(not(target_os = "linux"))]
fn report_strays(_program: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// True while `pid` is a process that could still do something.
    ///
    /// `kill(pid, 0)` is not that question: a killed child whose parent
    /// process is still alive and has not reaped it stays a **zombie**,
    /// and signalling a zombie succeeds. The kernel's own answer is the
    /// state field of `/proc/<pid>/stat` — third field, after a `comm`
    /// that may itself contain spaces and parentheses, hence the split
    /// on the last `')'`.
    #[cfg(target_os = "linux")]
    fn still_running(pid: u32) -> bool {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false; // reaped and gone
        };
        let Some((_, after_comm)) = stat.rsplit_once(')') else {
            return false;
        };
        !matches!(after_comm.split_whitespace().next(), Some("Z") | Some("X"))
    }

    /// A child that outlives the guard's `Drop` still dies with the
    /// thread that forked it — the contract `Drop` cannot provide, and
    /// the one the 42 orphans of #630 needed. `mem::forget` stands in
    /// for the `Drop` a `SIGKILL`ed test binary never runs.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_child_dies_with_the_thread_that_spawned_it() {
        let pid = std::thread::spawn(|| {
            let child = TestChild::builder("/bin/sleep").arg("600").spawn();
            let pid = child.id();
            std::mem::forget(child);
            pid
        })
        .join()
        .expect("spawn thread");

        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if !still_running(pid) {
                return; // dead, as required
            }
            assert!(
                Instant::now() < deadline,
                "pid {pid} outlived the thread that spawned it — is PR_SET_PDEATHSIG armed?"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The normal path still works: `Drop` stops a live child.
    #[cfg(target_os = "linux")]
    #[test]
    fn drop_stops_a_live_child() {
        let pid = {
            let child = TestChild::builder("/bin/sleep").arg("600").spawn();
            child.id()
        };
        assert!(
            !still_running(pid),
            "Drop must stop the child it guards (pid {pid})"
        );
    }

    /// A child waited for by hand disarms the guard, so `Drop` has no PID
    /// left to signal — the alternative is teardown killing whatever the
    /// kernel handed that number to next.
    #[test]
    fn an_explicit_wait_disarms_the_guard() {
        let mut child = TestChild::builder("/bin/true").spawn();
        let status = child.wait().expect("wait");
        assert!(status.success());
        assert!(
            child.child.is_none(),
            "a reaped child must disarm the guard"
        );
    }

    /// The stray report must not fire on a *live* run's children, or it
    /// becomes noise: cargo runs a crate's test binaries concurrently, so
    /// several daemons are legitimately up at any moment. Only an orphan
    /// — reparented to PID 1 — is the tell.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_live_childs_process_is_not_reported_as_a_stray() {
        let child = TestChild::builder("/bin/sleep").arg("600").spawn();
        let pid = child.id();
        assert!(
            !orphans_running(Path::new("/bin/sleep")).contains(&pid),
            "pid {pid} has a live parent and must not be reported as leaked"
        );
    }

    /// `output()` is a spawn too, guard and all.
    #[test]
    fn output_collects_the_childs_streams() {
        let out = TestChild::builder("/bin/echo").arg("hello").output();
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hello");
    }
}
