//! factor-q runtime library.
//!
//! Two role modules organise the runtime's responsibilities:
//!
//! - [`control_plane`] — global view: trigger ingestion, audit
//!   projection, schedules, coordination.
//! - [`worker`] — execution: invocation host loop, in-flight
//!   state, tool dispatch, LLM calls.
//!
//! In v1 both roles are hosted in the same `fqd` process,
//! but the boundary between them is enforced at compile time
//! through the [`worker::Worker`] trait. v2 splits the
//! deployment without changing the contract.
//!
//! See `docs/design/committed/data-architecture.md` for the architectural
//! framing and `docs/plans/closed/2026-04-28-data-architecture-v1.md`
//! for the implementation plan.

// The agent definition domain — the model, the frontmatter parser, the
// directory registry — lives in its own crate, because reading a
// definition is something both ends do: the daemon loads a directory of
// them at startup, and `fq agent validate` lints one file before it is
// deployed. The client can now do that without linking a store or a
// broker. Re-exported here so every `crate::agent::…` call site (and
// `fq_runtime::agent::…` for the daemon and the tests) is unchanged.
pub use fq_agent as agent;
// The registry's projection into the declared view shapes went with the
// registry: a `From<&LoadedAgent>` for a `fq-ops` shape is two foreign
// types anywhere else, and coherence and the domain agree about where
// it belongs. Same path from here as before.
pub use fq_agent::view as agent_view;
pub mod bus;
pub mod config;
pub mod db;
pub mod dead_letter;
pub mod event_tail;
pub mod events;
pub mod health;
pub mod llm;
pub mod mcp;
pub mod paths;
pub mod policy;
pub mod pricing;
pub mod prompt;
// The declared contract shapes now live in the wire crate; re-exported
// so a caller reaches them by the same path as before. The transient
// set is a macro, so it re-exports at the crate root the way
// `#[macro_export]` put it there.
pub use fq_ops::surface;
pub use fq_ops::transient_event_types;
pub mod tools;
pub mod transcript;
pub mod trigger;
pub mod turn;
pub mod validation;
pub mod views;
pub mod watermark;

// Role modules. Both stay `pub` so that downstream code (fq-daemon —
// the only crate that links this one — and integration tests) can reach
// typed APIs that haven't been surfaced at the crate root yet
// (e.g. `control_plane::projection::store::EventFilter`).
// The role boundary is enforced primarily by the `Worker` trait
// — `TriggerDispatcher` consumes `Arc<dyn Worker>`, so the
// control-plane has no compile-time handle on the worker's
// internals. Cross-module direct imports remain possible inside
// the crate; convention plus code review keep them rare.
pub mod control_plane;
pub mod worker;

// Compiled for this crate's own tests, and — reduced to the
// self-contained mock LLM server — for fq-daemon's integration tests
// via the `test-support` feature, which is how its dev-dependency on
// this crate is declared. The dev-dep-heavy helpers stay test-only;
// see the cfg gates inside the module.
#[cfg(any(test, feature = "test-support"))]
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
pub use db::{RuntimeDbPaths, SplitOutcome, split_legacy_events_db};
pub use llm::{ChatRequest, ChatResponse, LlmClient, LlmError};
pub use mcp::{
    AdvertisedCapabilities, McpClientManager, McpError, McpResourceReader, McpServerConfig,
    RootsHandle, ServerRequest, advertised_roots_from_tool_sandbox, roots_from_tool_sandbox,
};
pub use pricing::{ModelPricing, PricingTable};
pub use tools::ToolRegistry;
pub use trigger::{PublishedTrigger, TRIGGER_ID_HEADER, Trigger};
pub use views::Views;
pub use worker::{
    ArchiveAckConsumer, ArchiveAckError, ArchiveRetryError, ArchiveRetrySweeper, ExecutorError,
    Harness, InvocationOutcome, Reducer, ReducerContext, ReducerContextBuilder, ReducerRunner,
    RunnerConfig, RunnerConfigBuilder, SamplingChannel, WORKER_SCHEMA_VERSION, Worker, WorkerStore,
    WorkerStoreError,
};
