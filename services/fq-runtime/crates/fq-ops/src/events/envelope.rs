//! The envelope layer: system-written metadata that rides every event.
//!
//! Closed schema, stamped by the runtime — producing agents do not
//! touch it. Cost lives here rather than in a payload variant because
//! it is system-level accounting, not part of the typed contract
//! between graph nodes (ADR-0016 §7).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::agent::AgentId;
use crate::events::llm::LlmCallOrigin;

/// System-generated metadata. Closed schema — if a new field is
/// needed, the runtime grows. Producing agents do not touch the
/// envelope; the runtime stamps it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub schema_version: u32,
    pub event_id: Uuid,
    /// The previous event in this invocation, if any. `None` on the
    /// initial `triggered` event, on system events, and on the first
    /// event emitted by a recovery re-emit (where it explicitly
    /// starts a new chain — see step 2 of the envelope-refactor
    /// plan). Threaded through subsequent publishes by the reducer
    /// runner in step 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_event_id: Option<Uuid>,
    /// Trace correlation id. Equal to `invocation_id` for now;
    /// reserved as a separate field so multi-invocation traces
    /// (e.g. a graph workflow spanning multiple invocations) can be
    /// stitched together later without a wire-format change.
    pub trace_id: Uuid,
    pub agent_id: AgentId,
    pub invocation_id: Uuid,
    /// Stable identifier for the payload schema, e.g.
    /// `"factor-q/triggered@1"`. See [`schema_id_for`](super::schema_id_for).
    pub schema_id: String,
    pub timestamp: DateTime<Utc>,
    /// Cost incurred at this event, if any. Populated on
    /// `llm.response` events; absent on events that do not bill.
    /// Lives on the envelope because cost is system-level
    /// accounting, not part of the typed contract between graph
    /// nodes (ADR-0016 §7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<CostMetadata>,
}

/// Cost metadata attached to events that incur cost. Currently
/// rides on `llm.response` envelopes; a future tool-cost story
/// could attach it to `tool.result` envelopes too.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CostMetadata {
    pub call_id: Uuid,
    pub model: String,
    pub input_tokens: u32,
    pub output_tokens: u32,
    #[serde(default)]
    pub cache_read_tokens: u32,
    #[serde(default)]
    pub cache_write_tokens: u32,
    /// The share of `output_tokens` spent thinking rather than speaking.
    ///
    /// **Carried here because the cost record is where anyone looks to
    /// ask what a call cost, and for a reasoning-first model this split
    /// is most of the answer** — it was invisible in the cost data
    /// entirely (#437). It changes no figure: reasoning is already
    /// inside `output_tokens`, so `output_cost` and `total_cost` are what
    /// they always were. `0` where the provider does not report it.
    #[serde(default)]
    pub reasoning_tokens: u32,
    pub input_cost: f64,
    pub output_cost: f64,
    pub total_cost: f64,
    pub cumulative_invocation_cost: f64,
    pub cumulative_agent_cost: f64,
    /// What prompted the priced call (agent turn vs sampling), so
    /// sampling spend is attributable to its server while still
    /// counting toward the invocation total. Defaults to `AgentTurn`.
    #[serde(default)]
    pub origin: LlmCallOrigin,
}
