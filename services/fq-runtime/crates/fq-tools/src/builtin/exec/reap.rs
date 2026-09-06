//! Process-group teardown for the [`exec`](super) tool.
//!
//! The tool spawns its child as the leader of a **new process group**
//! (`process_group(0)`), so everything the command starts — a shell's
//! background jobs, a build's compilers, a test runner's own children —
//! inherits that group and can be ended as one. Ending only the direct
//! child leaves the rest running as the daemon's user with nothing
//! watching them: a timed-out `["bash", "-c", "<payload> & sleep 999"]`
//! used to leak `<payload>` indefinitely (review finding A9,
//! <https://github.com/bricef/factor-q/issues/552>).
//!
//! Two paths end a group, and both are needed:
//!
//! - **Timeout** — [`kill_group`] escalates: `SIGTERM` to the group, a
//!   bounded grace (`ExecConfig::kill_grace`) for it to leave on its
//!   own terms, then `SIGKILL` for whatever is left.
//! - **Drop** — [`GroupGuard`] sends `SIGKILL` to the group if the
//!   tool's future is dropped before the child is reaped, which is how
//!   a host-side deadline outside the tool cancels a call
//!   (<https://github.com/bricef/factor-q/issues/547>). `kill_on_drop`
//!   stays on as the backstop, but on its own it would again reap only
//!   the direct child.
//!
//! A command that **exits on its own is not group-killed**: the guard
//! disarms on the reap, so a descendant the agent deliberately
//! daemonized outlives the call, and the bounded output drain
//! (<https://github.com/bricef/factor-q/issues/176>) is what stops that
//! descendant holding the tool open. The drain bound also still earns
//! its place after a group kill, because a descendant that gave itself
//! a new session (`setsid`) leaves the group and keeps the pipe.
//!
//! Addressing a group by id is safe here because the id is the leader's
//! pid, and the kernel keeps a pid allocated while any process still
//! carries it as a group id — so the id cannot be recycled under us
//! while there is anything left to kill.

use std::time::{Duration, Instant};

use tokio::process::{Child, Command};
use tokio::time::timeout;
use tracing::warn;

#[cfg(unix)]
use libc::{SIGKILL, SIGTERM};

/// Unused off Unix, where [`signal_group`] is a no-op and [`group_id`]
/// never yields a group; defined so the call sites stay `cfg`-free.
#[cfg(not(unix))]
const SIGTERM: i32 = 15;
#[cfg(not(unix))]
const SIGKILL: i32 = 9;

/// How often the group is probed while waiting out the kill grace.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Make the child the leader of its own process group, so its whole
/// tree can be signalled as one. Call before spawning.
#[cfg(unix)]
pub(super) fn lead_own_group(cmd: &mut Command) {
    // Not `setsid`: a new session would also detach the child from the
    // controlling terminal and is more than this needs. A process group
    // is exactly the unit we want to kill.
    cmd.process_group(0);
}

/// Process groups are a Unix concept; elsewhere the direct child is the
/// only handle there is, which is the behaviour every path below falls
/// back to when [`group_id`] yields `None`.
#[cfg(not(unix))]
pub(super) fn lead_own_group(_cmd: &mut Command) {}

/// The group id of a child spawned after [`lead_own_group`] — its own
/// pid. `None` once the child has been reaped (its pid is gone) or
/// where process groups do not exist.
///
/// Asked of the kernel rather than assumed: a child that is not leading
/// a group of its own must not be torn down *as* one, or a `killpg`
/// aimed at a group that was never created would report success at
/// having killed nothing. Every path below then falls back to the
/// direct child, which is where this tool started.
#[cfg(unix)]
pub(super) fn group_id(child: &Child) -> Option<i32> {
    let pid = child.id()? as i32;
    // SAFETY: `getpgid` reads scheduler bookkeeping for one pid and
    // reports failure through errno. An unreaped child still answers,
    // zombie or not.
    let leads_own_group = unsafe { libc::getpgid(pid) } == pid;
    leads_own_group.then_some(pid)
}

#[cfg(not(unix))]
pub(super) fn group_id(_child: &Child) -> Option<i32> {
    None
}

/// Kills the child's process group if the tool's future is dropped
/// before the child is reaped.
///
/// Declare it *after* the `Child` so it drops *before* it: the group is
/// signalled while the leader is still unreaped, which is what keeps the
/// group id from being recycled between the signal and its delivery.
pub(super) struct GroupGuard {
    /// `None` once disarmed, or when there is no group to kill.
    pgid: Option<i32>,
}

impl GroupGuard {
    pub(super) fn new(pgid: Option<i32>) -> Self {
        Self { pgid }
    }

    /// Stop guarding. Called once the child has been reaped: either it
    /// exited on its own — in which case its descendants are the
    /// agent's business, not ours — or [`kill_group`] has already ended
    /// the group.
    pub(super) fn disarm(&mut self) {
        self.pgid = None;
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if let Some(pgid) = self.pgid {
            warn!(
                pgid,
                "exec call dropped before its child was reaped — killing the process group"
            );
            signal_group(pgid, SIGKILL);
        }
    }
}

/// End the whole group after a timeout, then reap the direct child.
///
/// `SIGTERM` first so a well-behaved tree can flush and exit, then
/// `SIGKILL` for whatever is still there when `grace` runs out. The
/// grace ends early once the group is observably empty, so the common
/// case costs a poll interval rather than the whole window.
pub(super) async fn kill_group(child: &mut Child, pgid: Option<i32>, grace: Duration) {
    let Some(pgid) = pgid else {
        // No group to address: the direct child is all we can reach.
        if let Err(err) = child.start_kill() {
            warn!(error = %err, "failed to kill the timed-out child");
        }
        let _ = child.wait().await;
        return;
    };

    let deadline = Instant::now() + grace;
    signal_group(pgid, SIGTERM);
    // The leader has to be reaped before the group can ever be observed
    // empty: a zombie still belongs to its group.
    let _ = timeout(grace, child.wait()).await;
    while Instant::now() < deadline {
        if !group_alive(pgid) {
            return;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    if group_alive(pgid) {
        warn!(
            pgid,
            grace_ms = grace.as_millis() as u64,
            "exec process group outlived SIGTERM — escalating to SIGKILL"
        );
        signal_group(pgid, SIGKILL);
    }
    // A no-op if the wait above already reaped it.
    let _ = child.wait().await;
}

/// Send `signal` to every process in the group.
#[cfg(unix)]
pub(super) fn signal_group(pgid: i32, signal: i32) {
    // SAFETY: `killpg` takes any group id and reports failure through
    // errno rather than memory; an already-empty group (ESRCH) is the
    // expected no-op on these paths.
    unsafe { libc::killpg(pgid, signal) };
}

#[cfg(not(unix))]
pub(super) fn signal_group(_pgid: i32, _signal: i32) {}

/// Whether any process still belongs to the group — zombies included,
/// which is why the leader is reaped before this is asked.
#[cfg(unix)]
pub(super) fn group_alive(pgid: i32) -> bool {
    // SAFETY: signal 0 runs the existence and permission checks without
    // delivering anything.
    let rc = unsafe { libc::killpg(pgid, 0) };
    // EPERM means the group is there but not ours to signal; only ESRCH
    // says it is gone.
    rc == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(not(unix))]
pub(super) fn group_alive(_pgid: i32) -> bool {
    false
}
