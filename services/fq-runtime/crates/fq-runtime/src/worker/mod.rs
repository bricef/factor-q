//! Worker role.
//!
//! Per `docs/design/committed/data-architecture.md` §3, a worker is where
//! work happens: it claims invocations the control-plane routes
//! to it, runs the host loop for those invocations, owns local
//! in-flight state, executes tool calls, and publishes lifecycle
//! events to NATS.
//!
//! In v1 the control-plane and worker share a single `fq run`
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
pub mod drain;
pub mod heartbeat;
pub mod id;
pub mod introspection;
pub mod recovery;
pub mod reducer;
pub mod store;

pub use archive_ack::{ArchiveAckConsumer, ArchiveAckError};
pub use archive_retry::{ArchiveRetryError, ArchiveRetrySweeper};
pub use drain::{DrainReason, DrainRequest, DrainSignal, DrainState};
pub use heartbeat::{DEFAULT_INTERVAL_MS as HEARTBEAT_DEFAULT_INTERVAL_MS, HeartbeatProducer};
pub use id::WorkerId;
pub use recovery::{
    CategoryCounts, ClassifiedInvocation, RecoveryCategory, categorise, scan_in_flight,
};
pub use reducer::{
    Harness, Reducer, ReducerContext, ReducerContextBuilder, ReducerRunner, RunnerConfig,
    RunnerConfigBuilder, SamplingChannel,
};
pub use store::{
    Compatibility, DispatchStatus, InvocationStateRow, LlmDispatchRow, ToolDispatchRow,
    WORKER_SCHEMA_VERSION, WorkerStore, WorkerStoreError,
};

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::oneshot;
use uuid::Uuid;

use crate::agent::Agent;
use crate::bus::BusError;
use crate::events::TriggerSource;
use crate::llm::{ChatResponse, LlmClient, LlmError};

/// A one-shot "durably started" signal the control-plane hands a
/// worker through [`Worker::run_invocation`].
///
/// The worker [`fire`](DurableStart::fire)s it exactly once, right
/// after the invocation's first durable (WAL) write — the point past
/// which a crash is recoverable from the WAL rather than lost. The
/// trigger dispatcher waits on the paired receiver and only *then*
/// acks the JetStream trigger, closing the ack→first-WAL-write window
/// (a crash before the first WAL write leaves the trigger unacked, so
/// JetStream redelivers it). See `control_plane::dispatcher`.
///
/// [`noop`](DurableStart::noop) is the "nobody is waiting" variant —
/// used by the direct `ReducerRunner::run` paths (CLI, tests, sim)
/// that ack nothing. Firing a noop signal is a no-op.
#[derive(Debug, Default)]
pub struct DurableStart {
    tx: Option<oneshot::Sender<()>>,
}

impl DurableStart {
    /// A signal whose fire notifies `rx`. Returns the signal to hand
    /// to the worker and the receiver the caller awaits.
    pub fn channel() -> (Self, oneshot::Receiver<()>) {
        let (tx, rx) = oneshot::channel();
        (Self { tx: Some(tx) }, rx)
    }

    /// A signal nobody is listening on — firing it does nothing. For
    /// the direct-run paths that do not ack a trigger.
    pub fn noop() -> Self {
        Self { tx: None }
    }

    /// Signal that the invocation has durably started. Idempotent: the
    /// first call notifies the waiter; later calls (and a `noop`) are
    /// no-ops. A dropped receiver (the dispatcher already acked and
    /// moved on) is ignored.
    pub fn fire(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(());
        }
    }
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
    Suspended {
        invocation_id: Uuid,
    },
}

/// Infrastructure errors returned by a [`Worker`].
#[derive(Debug, thiserror::Error)]
pub enum ExecutorError {
    #[error("event bus error: {0}")]
    Bus(#[from] BusError),

    #[error("LLM error: {0}")]
    Llm(#[from] LlmError),

    #[error("worker store error: {0}")]
    WorkerStore(String),

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
            // Bus / store unavailability is a momentary infra condition —
            // retryable once JetStream redelivers.
            ExecutorError::Bus(_) | ExecutorError::WorkerStore(_) => true,
            // Defer to the LLM client's own transient/permanent split.
            ExecutorError::Llm(err) => err.is_transient(),
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
    /// window). Direct callers that ack nothing pass
    /// [`DurableStart::noop`].
    async fn run_invocation(
        &self,
        agent: &Agent,
        llm: &dyn LlmClient,
        trigger_source: TriggerSource,
        trigger_subject: Option<String>,
        trigger_payload: Value,
        durable_start: DurableStart,
    ) -> Result<InvocationOutcome, ExecutorError>;

    /// Request that the worker drain: stop starting new steps and let
    /// each in-flight invocation suspend at its next step boundary,
    /// checkpointed to the WAL, to be resumed by the next binary's
    /// recovery (ADR-0027). This is the only handle the control-plane
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
        trigger_source: TriggerSource,
        trigger_subject: Option<String>,
        trigger_payload: Value,
        durable_start: DurableStart,
    ) -> Result<InvocationOutcome, ExecutorError> {
        self.run_signalling(
            agent,
            llm,
            trigger_source,
            trigger_subject,
            trigger_payload,
            durable_start,
        )
        .await
    }

    async fn request_drain(&self, _req: DrainRequest) {
        // v1: flip the worker-local flag the reducer loop polls. The
        // reason will feed the drain audit event once `fq drain` lands
        // (PR-3); the suspend primitive itself only needs the flag.
        self.drain_signal().request();
    }

    fn drain_status(&self) -> DrainState {
        self.drain_signal().state()
    }
}
