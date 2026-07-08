//! Graceful-drain signalling (ADR-0027).
//!
//! A *drain* tells a worker to stop starting new steps and let each
//! in-flight invocation suspend at its next step boundary — its state
//! already checkpointed to the WAL — so the next binary's recovery can
//! resume it. See
//! `docs/plans/active/2026-07-08-adr-0027-graceful-drain-execution.md`.
//!
//! This module is the *domain* vocabulary for that signal; it
//! deliberately carries no transport. In v1 the control-plane flips the
//! flag in-process through [`Worker::request_drain`](crate::Worker); in
//! v2 a remote-worker adapter reconstructs the same flag on the worker
//! node from a [`DrainRequest`] received over NATS. Either way the
//! reducer loop only ever polls the worker-local [`DrainSignal`] — the
//! signal is a trait-routed domain type, the flag is a private
//! implementation detail, never a shared object spanning the boundary.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Why a worker is being drained — recorded for the audit trail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainReason {
    /// A deploy is replacing the binary (ADR-0027).
    Deploy,
    /// An operator requested a drain directly.
    Operator,
}

/// A request to drain a worker. The domain command carried across the
/// `Worker` trait; the bounded-wait *deadline* is the orchestrator's
/// concern (it decides how long to wait before hard-stopping), not the
/// worker's, so it is not carried here.
#[derive(Debug, Clone)]
pub struct DrainRequest {
    pub reason: DrainReason,
}

impl DrainRequest {
    pub fn new(reason: DrainReason) -> Self {
        Self { reason }
    }
}

/// The observable drain state of a worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DrainState {
    /// Normal operation; new steps proceed.
    #[default]
    Running,
    /// A drain has been requested; in-flight invocations suspend at
    /// their next step boundary and no new step is started.
    Draining,
}

/// A cheap, cloneable, thread-safe drain flag shared between the
/// control-plane-facing `request_drain` and the reducer step loop that
/// polls it. Cloning shares the same underlying flag (an `Arc` inside),
/// so a single request drains every in-flight invocation on the worker.
///
/// The flag is monotonic: a worker does not un-drain. Draining ends by
/// the worker exiting; the next binary starts fresh (`Running`).
#[derive(Debug, Clone, Default)]
pub struct DrainSignal(Arc<AtomicBool>);

impl DrainSignal {
    pub fn new() -> Self {
        Self::default()
    }

    /// Request a drain. Idempotent.
    pub fn request(&self) {
        // `Release`/`Acquire` would suffice, but `SeqCst` keeps the
        // reasoning trivial: there is one flag and no other shared
        // state ordered against it.
        self.0.store(true, Ordering::SeqCst);
    }

    /// Whether a drain has been requested — the predicate the reducer
    /// loop polls at each step boundary.
    pub fn is_draining(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// The observable [`DrainState`], for a drain orchestrator to poll.
    pub fn state(&self) -> DrainState {
        if self.is_draining() {
            DrainState::Draining
        } else {
            DrainState::Running
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_running_and_latches_on_request() {
        let signal = DrainSignal::new();
        assert!(!signal.is_draining());
        assert_eq!(signal.state(), DrainState::Running);

        signal.request();
        assert!(signal.is_draining());
        assert_eq!(signal.state(), DrainState::Draining);

        // Idempotent.
        signal.request();
        assert!(signal.is_draining());
    }

    #[test]
    fn clones_share_one_flag() {
        let a = DrainSignal::new();
        let b = a.clone();
        a.request();
        assert!(b.is_draining(), "a clone must observe the same drain flag");
    }
}
