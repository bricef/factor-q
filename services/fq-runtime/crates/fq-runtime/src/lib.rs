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
//! See `docs/design/data-architecture.md` for the architectural
//! framing and `docs/plans/closed/2026-04-28-data-architecture-v1.md`
//! for the implementation plan.

pub mod agent;
pub mod bus;
pub mod config;
pub mod events;
pub mod llm;
pub mod mcp;
pub mod pricing;
pub mod tools;

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

pub use agent::{Agent, AgentId, AgentRegistry, McpServerDeclaration, Sandbox};
pub use bus::EventBus;
pub use config::Config;
pub use control_plane::dispatcher::{DispatcherError, TriggerDispatcher};
pub use control_plane::projection::{ProjectionConsumer, ProjectionStore};
pub use control_plane::{
    CONTROL_PLANE_SCHEMA_VERSION, ControlPlaneStore, ControlPlaneStoreError, CoordinationConsumer,
    CoordinationConsumerError, HeartbeatConsumer, HeartbeatConsumerError, OwnerStatus,
};
pub use llm::{ChatRequest, ChatResponse, LlmClient, LlmError};
pub use mcp::{McpClientManager, McpError, McpServerConfig};
pub use pricing::{ModelPricing, PricingTable};
pub use tools::ToolRegistry;
pub use worker::{
    AgentExecutor, ArchiveAckConsumer, ArchiveAckError, ArchiveRetryError, ArchiveRetrySweeper,
    ExecutorError, Harness, InvocationOutcome, Reducer, ReducerRunner, WORKER_SCHEMA_VERSION,
    Worker, WorkerStore, WorkerStoreError,
};
