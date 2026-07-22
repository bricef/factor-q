//! The canonical exemplar catalogue: one declaration per entity
//! kind, with typed inputs and outputs — for testing consumers of
//! this crate (registries, edges, codegen, renderers) against a
//! stable, known surface. Not part of the production catalogue: real
//! declarations live with their handlers, daemon-side.
//!
//! The declarations here are pinned by fq-ops's own registry tests
//! and schema snapshot, so their shapes are contract-stable.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{Atom, Authority, Command, Domain, Report, Stability, Synthetic, Verb, View};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EntryKey {
    pub seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EntryState {
    pub seq: u64,
    pub invocation_id: String,
    pub round: u64,
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EntryFilter {
    pub invocation_id: String,
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Turn: an atom — Get/List/Stream derive. A **Turn** is one action
/// (assistant output or tool result); a **Round** is the bundle of
/// Turns in one agent-loop iteration, recoverable via the `round`
/// grouping key (the ADR-0027 step boundary is a Round boundary).
pub fn turn() -> Atom {
    Atom::new::<EntryKey, EntryState, EntryFilter>(
        Domain::Turn,
        "exemplar resource",
        Stability::Experimental,
    )
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct InvocationKey {
    pub invocation_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct InvocationState {
    pub invocation_id: String,
    pub agent_id: String,
    pub phase: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct InvocationFilter {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Invocation: a view — Get/List derive; no Stream (stream its atoms).
pub fn invocation() -> View {
    View::new::<InvocationKey, InvocationState, InvocationFilter>(
        Domain::Invocation,
        "exemplar resource",
        Stability::Experimental,
    )
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TriggerKey {
    pub seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TriggerState {
    pub seq: u64,
    pub agent_id: String,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TriggerFilter {
    #[serde(default)]
    pub agent_id: Option<String>,
}

/// Trigger: an atom. Not operator-creatable via a generic verb —
/// dispatching work is `trigger.publish`, a command.
pub fn trigger() -> Atom {
    Atom::new::<TriggerKey, TriggerState, TriggerFilter>(
        Domain::Trigger,
        "exemplar resource",
        Stability::Experimental,
    )
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ControlState {
    pub version: String,
    pub nats_connected: bool,
    pub stream_ok: bool,
}

/// Control: the synthetic resource — Get alone derives (the machinery
/// describing itself); its verbs register as commands.
pub fn control() -> Synthetic {
    Synthetic::new::<ControlState>(
        Domain::Control,
        "exemplar resource",
        Stability::Experimental,
    )
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DropInput {
    pub invocation_id: String,
    pub reason: Option<String>,
}

/// invocation.drop: a command — the declaration is one constructor
/// call carrying identity, input type, authority, and contract text.
pub fn invocation_drop() -> Command {
    Command::new::<DropInput>(
        Domain::Invocation,
        "drop",
        Authority {
            verb: Verb::Write,
            scope: Domain::Invocation,
        },
        "Drop an in-flight invocation, archiving it as failed.",
        Stability::Experimental,
    )
    .description("Kill-switch semantics: workers observe the drop at their next step boundary.")
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DownInput {
    #[serde(default)]
    pub now: bool,
}

/// control.down: a machinery verb on the synthetic resource — manual
/// authority, same one-site declaration.
pub fn control_down() -> Command {
    Command::new::<DownInput>(
        Domain::Control,
        "down",
        Authority {
            verb: Verb::Write,
            scope: Domain::Control,
        },
        "Stop the daemon, draining in-flight work to a step boundary.",
        Stability::Experimental,
    )
    .description("Confirmation is the shutdown event, not the ack.")
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PublishInput {
    pub agent_id: String,
    pub payload: serde_json::Value,
}

/// trigger.publish: creation is not a generic verb — dispatching work
/// is a command, and its authority (Write trigger) stays separately
/// grantable from the machinery's lifecycle authority (Write control).
pub fn trigger_publish() -> Command {
    Command::new::<PublishInput>(
        Domain::Trigger,
        "publish",
        Authority {
            verb: Verb::Write,
            scope: Domain::Trigger,
        },
        "Dispatch a trigger to an agent via the durable trigger stream.",
        Stability::Experimental,
    )
    .description(
        "At-least-once delivery with a bounded budget; the receipt references the \
         appended trigger atom.",
    )
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CostParams {
    #[serde(default)]
    pub since: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CostOutput {
    pub total_cost: f64,
    pub total_llm_calls: u64,
}

/// cost.summary: a report — a named computation, Read on its inputs.
/// cost.summary scopes to Domain::Cost — a domain carrying only
/// reports, so the aggregate is grantable without granting the raw
/// event log it computes from.
pub fn cost_summary() -> Report {
    Report::new::<CostParams, CostOutput>(
        Domain::Cost,
        "summary",
        "Aggregate cost across all agents.",
        Stability::Experimental,
    )
    .description("Cost figures are retained indefinitely; totals never window.")
}
