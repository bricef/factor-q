//! Worker role.
//!
//! Per `docs/design/data-architecture.md` §3, a worker is where
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
pub mod heartbeat;
pub mod id;
pub mod introspection;
pub mod recovery;
pub mod reducer;
pub mod store;

pub use archive_ack::{ArchiveAckConsumer, ArchiveAckError};
pub use archive_retry::{ArchiveRetryError, ArchiveRetrySweeper};
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
use uuid::Uuid;

use crate::agent::Agent;
use crate::bus::BusError;
use crate::events::TriggerSource;
use crate::llm::{ChatResponse, LlmClient, LlmError};

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

    #[error("max iterations exceeded")]
    MaxIterationsExceeded,
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
    async fn run_invocation(
        &self,
        agent: &Agent,
        llm: &dyn LlmClient,
        trigger_source: TriggerSource,
        trigger_subject: Option<String>,
        trigger_payload: Value,
    ) -> Result<InvocationOutcome, ExecutorError>;
}

#[async_trait]
impl<R: crate::worker::reducer::Reducer + Send + Sync + 'static> Worker for ReducerRunner<R> {
    /// Defers to [`ReducerRunner::run`] with the reducer
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
    ) -> Result<InvocationOutcome, ExecutorError> {
        self.run(agent, llm, trigger_source, trigger_subject, trigger_payload)
            .await
    }
}
