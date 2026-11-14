//! Worker role.
//!
//! Per `docs/design/committed/data-architecture.md` §3, a worker is where
//! work happens: it claims invocations the control-plane routes
//! to it, runs the host loop for those invocations, owns local
//! in-flight state, executes tool calls, and publishes lifecycle
//! events to NATS.
//!
//! In v1 the control-plane and worker share a single `fqd`
//! process; this module enforces the role boundary at compile
//! time so v2 (separate deployment) is a process split rather
//! than a redesign.
//!
//! The boundary itself is the [`Worker`] trait. Anything that
//! the control-plane asks of a worker goes through the trait;
//! the control-plane has no other handle on worker internals.
//!
//! The shipped implementation is [`ReducerRunner`], the
//! reducer-harness path that drives a pure synchronous
//! [`Reducer`] from the host side. The control-plane (and
//! `fq trigger` from the CLI) hand invocations to a
//! `dyn Worker`; in v2 a remote-worker adapter implements the
//! same trait against NATS without the control-plane noticing.

pub mod archive_ack;
pub mod archive_retry;
pub mod deferral;
pub mod drain;
pub mod heartbeat;
pub mod introspection;
pub mod recovery;
pub mod reducer;
pub mod store;
pub mod workspace;

pub use archive_ack::{ArchiveAckConsumer, ArchiveAckError};
pub use archive_retry::{ArchiveHolds, ArchiveRetryError, ArchiveRetrySweeper};
pub use deferral::{DeferralQueue, DueResume};
pub use drain::{DrainReason, DrainRequest, DrainSignal, DrainState};
pub use heartbeat::{DEFAULT_INTERVAL_MS as HEARTBEAT_DEFAULT_INTERVAL_MS, HeartbeatProducer};
// The identifier rides the wire — four event payloads name a worker,
// and two subjects are built from one — so it lives in the contract
// crate and is re-exported here, where the role is.
pub use fq_ops::worker::WorkerId;
pub use recovery::{
    CategoryCounts, ClassifiedInvocation, DurableStartReport, RecoveryCategory, WalRun, categorise,
    scan_in_flight,
};
pub use reducer::{
    Harness, MAX_SUPPORTED_ITERATIONS, Reducer, ReducerContext, ReducerContextBuilder,
    ReducerRunner, RunnerConfig, RunnerConfigBuilder, SamplingChannel,
};
pub use store::{
    Compatibility, DispatchStatus, InvocationStateRow, LlmDispatchRow, ToolDispatchRow,
    WORKER_SCHEMA_VERSION, WorkerStore, WorkerStoreError,
};

use async_trait::async_trait;
use tokio::sync::oneshot;
use uuid::Uuid;

use crate::agent::Agent;
use crate::bus::BusError;
use crate::llm::{ChatResponse, LlmClient, LlmError};
use crate::trigger::Trigger;

/// A one-shot "durably started" signal the control-plane hands a
/// worker through [`Worker::run_invocation`], carrying the id the
/// invocation is to run under.
///
/// The worker [`fire`](DurableStart::fire)s it exactly once, right
/// after the invocation's first durable (WAL) write — the point past
/// which a crash is recoverable from the WAL rather than lost. The
/// trigger dispatcher waits on the paired receiver and only *then*
/// acks the JetStream trigger, closing the ack→first-WAL-write window
/// (a crash before the first WAL write leaves the trigger unacked, so
/// JetStream redelivers it). See `control_plane::dispatcher`.
///
/// The id is the dispatcher's: chosen before it claims the trigger and
/// written on the claim row with the claim, so the row names the
/// invocation before the invocation exists. A worker runs the invocation
/// under it, because the WAL row carrying that id is how startup recovery
/// finds the claim of an invocation whose durable start a crash kept the
/// dispatcher from recording (#959).
///
/// [`noop`](DurableStart::noop) is the "nobody is waiting" variant —
/// used by the direct `ReducerRunner::run` paths (CLI, tests, sim)
/// that claim and ack nothing, so it carries no id and the worker mints
/// one. Firing a noop signal notifies nobody.
///
/// Whatever the variant, the signal also remembers whether it has
/// fired, which is the worker's own answer to "does a WAL row exist for
/// this invocation yet": a stop at a step boundary is
/// [`InvocationOutcome::Suspended`] only once it has
/// (`DurableStart::stopped_at_boundary`, #974).
#[derive(Debug, Default)]
pub struct DurableStart {
    invocation_id: Option<Uuid>,
    tx: Option<oneshot::Sender<()>>,
    started: bool,
}

