//! Stopping the hosted tasks, and spending the drain deadline on the
//! drain.
//!
//! **What was wrong (review B9).** The teardown computed the drain
//! deadline, then ran up to eight *sequential* five-second joins before
//! it joined the dispatcher against that same deadline — so a
//! stragglers-everywhere stop burned forty of the default hundred and
//! twenty seconds on infrastructure tasks that have nothing to suspend,
//! and the invocations the drain exists for got what was left. Worse,
//! the heartbeat producer was among the tasks stopped first, so the
//! worker went quiet at the exact moment it was still executing steps,
//! and a stale sweep firing mid-drain would see a worker that had
//! stopped answering.
//!
//! **The order now.** The dispatcher is told to stop first, and the
//! drain wait — the dispatcher's own join plus the recovery-resume
//! tasks, run concurrently — gets the whole deadline. Every other task
//! keeps running through it, the heartbeat producer included, so the
//! worker is alive on the roster for exactly as long as it is still
//! doing work. Only when the drain has ended does the rest stop, and
//! those joins run concurrently too.
//!
//! **The bound that gives.** A stop takes at most
//! `drain_deadline_ms` (nothing, if not draining, plus a five-second
//! dispatcher join) + five seconds for the auxiliary tasks + the MCP
//! shutdown and the two best-effort control-plane writes. It used to be
//! `drain_deadline_ms` + up to forty seconds + the same tail.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::task::JoinHandle;
use tokio::time::{Duration, Instant, timeout, timeout_at};

use crate::control_commands::{DownReceiver, wait_for_down_now};
use crate::signals::ShutdownSignals;

/// How long a task that is not invocation-bearing gets to stop once it
/// has been asked. These have nothing to suspend — they are consumers
/// and sweepers — so this is a bound on the shutdown handshake, not on
/// any work.
pub(crate) const AUXILIARY_JOIN: Duration = Duration::from_secs(5);

/// How the drain wait ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DrainOutcome {
    /// Everything suspended at a step boundary within the deadline.
    Suspended,
    /// The deadline elapsed; the stragglers are abandoned and the next
    /// start's recovery resumes them.
    DeadlineElapsed,
    /// A second signal, or `fq down --now`, asked for the immediate
    /// stop. Still clean — the teardown below this call runs in full.
    Escalated { reason: &'static str },
}

/// Wait — bounded — for the invocation-bearing work to suspend.
///
/// Three ways out, and the third is the one #509 was about: the
/// signal streams stay open through this wait, so a second SIGTERM (or
/// a Ctrl-C, or `fq down --now` on the control path) escalates the
/// running drain instead of being absorbed. The escalation is not a
/// force-abort — it returns here, and everything after it in the
/// teardown still runs, so the worker is deregistered and
/// `system.shutdown` is published exactly as on any other clean stop.
pub(crate) async fn wait_for_drain<E: std::fmt::Display>(
    deadline: Instant,
    dispatcher: JoinHandle<Result<(), E>>,
    resume_handles: Vec<JoinHandle<()>>,
    signals: &mut ShutdownSignals,
    down: &mut DownReceiver,
) -> DrainOutcome {
    let total = resume_handles.len();
    let suspended = Arc::new(AtomicUsize::new(0));
    let counted = resume_handles.into_iter().map(|handle| {
        let suspended = suspended.clone();
        async move {
            let _ = handle.await;
            suspended.fetch_add(1, Ordering::Relaxed);
        }
    });
    let drained = async {
        let (dispatcher_result, _) = tokio::join!(
            dispatcher,
            futures::future::join_all(counted.collect::<Vec<_>>()),
        );
        dispatcher_result
    };

    let outcome = tokio::select! {
        result = drained => {
            report_dispatcher(result);
            DrainOutcome::Suspended
        }
        _ = tokio::time::sleep_until(deadline) => DrainOutcome::DeadlineElapsed,
        reason = signals.next() => DrainOutcome::Escalated { reason },
        _ = wait_for_down_now(down) => DrainOutcome::Escalated { reason: "down_now" },
    };

    let suspended = suspended.load(Ordering::Relaxed);
    match outcome {
        DrainOutcome::Suspended => {
            if suspended > 0 {
                println!("  drained {suspended} in-flight invocation(s) cleanly.");
            }
        }
        DrainOutcome::DeadlineElapsed => {
            tracing::warn!(
                suspended,
                hard_stopped = total - suspended,
                "drain deadline elapsed; hard-stopped invocations will be resumed by \
                 recovery on the next start"
            );
        }
        DrainOutcome::Escalated { reason } => {
            println!();
            println!(
                "Escalating the drain to an immediate stop ({reason}) — \
                 tearing down cleanly and deregistering the worker."
            );
            tracing::warn!(
                reason,
                suspended,
                hard_stopped = total - suspended,
                "drain escalated to an immediate stop; invocations still running will \
                 be resumed by recovery on the next start"
            );
        }
    }
    outcome
}

fn report_dispatcher<E: std::fmt::Display>(result: Result<Result<(), E>, tokio::task::JoinError>) {
    match result {
        Ok(Ok(())) => println!("  trigger dispatcher stopped cleanly."),
        Ok(Err(err)) => tracing::error!(error = %err, "trigger dispatcher exited with error"),
        Err(err) => tracing::error!(error = %err, "trigger dispatcher task panicked"),
    }
}

/// The dispatcher's join when there is no drain to wait for — a
/// signal-driven fast stop, or a task failure.
pub(crate) async fn join_dispatcher<E: std::fmt::Display>(
    dispatcher: JoinHandle<Result<(), E>>,
    deadline: Instant,
) {
    match timeout_at(deadline, dispatcher).await {
        Ok(result) => report_dispatcher(result),
        Err(_) => tracing::warn!("trigger dispatcher did not shut down in time"),
    }
}

/// Join a task that answers with a `Result`, bounded by
/// [`AUXILIARY_JOIN`].
pub(crate) async fn join_fallible<E: std::fmt::Display>(
    name: &str,
    handle: JoinHandle<Result<(), E>>,
) {
    match timeout(AUXILIARY_JOIN, handle).await {
        Ok(Ok(Ok(()))) => println!("  {name} stopped cleanly."),
        Ok(Ok(Err(err))) => tracing::error!(task = name, error = %err, "task exited with error"),
        Ok(Err(err)) => tracing::error!(task = name, error = %err, "task panicked"),
        Err(_) => tracing::warn!(task = name, "task did not shut down within 5s"),
    }
}

/// [`join_fallible`], for a task that may not have been spawned at all
/// (the summariser is `[summary] model`-conditional).
pub(crate) async fn join_optional<E: std::fmt::Display>(
    name: &str,
    handle: Option<JoinHandle<Result<(), E>>>,
) {
    if let Some(handle) = handle {
        join_fallible(name, handle).await;
    }
}

/// Join a task that answers with `()` — a panic is the only way it can
/// fail, and it arrives as a `JoinError`.
pub(crate) async fn join_infallible(name: &str, handle: JoinHandle<()>) {
    match timeout(AUXILIARY_JOIN, handle).await {
        Ok(Ok(())) => println!("  {name} stopped cleanly."),
        Ok(Err(err)) => tracing::error!(task = name, error = %err, "task panicked"),
        Err(_) => tracing::warn!(task = name, "task did not shut down within 5s"),
    }
}

#[cfg(test)]
mod tests;
