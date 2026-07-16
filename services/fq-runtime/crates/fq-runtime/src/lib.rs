//! factor-q runtime library.
//!
//! Two role modules organise the runtime's responsibilities:
//!
//! - [`control_plane`] — global view: trigger ingestion, audit
//!   projection, schedules, coordination.
//! - [`worker`] — execution: invocation host loop, in-flight
//!   state, tool dispatch, LLM calls.
//!
//! In v1 both roles are hosted in the same `fq run` process,
//! but the boundary between them is enforced at compile time
//! through the [`worker::Worker`] trait. v2 splits the
//! deployment without changing the contract.
//!
//! See `docs/design/committed/data-architecture.md` for the architectural
//! framing and `docs/plans/closed/2026-04-28-data-architecture-v1.md`
//! for the implementation plan.

pub mod agent;
pub mod bus;
pub mod config;
pub mod events;
pub mod health;
pub mod llm;
pub mod mcp;
pub mod policy;
pub mod pricing;
pub mod prompt;
pub mod tools;
pub mod transcript;
pub mod validation;
pub mod views;

// The read-only operator service (#105 layer 2) — optional so library
// consumers don't pull tarpc; `fq-cli` turns it on.
#[cfg(feature = "read-service")]
pub mod read_service;

// Role modules. Both stay `pub` so that downstream code (fq-cli,
// integration tests) can reach typed APIs that haven't been
// surfaced at the crate root yet (e.g. `control_plane::projection::store::EventFilter`).
// The role boundary is enforced primarily by the `Worker` trait
// — `TriggerDispatcher` consumes `Arc<dyn Worker>`, so the
// control-plane has no compile-time handle on the worker's
// internals. Cross-module direct imports remain possible inside
// the crate; convention plus code review keep them rare.
pub mod control_plane;
pub mod worker;

#[cfg(test)]
pub mod test_support;

pub use agent::{
    Agent, AgentId, AgentRegistry, CapabilityValidation, ElicitationGrant, EvaluatorSpec,
    McpServerDeclaration, RootsGrant, SamplingGrant, Sandbox,
};
pub use bus::EventBus;
pub use config::Config;
pub use control_plane::dispatcher::{
    DispatcherError, SharedRegistry, TriggerDispatcher, shared_registry,
};
pub use control_plane::projection::{ProjectionConsumer, ProjectionStore};
pub use control_plane::{
    AdvisoryWatch, AdvisoryWatchError, CONTROL_PLANE_SCHEMA_VERSION, ControlPlaneStore,
    ControlPlaneStoreError, CoordinationConsumer, CoordinationConsumerError, HeartbeatConsumer,
    HeartbeatConsumerError, OwnerStatus, SummaryConsumer, SummaryConsumerError,
};
pub use llm::{ChatRequest, ChatResponse, LlmClient, LlmError};
pub use mcp::{
    AdvertisedCapabilities, McpClientManager, McpError, McpResourceReader, McpServerConfig,
    RootsHandle, ServerRequest, advertised_roots_from_tool_sandbox, roots_from_tool_sandbox,
};
pub use pricing::{ModelPricing, PricingTable};
pub use tools::ToolRegistry;
pub use views::Views;
pub use worker::{
    ArchiveAckConsumer, ArchiveAckError, ArchiveRetryError, ArchiveRetrySweeper, ExecutorError,
    Harness, InvocationOutcome, Reducer, ReducerContext, ReducerContextBuilder, ReducerRunner,
    RunnerConfig, RunnerConfigBuilder, SamplingChannel, WORKER_SCHEMA_VERSION, Worker, WorkerStore,
    WorkerStoreError,
};