impl DurableStart {
    /// A signal for an invocation that is to run under `invocation_id`,
    /// whose fire notifies `rx`. Returns the signal to hand to the
    /// worker and the receiver the caller awaits.
    pub fn channel(invocation_id: Uuid) -> (Self, oneshot::Receiver<()>) {
        let (tx, rx) = oneshot::channel();
        let signal = Self {
            invocation_id: Some(invocation_id),
            tx: Some(tx),
            started: false,
        };
        (signal, rx)
    }

    /// A signal nobody is listening on — firing it notifies nobody. For
    /// the direct-run paths that do not ack a trigger.
    pub fn noop() -> Self {
        Self::default()
    }

    /// The signal for an invocation resumed from its WAL: it durably
    /// started in an earlier drive, so there is nothing to signal, and a
    /// stop at any of its step boundaries leaves a row to resume.
    pub(crate) fn resumed() -> Self {
        Self {
            started: true,
            ..Self::default()
        }
    }

    /// The id the invocation must run under, when the caller chose one.
    pub fn invocation_id(&self) -> Option<Uuid> {
        self.invocation_id
    }

    /// Signal that the invocation has durably started. Idempotent: the
    /// first call notifies the waiter; later calls (and a `noop`) notify
    /// nobody. A dropped receiver (the dispatcher already acked and
    /// moved on) is ignored.
    pub fn fire(&mut self) {
        self.started = true;
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(());
        }
    }

    /// What an invocation stopped at a step boundary by `stopped_by`
    /// returns (#974). Once the signal has fired, the invocation's first
    /// WAL write has landed and the previous step's checkpoint is on
    /// disk, so the stop is [`InvocationOutcome::Suspended`] and recovery
    /// resumes it. Before that the worker has written no row, so the stop
    /// is [`InvocationOutcome::Unstarted`]: there is nothing for recovery
    /// to find, and the work is whoever handed it over's to redo or drop.
    pub(crate) fn stopped_at_boundary(
        &self,
        invocation_id: Uuid,
        stopped_by: BoundaryStop,
    ) -> InvocationOutcome {
        if self.started {
            InvocationOutcome::Suspended { invocation_id }
        } else {
            InvocationOutcome::Unstarted {
                invocation_id,
                stopped_by,
            }
        }
    }
}

/// What stopped an invocation at a step boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundaryStop {
    /// The worker is draining (ADR-0027): this process is on its way out,
    /// and the work is the next binary's.
    Drain,
    /// An operator halted the invocation (`invocation.drop --live`),
    /// whose drop is its outcome. The process goes on running.
    Halt,
}

