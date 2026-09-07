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
//! 3. A one-shot stray report at first spawn: any **orphaned** process
//!    running the binary we are about to start is named on stderr.
//!    Orphaned, not merely present — cargo runs a crate's test binaries
//!    concurrently, so several live runs legitimately have a daemon up,
//!    and naming those is noise that trains the reader to skip the line.
//!    Never killed — somebody else's daemon is not ours to end — but a
//!    leak that was invisible for 27 hours now announces itself on the
//!    next run.
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
///
/// **Not** the daemon's drain deadline, which defaults to 180 s: this is
/// teardown, reached after the test's assertions are made, on daemons
/// that are idle by then — the drain they run has nothing to drain.
/// Several fixtures previously waited on that `SIGTERM` unboundedly, so
/// this is a deliberate change: a wedged child now costs five seconds
/// instead of hanging the suite. A test that means to observe a real
/// drain waits for it explicitly (`wait_timeout`) and does not leave the
/// question to `Drop`.
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
            pid: child.id(),
            child: Some(child),
            status: None,
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
///
/// **Reaping is not forgetting.** The exit status is kept, and every
/// query answers from it afterwards: `try_wait` still reports `Some`,
/// `wait` returns immediately, `wait_timeout` does not sit out its
/// deadline. `std::process::Child` caches the status for exactly this
/// reason, and the hand-rolled `wait_with_timeout` copies this fixture
/// replaced inherited that behaviour for free. A caller that reaches the
/// child through a path that happens to reap it — `wait_for_log_line`
/// returns normally when the daemon writes its line and exits inside one
/// poll interval — must not then be told "still running" by a `None` it
/// cannot distinguish from a live child.
pub struct TestChild {
    child: Option<Child>,
    /// What the child exited with, once it has. Survives the `Child`.
    status: Option<ExitStatus>,
    /// The PID it was given, which outlives the handle for the sake of a
    /// caller that captured it while the child was alive.
    pid: u32,
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
        //
        // stdin is the one exception, and it is a change: the daemon
        // sites used to inherit the test binary's stdin, which is the
        // developer's terminal under a bare `cargo test`. Nothing relied
        // on that — every pre-fixture `stdin(..)` call in this tree is on
        // an `fq`-client `Command` that stays a `Command` — and a daemon
        // holding a share of the runner's terminal is a hazard on its
        // own. A site that wants otherwise says so with `.stdin(..)`.
        command.stdin(Stdio::null());
        TestChildBuilder { program, command }
    }

    /// The PID the child was given.
    ///
    /// Still answered after the child has been reaped, because that is
    /// what a caller who captured it while the child was alive already
    /// holds. It is not a licence to signal: [`signal`](Self::signal)
    /// refuses once the child is reaped rather than firing at a number
    /// the kernel may have handed to somebody else.
    #[must_use]
    pub fn id(&self) -> u32 {
        self.pid
    }

    /// Poll for exit without blocking.
    ///
    /// Keeps answering `Some(status)` once the child has exited — see the
    /// type docs. Only a live child returns `None`.
    pub fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        let Some(child) = self.child.as_mut() else {
            return Ok(self.status);
        };
        let status = child.try_wait()?;
        if let Some(status) = status {
            self.status = Some(status);
            self.child = None; // reaped: the guard has nothing left to do
        }
        Ok(status)
    }

    /// Block until the child exits. Returns the remembered status
    /// immediately if it already has.
    pub fn wait(&mut self) -> std::io::Result<ExitStatus> {
        let Some(child) = self.child.as_mut() else {
            return self.status.ok_or_else(reaped_and_forgotten);
        };
        let status = child.wait()?;
        self.status = Some(status);
        self.child = None;
        Ok(status)
    }

    /// Send `signal` to the child. `Ok(())` means the kernel accepted it,
    /// not that the child acted on it.
    ///
    /// A reaped child is `ESRCH` — the same answer the kernel gives for a
    /// process that is gone, and emphatically not a signal aimed at a
    /// recycled PID.
    pub fn signal(&self, signal: i32) -> std::io::Result<()> {
        if self.child.is_none() {
            return Err(reaped_and_forgotten());
        }
        // SAFETY: `kill` on a live child PID this process owns. The child
        // is unreaped, so the PID is still ours and cannot have been
        // recycled.
        let rc = unsafe { libc::kill(self.pid as libc::pid_t, signal) };
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
        let status = loop {
            if let Ok(Some(status)) = child.try_wait() {
                break Some(status);
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                break child.wait().ok();
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        self.status = status.or(self.status);
        self.child = None;
    }
}

/// The child is gone and its status was never observed — only reachable
/// via [`TestChild::terminate`], which reaps without always being able to
/// collect a status.
fn reaped_and_forgotten() -> std::io::Error {
    std::io::Error::from_raw_os_error(libc::ESRCH)
}

impl Drop for TestChild {
    fn drop(&mut self) {
        self.terminate();
    }
}

/// This process's PID, to be captured *before* the fork and re-checked
/// after it. Read it in the parent: `getpid` in the child answers about
/// the child.
pub(crate) fn spawning_process() -> libc::pid_t {
    std::process::id() as libc::pid_t
}

/// The post-fork, pre-exec half of the guard, given the PID
/// [`spawning_process`] returned in the parent.
///
/// Its own function rather than a closure body because three call sites
/// need it and only one of them used to have the race check:
/// [`arm_death_guard`] below, [`crate::spawn_grouped`], and
/// [`crate::NatsServer`]. Two `pre_exec` bodies drifting from a third is
/// how the test broker kept a hole this module had already closed.
///
/// # Safety
///
/// Callable only between `fork` and `exec`, where nothing but
/// async-signal-safe syscalls is legal. `prctl`, `getppid` and `_exit`
/// all are, and nothing here allocates.
pub(crate) unsafe fn arm_in_child(parent: libc::pid_t) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: the caller guarantees the post-fork, pre-exec context.
        // `PR_SET_PDEATHSIG` survives the exec — it is cleared by `fork`,
        // and by `exec` of a set-user-ID binary, and neither `fqd` nor
        // `nats-server` is one.
        unsafe {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            // The race the death signal cannot cover on its own: if the
            // parent died between fork and now, the signal it would have
            // sent is already in the past and will never arrive. Losing
            // our parent is exactly the condition we were armed for, so
            // act on it directly.
            if libc::getppid() != parent {
                libc::_exit(1);
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        // Everywhere else there is no parent-death signal; `Drop` is the
        // whole guarantee. The suites that spawn daemons are Linux-only
        // in practice (the pinned broker binary), so this costs nothing
        // today and keeps the crate compiling if that changes.
        let _ = parent;
    }
    Ok(())
}

/// Arm the guard on `command`: the kernel kills the child when the thread
/// that forked it dies.
pub(crate) fn arm_death_guard(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;

    let parent = spawning_process();
    // SAFETY: `pre_exec` runs the closure between fork and exec, which is
    // exactly `arm_in_child`'s contract.
    unsafe {
        command.pre_exec(move || arm_in_child(parent));
    }
}

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
         reparented to a reaper above this run. A previous run leaked them. \
         Nothing was killed; see #630 and kill by exe path, never by name.",
        strays.len(),
        program.display(),
    );
}

