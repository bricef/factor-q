//! The daemon's own row in the control-plane worker table, and the
//! promise that *something* will say what became of it.
//!
//! Registration is a side effect on shared state: from the moment it
//! lands, the coordination consumer's stale sweep is watching this
//! process, and an exit that says nothing leaves a row sitting `alive`
//! until the sweep flips it to `stale` — the accumulation the review
//! calls B5. The teardown in `hosted.rs` has always dealt with the
//! paths it owns. What it could not see is the `?` above it: a failed
//! recovery scan, an uncovered model in the pricing table, an edge
//! identity that would not load. Each of those returned straight out of
//! `run_daemon` past every teardown, and each left a row behind.
//!
//! The fix is structural rather than a call added at each `?`. This
//! type is armed by the registration itself and can be settled exactly
//! once; `run_daemon` settles it after everything that could fail has
//! either failed or handed the row to a teardown that settled it first.
//! A new fallible step cannot be added in the wrong place, because
//! there is no wrong place left: every path out passes the same line.
//!
//! **`shutdown`, not `stale`, on an error exit.** The row records what
//! the *process* did, and a daemon that fails a startup step and
//! returns has stopped deliberately. `stale` is reserved for the
//! honest case — a process that stopped answering without saying
//! anything — which is why the task-failure teardown still declines to
//! mark it (see [`WorkerRegistration::settle`]).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Context;
use fq_runtime::ControlPlaneStore;
use fq_runtime::worker::WorkerId;

/// A live worker row, and the flag that says nobody has yet accounted
/// for it.
///
/// Cheap to clone — the arming flag is shared, so the teardown's copy
/// and `run_daemon`'s copy settle the same registration and the second
/// one to try is a no-op.
#[derive(Clone)]
pub(crate) struct WorkerRegistration {
    cp_store: Arc<ControlPlaneStore>,
    worker_id: WorkerId,
    outstanding: Arc<AtomicBool>,
}

impl WorkerRegistration {
    /// Self-register this daemon's worker side with the control plane
    /// (v1 single-process: the daemon plays both roles), arming the
    /// guard on success.
    ///
    /// A registration that fails arms nothing: there is no row, so
    /// there is nothing to account for.
    pub(crate) async fn register(
        cp_store: Arc<ControlPlaneStore>,
        worker_id: WorkerId,
        host_label: &str,
        now_ms: i64,
    ) -> anyhow::Result<Self> {
        cp_store
            .register_worker(worker_id.as_str(), host_label, now_ms)
            .await
            .context("failed to self-register worker with control-plane")?;
        Ok(Self {
            cp_store,
            worker_id,
            outstanding: Arc::new(AtomicBool::new(true)),
        })
    }

    /// A teardown's own verdict on the row.
    ///
    /// `clean` marks it `shutdown`. An unclean exit — a hosted task
    /// died and took the runtime with it — deliberately leaves the row
    /// alone so the stale sweep reports it, which is the honest signal
    /// that this daemon did not exit cleanly. Either way the row is
    /// accounted for and the catch-all below will not touch it.
    pub(crate) async fn settle(&self, clean: bool) {
        if !self.claim() {
            return;
        }
        if clean {
            self.mark_shutdown().await;
        }
    }

    /// The catch-all: a path out that reached no teardown at all.
    ///
    /// Always marks `shutdown`, because the process is leaving on
    /// purpose — an error return is a decision, not a disappearance.
    /// A no-op if a teardown already settled the row.
    pub(crate) async fn settle_unclaimed(&self) {
        if !self.claim() {
            return;
        }
        tracing::warn!(
            worker = %self.worker_id,
            "daemon exited before its teardown ran; deregistering the worker so its \
             row does not age into `stale`"
        );
        self.mark_shutdown().await;
    }

    /// True for exactly one caller — whoever takes responsibility for
    /// the row first.
    fn claim(&self) -> bool {
        self.outstanding.swap(false, Ordering::SeqCst)
    }

    /// Best-effort by design: a control plane that cannot be reached on
    /// the way out must never turn a stop into a hang. The stale sweep
    /// is the backstop for exactly this case.
    async fn mark_shutdown(&self) {
        if let Err(err) = self
            .cp_store
            .mark_worker_shutdown(self.worker_id.as_str())
            .await
        {
            tracing::warn!(error = %err, "failed to mark worker as gracefully shut down");
        }
    }
}

#[cfg(test)]
mod tests;