/// Outcome of a successful call to [`Worker::run_invocation`].
#[derive(Debug)]
pub enum InvocationOutcome {
    Completed {
        invocation_id: Uuid,
        response: ChatResponse,
        cost: f64,
        duration_ms: u64,
    },
    BudgetExceeded {
        invocation_id: Uuid,
        cost: f64,
    },
    /// The invocation suspended at a step boundary in response to a
    /// drain (ADR-0027). **Not a terminal outcome:** its state is
    /// already checkpointed to the WAL and the row stays in-flight, so
    /// the next binary's recovery resumes it. No `Completed`/`Failed`
    /// event is emitted — the durable state is bit-identical to a crash
    /// at that boundary, which recovery already handles.
    ///
    /// Only ever returned once the invocation's first WAL write has
    /// landed, so there is always a row to resume. A stop before that is
    /// [`Self::Unstarted`].
    Suspended {
        invocation_id: Uuid,
    },
    /// The invocation stopped at its first step boundary, **before its
    /// first WAL write** (#974): a drain or an operator halt arrived while
    /// it was being set up. Either way the worker wrote no
    /// `invocation_state` row, so recovery has nothing to resume, and
    /// signalled no durable start; its `triggered` event is on the stream.
    ///
    /// The rest depends on `stopped_by`. After a **drain** nothing durable
    /// exists for the invocation and no outcome event follows its
    /// `triggered`: the trigger dispatcher leaves its trigger un-acked for
    /// the next binary, which runs it under a new invocation id. A
    /// **halt** is the first half of an operator drop, which then publishes
    /// `invocation.operator_recovered` for the invocation and has the
    /// control plane write its archive and ownership rows as `failed`:
    /// that is its outcome, and the dispatcher consumes its trigger.
    Unstarted {
        invocation_id: Uuid,
        stopped_by: BoundaryStop,
    },
    /// The invocation was put down because its model is rate-limited
    /// past what the retry layer waits in place (#278). **Not a terminal
    /// outcome, and not a failure:** like [`Self::Suspended`], the WAL
    /// row stays in flight — with `phase = "deferred"` — and the
    /// `invocation.deferred` event says why. Whoever drives invocations
    /// resumes it after `resume_after` ([`DeferralQueue`]); a daemon that
    /// stops first resumes it at startup like any in-flight row. No
    /// `failed` event is emitted and no trigger retry is consumed.
    Deferred {
        invocation_id: Uuid,
        /// How long to wait before resuming: at least what the provider
        /// asked for, and at least the model's escalating default pause.
        resume_after: std::time::Duration,
    },
}

/// Permanent verdicts reached while resuming an invocation.
#[derive(Debug)]
pub enum ResumeError {
    NoStateRow { invocation_id: Uuid },
    AlreadyTerminal { invocation_id: Uuid },
    AmbiguousWal { invocation_id: Uuid },
    ReplayFailed { step_index: u32, source: String },
}

impl std::fmt::Display for ResumeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoStateRow { invocation_id } => {
                write!(f, "no state row for {invocation_id}; nothing to resume")
            }
            Self::AlreadyTerminal { invocation_id } => {
                write!(
                    f,
                    "invocation {invocation_id} is already terminal; nothing to resume"
                )
            }
            Self::AmbiguousWal { invocation_id } => write!(
                f,
                "invocation {invocation_id} has ambiguous WAL state; triage with \
                 `fq invocation resume <id>` to reconcile and continue, or \
                 `fq invocation drop <id> --reason ...` to abandon it"
            ),
            Self::ReplayFailed { step_index, source } => {
                write!(f, "replay step {step_index} failed: {source}")
            }
        }
    }
}

impl std::error::Error for ResumeError {}