/// Every PID whose executable is exactly `program` and which has been
/// **reparented to a reaper above this run**.
///
/// The exe path is the only reliable identity here: the dogfood daemon
/// and every other worktree's daemon are all called `fqd`, and acting on
/// a name is how the wrong process gets hit.
///
/// "Orphaned" is not "parent is PID 1". PID 1 is only the reaper when
/// nothing closer claimed the role: a systemd user scope, a
/// `docker run --init`, or any `PR_SET_CHILD_SUBREAPER` process adopts
/// its descendants' orphans instead, and a rule written as `ppid == 1`
/// silently reports nothing there. Every such reaper is by construction
/// an *ancestor of ours*, so that is the test — and it keeps the
/// discrimination that matters: a daemon belonging to a live sibling test
/// binary has that binary as its parent, which is not on our ancestor
/// chain, so a concurrent `cargo test` is never mistaken for a leak.
#[cfg(target_os = "linux")]
fn orphans_running(program: &Path) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let reapers = ancestors_of_this_process();
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
        if parent_pid(pid).is_some_and(|ppid| reapers.contains(&ppid)) {
            strays.push(pid);
        }
    }
    strays
}

/// Our strict ancestors, nearest first, ending at PID 1.
///
/// Bounded by a hop limit rather than trusting the chain to terminate:
/// this walks live kernel state that can change under us, and a test
/// helper has no business looping forever over `/proc`.
#[cfg(target_os = "linux")]
fn ancestors_of_this_process() -> Vec<u32> {
    let mut chain = Vec::new();
    let mut pid = std::process::id();
    for _ in 0..64 {
        let Some(parent) = parent_pid(pid) else { break };
        if parent == 0 || chain.contains(&parent) {
            break;
        }
        chain.push(parent);
        if parent == 1 {
            break;
        }
        pid = parent;
    }
    chain
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

    /// Reaping must not lose the answer. Before this, `try_wait` reported
    /// `None` after an exit — indistinguishable from "still running" — so
    /// `wait_timeout` sat out its whole deadline and returned `None`
    /// ("the daemon did not exit") about a daemon that had exited, and
    /// `signal` panicked where the kernel would have said ESRCH. That is
    /// reachable: `daemon_shutdown::wait_for_log_line` returns normally
    /// with the child already reaped when the daemon writes its line and
    /// exits inside one poll interval, and its callers then ask
    /// `wait_timeout` what happened.
    #[test]
    fn a_reaped_child_still_answers_for_its_exit() {
        let mut child = TestChild::builder("/bin/true").spawn();
        let waited = child.wait().expect("wait");
        assert!(waited.success());

        assert_eq!(
            child.try_wait().expect("try_wait after reaping"),
            Some(waited),
            "try_wait must keep reporting the exit, not None"
        );

        let started = Instant::now();
        assert_eq!(
            child.wait_timeout(Duration::from_secs(3)),
            Some(waited),
            "wait_timeout must answer from the remembered status"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "wait_timeout sat out its deadline on an already-reaped child"
        );
        assert_eq!(child.wait().expect("wait again"), waited);

        let refused = child.signal(libc::SIGTERM).expect_err("a reaped child");
        assert_eq!(
            refused.raw_os_error(),
            Some(libc::ESRCH),
            "signalling a reaped child is ESRCH, never a panic and never a \
             signal at a recycled PID"
        );
    }

    /// The stray report must not fire on a *live* run's children, or it
    /// becomes noise: cargo runs a crate's test binaries concurrently, so
    /// several daemons are legitimately up at any moment. Only a process
    /// reparented to a reaper above this run is the tell.
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

    /// The reaper set the stray rule keys on: our own strict ancestors,
    /// ending at PID 1. `ppid == 1` was the first version and is only the
    /// bottom of this chain — under a systemd user scope or a
    /// `docker run --init` the reaper is nearer, and a rule written as
    /// `== 1` reports nothing at all there.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_reaper_set_is_our_ancestor_chain_not_just_pid_1() {
        let chain = ancestors_of_this_process();
        assert_eq!(
            chain.last(),
            Some(&1),
            "the chain must reach init: {chain:?}"
        );
        assert_eq!(
            chain.first().copied(),
            parent_pid(std::process::id()),
            "and start at our own parent"
        );
        assert!(
            !chain.contains(&std::process::id()),
            "strict ancestors only — our own children's parent is us, and \
             they are not leaks"
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