/// Infrastructure errors returned by a [`Worker`].
#[derive(Debug, thiserror::Error)]
pub enum ExecutorError {
    #[error("event bus error: {0}")]
    Bus(#[from] BusError),

    #[error("LLM error: {0}")]
    Llm(#[from] LlmError),

    #[error("worker store error: {0}")]
    WorkerStore(String),

    #[error("{0}")]
    Resume(ResumeError),

    /// Workspace provisioning/re-association failed (parallel-workers
    /// Phase 0). On the fresh path this happens before the first WAL
    /// write, so the trigger dispatcher's transient/permanent split
    /// decides redelivery.
    #[error("workspace error: {0}")]
    Workspace(#[from] workspace::WorkspaceError),

    /// The invocation reached a terminal `failed` event. Carries the
    /// same [`FailureKind`](crate::events::FailureKind) as that event
    /// — the error returned to the caller and the event trail never
    /// disagree about why an invocation failed.
    #[error("invocation failed ({kind:?}): {message}")]
    InvocationFailed {
        kind: crate::events::FailureKind,
        message: String,
    },
}

impl ExecutorError {
    /// Whether retrying the invocation *from scratch* could plausibly
    /// succeed — i.e. the failure is an infrastructure hiccup, not a
    /// permanent (poison) condition.
    ///
    /// The trigger dispatcher uses this for a failure *before the first
    /// WAL write*, where the run left nothing durable to recover: a
    /// transient error NAKs the trigger so JetStream redelivers and
    /// retries it, a permanent one ACKs it (redelivering a poison trigger
    /// would loop under the consumer's unbounded redelivery — see #49).
    /// Mirrors the heartbeat/projection/coordination consumers, which NAK
    /// store/bus errors to redeliver.
    pub fn is_transient(&self) -> bool {
        match self {
            // An event larger than the broker's `max_payload` is refused
            // the same way on every attempt: the run that built it builds
            // it again (#828). The runner has already ended the invocation
            // with a `failed` for it, where it could.
            ExecutorError::Bus(BusError::PayloadTooLarge { .. }) => false,
            // Bus / store unavailability is a momentary infra condition —
            // retryable once JetStream redelivers.
            ExecutorError::Bus(_) | ExecutorError::WorkerStore(_) => true,
            // Resume protocol verdicts cannot heal on redelivery, and #49's
            // consumer has no max_deliver bound yet.
            ExecutorError::Resume(_) => false,
            // Defer to the LLM client's own transient/permanent split.
            ExecutorError::Llm(err) => err.is_transient(),
            // Workspace provisioning is pure filesystem work, so no
            // failure heals on redelivery — and #49's consumer has no
            // max_deliver bound yet. The provider decides.
            ExecutorError::Workspace(err) => err.is_transient(),
            // A terminal `failed` event is a permanent agent-level outcome
            // (budget, max-iterations, validation, …); retrying the same
            // trigger just reproduces it.
            ExecutorError::InvocationFailed { .. } => false,
        }
    }
}

/// What the control-plane asks of a worker.
///
/// In v1 this is implemented by [`ReducerRunner`]; v2 will add a
/// remote-worker adapter implementing the same trait against
/// NATS without the control-plane noticing.
///
/// The trait is deliberately narrow: one method, fully
/// async. Everything else the worker needs (LLM client, event
/// bus, tool registry, pricing) is captured by the
/// implementation at construction time.
#[async_trait]
pub trait Worker: Send + Sync {
    /// Execute an invocation to terminal.
    ///
    /// Errors returned from this method are *infrastructure*
    /// errors (event bus down, LLM client unreachable, etc.).
    /// Agent-level outcomes (a budget being exceeded, an LLM
    /// error during a turn, max iterations reached) come back
    /// as `InvocationOutcome::*` variants and as `Failed` events
    /// on the bus, not as errors here.
    ///
    /// `durable_start` is [`fire`](DurableStart::fire)d exactly once,
    /// right after the invocation's first durable (WAL) write — the
    /// control-plane waits on it to ack the trigger only after the run
    /// is recoverable from the WAL (closing the ack→first-WAL-write
    /// window). When it carries an
    /// [`invocation_id`](DurableStart::invocation_id), the invocation
    /// runs under that id and no other: the trigger's claim already
    /// names it. Direct callers that ack nothing pass
    /// [`DurableStart::noop`]. An invocation stopped before that write
    /// returns [`InvocationOutcome::Unstarted`] with the signal unfired.
    ///
    /// The trigger arrives as one [`Trigger`] value rather than as
    /// loose provenance params (issue #87): it carries its own
    /// identity, and a seam that took the pieces separately would let a
    /// caller hand a worker work with no name on it.
    async fn run_invocation(
        &self,
        agent: &Agent,
        llm: &dyn LlmClient,
        trigger: Trigger,
        delivery_attempt: Option<u32>,
        durable_start: DurableStart,
    ) -> Result<InvocationOutcome, ExecutorError>;

    /// Continue an in-flight invocation from its WAL — the one this
    /// worker put down as [`InvocationOutcome::Deferred`] and the
    /// control plane is now handing back after the delay (#278). The
    /// same recovery path a drain or a crash is followed by; a resume
    /// that meets another rate limit comes back `Deferred` again.
    ///
    /// The default refuses: a worker that cannot resume says so rather
    /// than pretending, and the test doubles that never defer keep
    /// their two-method shape.
    async fn resume_invocation(
        &self,
        agent: &Agent,
        llm: &dyn LlmClient,
        invocation_id: Uuid,
    ) -> Result<InvocationOutcome, ExecutorError> {
        let _ = (agent, llm);
        Err(ExecutorError::WorkerStore(format!(
            "this worker cannot resume invocation {invocation_id}"
        )))
    }

    /// Where this worker's WAL holds `invocation_id`'s run, if it holds a
    /// row for it: in flight, or finished and not yet archived. It is the
    /// worker's half of a start on record, the definition startup recovery,
    /// `fq doctor` and the trigger dispatcher share
    /// ([`crate::control_plane::store::RunRecords`]).
    ///
    /// The dispatcher asks it about a claim its own process holds that no
    /// copy of the trigger is handling any more (issue 967), to tell a run
    /// that started from a delivery that never did. The default answers
    /// with an error, which the dispatcher reads as "cannot tell" and so
    /// leaves the claim as it is: a worker that has not been taught to
    /// answer can never make the dispatcher start a run twice.
    async fn wal_run(&self, invocation_id: &str) -> Result<Option<WalRun>, ExecutorError> {
        Err(ExecutorError::WorkerStore(format!(
            "this worker cannot say whether its WAL holds invocation {invocation_id}"
        )))
    }

    /// Request that the worker drain: stop starting new steps and let
    /// each in-flight invocation suspend at its next step boundary,
    /// checkpointed to the WAL, to be resumed by the next binary's
    /// recovery (ADR-0027). One still being set up, with no WAL write
    /// yet, stops as [`InvocationOutcome::Unstarted`] instead, for its
    /// trigger to be redelivered. This is the only handle the control-plane
    /// has on in-flight work — in v1 an in-process flag flip, in v2 an
    /// RPC to the worker node. Idempotent; a worker does not un-drain.
    async fn request_drain(&self, req: DrainRequest);

    /// The worker's current [`DrainState`]. Lets the trigger dispatcher
    /// (and, later, a drain orchestrator) observe through this one seam
    /// whether a drain is in progress — e.g. to stop pulling new
    /// triggers — without a second handle on worker internals.
    fn drain_status(&self) -> DrainState;
}

#[async_trait]
impl<R: crate::worker::reducer::Reducer + Send + Sync + 'static> Worker for ReducerRunner<R> {
    /// Defers to [`ReducerRunner::run_signalling`] with the reducer
    /// the runner was constructed with. The trait doesn't
    /// have to expose the `R: Reducer` generic — production
    /// wires `ReducerRunner<Harness>`; tests pick whichever
    /// reducer they want at construction time.
    async fn run_invocation(
        &self,
        agent: &Agent,
        llm: &dyn LlmClient,
        trigger: Trigger,
        delivery_attempt: Option<u32>,
        durable_start: DurableStart,
    ) -> Result<InvocationOutcome, ExecutorError> {
        self.run_signalling(agent, llm, trigger, delivery_attempt, durable_start)
            .await
    }

    async fn resume_invocation(
        &self,
        agent: &Agent,
        llm: &dyn LlmClient,
        invocation_id: Uuid,
    ) -> Result<InvocationOutcome, ExecutorError> {
        self.resume(agent, llm, invocation_id).await
    }

    async fn wal_run(&self, invocation_id: &str) -> Result<Option<WalRun>, ExecutorError> {
        self.wal_run_of(invocation_id).await
    }

    async fn request_drain(&self, _req: DrainRequest) {
        // v1: flip the worker-local flag the reducer loop polls. The
        // reason will feed the drain audit event once `fq down` lands
        // (PR-3); the suspend primitive itself only needs the flag.
        self.drain_signal().request();
    }

    fn drain_status(&self) -> DrainState {
        self.drain_signal().state()
    }
}

#[cfg(test)]
mod durable_start_tests {
    use super::*;

    /// A boundary stop is `Suspended` exactly when there is a WAL row to
    /// resume, whichever variant of the signal the drive was handed
    /// (#974).
    #[test]
    fn a_boundary_stop_is_suspended_only_once_the_invocation_has_durably_started() {
        let id = Uuid::now_v7();
        let unstarted = |signal: &DurableStart, by| {
            matches!(
                signal.stopped_at_boundary(id, by),
                InvocationOutcome::Unstarted { invocation_id, stopped_by }
                    if invocation_id == id && stopped_by == by
            )
        };
        let suspended = |signal: &DurableStart| {
            [BoundaryStop::Drain, BoundaryStop::Halt]
                .into_iter()
                .all(|by| {
                    matches!(
                        signal.stopped_at_boundary(id, by),
                        InvocationOutcome::Suspended { invocation_id } if invocation_id == id
                    )
                })
        };

        // A dispatched invocation, and a direct run: unstarted until the
        // first WAL write fires the signal, suspended after.
        let (mut dispatched, _rx) = DurableStart::channel(id);
        let mut direct = DurableStart::noop();
        for signal in [&mut dispatched, &mut direct] {
            assert!(unstarted(signal, BoundaryStop::Drain));
            assert!(unstarted(signal, BoundaryStop::Halt));
            signal.fire();
            assert!(suspended(signal));
            signal.fire();
            assert!(suspended(signal), "firing again changes nothing");
        }

        // A resumed invocation has a row from the start.
        assert!(suspended(&DurableStart::resumed()));
    }

    /// Firing still notifies the dispatcher's waiter, once.
    #[test]
    fn firing_notifies_the_waiter() {
        let (mut signal, mut rx) = DurableStart::channel(Uuid::now_v7());
        assert!(rx.try_recv().is_err(), "nothing yet");
        signal.fire();
        assert!(rx.try_recv().is_ok(), "the first fire notifies");
    }
}

#[cfg(test)]
mod executor_error_tests {
    use super::*;

    #[test]
    fn resume_verdicts_are_permanent_but_store_failures_are_transient() {
        let invocation_id = Uuid::nil();
        let verdicts = [
            ResumeError::NoStateRow { invocation_id },
            ResumeError::AlreadyTerminal { invocation_id },
            ResumeError::AmbiguousWal { invocation_id },
            ResumeError::ReplayFailed {
                step_index: 3,
                source: "corrupt replay".to_string(),
            },
        ];

        for verdict in verdicts {
            assert!(!ExecutorError::Resume(verdict).is_transient());
        }
        assert!(ExecutorError::WorkerStore("database unavailable".to_string()).is_transient());
    }

    /// An event the broker refuses for its size is refused again on every
    /// attempt, so it is permanent (#828); every other bus error is still
    /// a momentary condition a redelivery can heal.
    #[test]
    fn an_oversized_event_is_permanent_but_other_bus_errors_are_transient() {
        let oversized = ExecutorError::Bus(BusError::PayloadTooLarge {
            event_type: crate::events::EventKind::ToolResult,
            size: 2_000_000,
            limit: 1_048_576,
        });
        assert!(!oversized.is_transient());

        let momentary = [
            BusError::Publish("broker unreachable".to_string()),
            BusError::PublishTimedOut,
            BusError::Stream("stream unavailable".to_string()),
        ];
        for err in momentary {
            assert!(ExecutorError::Bus(err).is_transient());
        }
    }
}
