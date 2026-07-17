//! Event schema for factor-q.
//!
//! Every event on the bus has three structurally distinct layers:
//!
//! - [`Envelope`] — runtime-written system metadata. Closed schema.
//! - [`EventPayload`] — typed contract between graph nodes. The only
//!   thing that drives downstream agent behaviour.
//! - [`Annotations`] — open key/value commentary from the producing
//!   agent. **Never** read by consuming agents — the runtime will
//!   strip annotations when building a downstream prompt (the
//!   barrier lands in step 4 of the envelope-refactor plan).
//!
//! Each layer has different write permissions, read audiences, and
//! mutability rules; see
//! `docs/design/aspirational/inter-node-contracts-and-event-layers.md` for the
//! rationale.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::agent::AgentId;

pub const SCHEMA_VERSION: u32 = 2;

/// Well-known annotation keys. Kept as documented constants so the
/// learning loop has a stable vocabulary; unknown keys are still
/// permitted in the [`Annotations`] map.
///
/// Per §6 of `inter-node-contracts-and-event-layers.md`, every key
/// here is **advisory** — annotations are never read by consuming
/// agents. The runtime strips them at the consumer-context
/// boundary via [`Event::for_consumer_context`]; downstream prompts
/// see envelope + payload only.
pub mod annotation_keys {
    /// Free-form commentary from the producing agent.
    pub const NOTES: &str = "notes";
    /// Self-reported confidence. Advisory only — never read by
    /// consumers. Calibrated confidence comes from a verifier
    /// node, not from the producer.
    pub const CONFIDENCE: &str = "confidence";
    /// Chain-of-thought / working. Never read by consumers — the
    /// fresh-context discipline depends on the reasoning trace
    /// never reaching a downstream agent's prompt.
    pub const REASONING: &str = "reasoning";
    /// Sources looked at but not directly used in the payload.
    /// Sources actually used belong in a typed `Citation[]` field
    /// on the payload.
    pub const SOURCES_CONSIDERED: &str = "sources_considered";
    /// Markers the producer wants downstream humans (or a meta-
    /// agent) to see.
    pub const FLAGS: &str = "flags";
}

/// Subject hierarchy for factor-q events.
pub mod subjects {
    pub const SYSTEM_STARTUP: &str = "fq.system.startup";
    pub const SYSTEM_SHUTDOWN: &str = "fq.system.shutdown";
    pub const SYSTEM_TASK_FAILED: &str = "fq.system.task_failed";
    pub const SYSTEM_RECOVERY: &str = "fq.system.recovery";
    /// Daemon-scoped log records forwarded from connected MCP servers
    /// (ADR-0020).
    pub const SYSTEM_MCP_LOG: &str = "fq.system.mcp.log";

    /// Validate that `s` is safe to use as a single NATS subject
    /// token. NATS subjects are dot-separated tokens; a token
    /// must be non-empty and must not contain `.`, `*`, `>`, or
    /// whitespace. This is the shared predicate used by every
    /// id-newtype whose value lands in a subject string
    /// (currently [`crate::agent::AgentId`] and
    /// [`crate::worker::WorkerId`]). Wrapping the constraint in
    /// a single function means the validation can't drift
    /// between sites.
    pub fn validate_token(s: &str) -> Result<(), SubjectTokenError> {
        if s.is_empty() {
            return Err(SubjectTokenError::Empty);
        }
        for ch in s.chars() {
            if ch == '.' || ch == '*' || ch == '>' || ch.is_whitespace() {
                return Err(SubjectTokenError::InvalidChar(s.to_string()));
            }
        }
        Ok(())
    }

    /// Failure mode from [`validate_token`].
    #[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
    pub enum SubjectTokenError {
        #[error("must not be empty")]
        Empty,
        #[error("must not contain '.', '*', '>', or whitespace: {0:?}")]
        InvalidChar(String),
    }

    pub fn agent_triggered(agent_id: &str) -> String {
        format!("fq.agent.{agent_id}.triggered")
    }

    pub fn agent_llm_request(agent_id: &str) -> String {
        format!("fq.agent.{agent_id}.llm.request")
    }

    pub fn agent_llm_response(agent_id: &str) -> String {
        format!("fq.agent.{agent_id}.llm.response")
    }

    pub fn agent_tool_call(agent_id: &str) -> String {
        format!("fq.agent.{agent_id}.tool.call")
    }

    pub fn agent_tool_dispatched(agent_id: &str) -> String {
        format!("fq.agent.{agent_id}.tool.dispatched")
    }

    pub fn agent_tool_result(agent_id: &str) -> String {
        format!("fq.agent.{agent_id}.tool.result")
    }

    pub fn agent_llm_dispatched(agent_id: &str) -> String {
        format!("fq.agent.{agent_id}.llm.dispatched")
    }

    /// An invocation cannot be auto-recovered (see
    /// data-architecture.md §3.4). The worker publishes this
    /// on startup when its WAL categorisation finds a
    /// `dispatched`-without-`completed` row.
    pub fn agent_invocation_ambiguous(agent_id: &str) -> String {
        format!("fq.agent.{agent_id}.invocation.ambiguous")
    }

    /// Worker → control-plane archive hand-off (step 8 of
    /// data-architecture.md). Emitted by the worker after an
    /// invocation reaches terminal state, carrying the final
    /// state blob the control-plane writes into
    /// `invocation_archive`.
    pub fn agent_invocation_archived(agent_id: &str) -> String {
        format!("fq.agent.{agent_id}.invocation.archived")
    }

    /// Operator → control-plane (step 9). Emitted by an
    /// operator-issued `fq invocation drop` (or future
    /// recovery actions) so audit can distinguish operator-
    /// triggered terminal transitions from worker-triggered
    /// ones. The coordination consumer's existing
    /// `fq.agent.*.invocation.*` filter picks it up.
    pub fn agent_invocation_operator_recovered(agent_id: &str) -> String {
        format!("fq.agent.{agent_id}.invocation.operator_recovered")
    }

    /// A durable host notice injected into the invocation's
    /// conversation at a step boundary (#155). Deliberately outside
    /// the coordination consumer's `fq.agent.*.invocation.*` filter —
    /// notices are conversation-plane observability, not coordination.
    pub fn agent_host_notice(agent_id: &str) -> String {
        format!("fq.agent.{agent_id}.host_notice")
    }

    pub fn agent_invocation_summary(agent_id: &str) -> String {
        format!("fq.agent.{agent_id}.invocation_summary")
    }

    pub fn agent_completed(agent_id: &str) -> String {
        format!("fq.agent.{agent_id}.completed")
    }

    pub fn agent_failed(agent_id: &str) -> String {
        format!("fq.agent.{agent_id}.failed")
    }

    /// Every agent's `failed` subject — the dead-letter scan's filter.
    pub const ALL_AGENTS_FAILED: &str = "fq.agent.*.failed";

    /// Worker liveness signal. Emitted periodically by each
    /// worker; the control-plane's heartbeat consumer updates
    /// `coordination_worker.last_heartbeat` on receipt. The
    /// stale-worker sweep in the coordination consumer marks a
    /// worker stale when this signal falls behind its threshold.
    pub fn worker_heartbeat(worker_id: &str) -> String {
        format!("fq.worker.{worker_id}.heartbeat")
    }

    /// Worker liveness transition emitted once when a heartbeat lapses.
    pub fn worker_orphaned(worker_id: &str) -> String {
        format!("fq.worker.{worker_id}.orphaned")
    }

    /// Control-plane → worker acknowledgement of an
    /// `invocation.archived` hand-off. Worker-scoped so a worker
    /// can subscribe to its own acks with a single filter
    /// (mirrors the heartbeat naming). The invocation_id lives
    /// on the envelope.
    pub fn worker_invocation_archive_acked(worker_id: &str) -> String {
        format!("fq.worker.{worker_id}.invocation.archive_acked")
    }
}

/// A complete event: envelope + payload + annotations.
///
/// The three layers are kept as separate fields rather than
/// flattened so the trust/visibility boundary between them is
/// expressed in the type system. Producing agents do not touch the
/// envelope; consuming agents must not read annotations (step 4
/// adds the runtime-enforced barrier via
/// [`Event::for_consumer_context`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub envelope: Envelope,
    pub payload: EventPayload,
    #[serde(default, skip_serializing_if = "Annotations::is_empty")]
    pub annotations: Annotations,
}

impl Event {
    /// Construct a new event for the given agent and invocation.
    /// The envelope is stamped with a fresh `event_id`, the current
    /// time, `trace_id = invocation_id` (single-trace-per-invocation
    /// for now), and `schema_id` derived from the payload variant.
    /// `parent_event_id` is `None`; chain it later with
    /// [`Event::with_parent`] (step 2 of the envelope refactor).
    pub fn new(agent_id: AgentId, invocation_id: Uuid, payload: EventPayload) -> Self {
        let envelope = Envelope {
            schema_version: SCHEMA_VERSION,
            event_id: Uuid::now_v7(),
            parent_event_id: None,
            trace_id: invocation_id,
            agent_id,
            invocation_id,
            schema_id: schema_id_for(&payload).to_string(),
            timestamp: Utc::now(),
            cost: None,
        };
        Self {
            envelope,
            payload,
            annotations: Annotations::default(),
        }
    }

    /// Construct a system event. System events use the sentinel
    /// agent id `"system"`; the runtime id doubles as the
    /// invocation id and trace id so every system event from a
    /// single daemon run shares a correlation key.
    pub fn system(runtime_id: Uuid, payload: EventPayload) -> Self {
        let envelope = Envelope {
            schema_version: SCHEMA_VERSION,
            event_id: Uuid::now_v7(),
            parent_event_id: None,
            trace_id: runtime_id,
            agent_id: AgentId::system(),
            invocation_id: runtime_id,
            schema_id: schema_id_for(&payload).to_string(),
            timestamp: Utc::now(),
            cost: None,
        };
        Self {
            envelope,
            payload,
            annotations: Annotations::default(),
        }
    }

    /// Chain this event's envelope to a prior event in the same
    /// invocation. The reducer runner threads the previously-
    /// published event's id through each subsequent publish so the
    /// projection (and any future replay) can reconstruct
    /// happens-before from the envelope chain rather than from
    /// timestamps. System events and recovery re-emits leave the
    /// parent unset (the chain restarts) — see the
    /// `parent_event_id` field doc on [`Envelope`] for the
    /// resolved semantics.
    pub fn with_parent(mut self, parent_event_id: Uuid) -> Self {
        self.envelope.parent_event_id = Some(parent_event_id);
        self
    }

    /// Attach cost metadata to the envelope. Per ADR-0016 and §7 of
    /// `inter-node-contracts-and-event-layers.md`, cost is
    /// system-level accounting (not part of the typed contract
    /// between graph nodes) so it rides on the envelope rather than
    /// as a payload variant. Populated on `llm.response` events;
    /// absent on events that do not bill.
    pub fn with_cost(mut self, cost: CostMetadata) -> Self {
        self.envelope.cost = Some(cost);
        self
    }

    /// Add or replace an annotation. Annotations are advisory and
    /// never reach consuming agents — the runtime strips them when
    /// building a downstream prompt via
    /// [`Event::for_consumer_context`]. See the
    /// [`annotation_keys`] module for well-known keys; unknown keys
    /// are permitted and logged.
    pub fn annotate(mut self, key: impl Into<String>, value: Value) -> Self {
        self.annotations.0.insert(key.into(), value);
        self
    }

    /// Build the consumer-facing view of this event: envelope and
    /// payload only, annotations stripped.
    ///
    /// This is the **only** sanctioned way to feed an upstream
    /// event into a downstream agent's prompt context. A consumer
    /// that reads annotations turns them into a structured-bypass
    /// channel for cross-node coupling, which destroys the
    /// path-independence that justifies multi-invocation in the
    /// first place (§6 of
    /// `inter-node-contracts-and-event-layers.md`).
    ///
    /// The reasoning-trace case matters specifically: fresh-context
    /// verification only works if the verifier does not see the
    /// producer's reasoning. If reasoning leaks via annotations
    /// into a downstream agent's input, the path-independence is
    /// lost.
    pub fn for_consumer_context(&self) -> ConsumerView<'_> {
        ConsumerView {
            envelope: &self.envelope,
            payload: &self.payload,
        }
    }

    /// Return the NATS subject this event should be published on.
    pub fn subject(&self) -> String {
        self.payload.subject(self.envelope.agent_id.as_str())
    }
}

/// Consumer-facing view of an event: envelope + payload, with
/// annotations stripped at the type level.
///
/// Constructed via [`Event::for_consumer_context`]. Carries
/// references, so it's zero-copy; serialise it to JSON and pass
/// it to a downstream agent's prompt builder. Direct access to
/// `event.annotations` remains available for humans, meta-agents,
/// and the learning loop — only the consumer path is barred.
#[derive(Debug, Clone, Serialize)]
pub struct ConsumerView<'a> {
    pub envelope: &'a Envelope,
    pub payload: &'a EventPayload,
}

/// Validated identifier for a tool call.
///
/// Tool call ids are generated by the LLM provider and used as a
/// correlation key across the `tool.call` / `tool.dispatched` /
/// `tool.result` events, the WAL `tool_dispatch` rows, and the
/// tool-role messages fed back to the LLM. The newtype catches a
/// real bug class: every one of those uses is a bare `String`
/// today, so a code change that swaps `tool_call_id` for
/// `invocation_id` (or any other id) compiles fine.
///
/// Validation is intentionally minimal — non-empty only. Tool ids
/// originate from external providers (Anthropic / OpenAI / etc.)
/// and the runtime should not enforce a provider-specific shape.
/// Deserialise runs the same check so wire-format malformation
/// surfaces at parse time.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ToolCallId(String);

impl ToolCallId {
    pub fn new(s: impl Into<String>) -> Result<Self, ToolCallIdError> {
        let s = s.into();
        if s.is_empty() {
            return Err(ToolCallIdError::Empty);
        }
        Ok(Self(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ToolCallIdError {
    #[error("tool_call_id must not be empty")]
    Empty,
}

impl std::fmt::Display for ToolCallId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl AsRef<str> for ToolCallId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl PartialEq<str> for ToolCallId {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for ToolCallId {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl Serialize for ToolCallId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ToolCallId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::new(s).map_err(serde::de::Error::custom)
    }
}

/// Stable event metadata, co-located so a payload variant has one
/// exhaustive definition for its subject and schema. The projection event type
/// is derived from the schema id rather than maintained as a fourth mapping.
impl EventPayload {
    pub fn subject(&self, agent: &str) -> String {
        match self {
            Self::Triggered(_) => subjects::agent_triggered(agent),
            Self::LlmRequest(_) => subjects::agent_llm_request(agent),
            Self::LlmResponse(_) => subjects::agent_llm_response(agent),
            Self::ToolCall(_) => subjects::agent_tool_call(agent),
            Self::ToolDispatched(_) => subjects::agent_tool_dispatched(agent),
            Self::ToolResult(_) => subjects::agent_tool_result(agent),
            Self::LlmDispatched(_) => subjects::agent_llm_dispatched(agent),
            Self::InvocationAmbiguous(_) => subjects::agent_invocation_ambiguous(agent),
            Self::InvocationArchived(_) => subjects::agent_invocation_archived(agent),
            Self::InvocationOperatorRecovered(_) => {
                subjects::agent_invocation_operator_recovered(agent)
            }
            Self::Completed(_) => subjects::agent_completed(agent),
            Self::Failed(_) => subjects::agent_failed(agent),
            Self::HostNotice(_) => subjects::agent_host_notice(agent),
            Self::InvocationSummary(_) => subjects::agent_invocation_summary(agent),
            Self::SystemStartup(_) => subjects::SYSTEM_STARTUP.to_string(),
            Self::SystemShutdown(_) => subjects::SYSTEM_SHUTDOWN.to_string(),
            Self::SystemTaskFailed(_) => subjects::SYSTEM_TASK_FAILED.to_string(),
            Self::SystemRecovery(_) => subjects::SYSTEM_RECOVERY.to_string(),
            Self::McpServerLog(_) => subjects::SYSTEM_MCP_LOG.to_string(),
            Self::WorkerHeartbeat(p) => subjects::worker_heartbeat(p.worker_id.as_str()),
            Self::WorkerOrphaned(p) => subjects::worker_orphaned(p.worker_id.as_str()),
            Self::InvocationArchiveAcked(p) => {
                subjects::worker_invocation_archive_acked(p.worker_id.as_str())
            }
        }
    }

    pub fn schema_id(&self) -> &'static str {
        match self {
            Self::Triggered(_) => "factor-q/triggered@1",
            Self::LlmRequest(_) => "factor-q/llm_request@1",
            Self::LlmDispatched(_) => "factor-q/llm_dispatched@1",
            Self::LlmResponse(_) => "factor-q/llm_response@1",
            Self::ToolCall(_) => "factor-q/tool_call@1",
            Self::ToolDispatched(_) => "factor-q/tool_dispatched@1",
            Self::ToolResult(_) => "factor-q/tool_result@1",
            Self::InvocationSummary(_) => "factor-q/invocation_summary@1",
            Self::Completed(_) => "factor-q/completed@1",
            Self::Failed(_) => "factor-q/failed@1",
            Self::HostNotice(_) => "factor-q/host_notice@1",
            Self::InvocationAmbiguous(_) => "factor-q/invocation_ambiguous@1",
            Self::InvocationArchived(_) => "factor-q/invocation_archived@1",
            Self::InvocationArchiveAcked(_) => "factor-q/invocation_archive_acked@1",
            Self::InvocationOperatorRecovered(_) => "factor-q/invocation_operator_recovered@1",
            Self::SystemStartup(_) => "factor-q/system_startup@1",
            Self::SystemShutdown(_) => "factor-q/system_shutdown@1",
            Self::SystemTaskFailed(_) => "factor-q/system_task_failed@1",
            Self::SystemRecovery(_) => "factor-q/system_recovery@1",
            Self::WorkerHeartbeat(_) => "factor-q/worker_heartbeat@1",
            Self::WorkerOrphaned(_) => "factor-q/worker_orphaned@1",
            Self::McpServerLog(_) => "factor-q/mcp_server_log@1",
        }
    }

    pub fn event_type(&self) -> &'static str {
        self.schema_id()
            .strip_prefix("factor-q/")
            .unwrap()
            .strip_suffix("@1")
            .unwrap()
    }
}

/// Stable identifier for an event's payload schema. Versioned from day one.
pub fn schema_id_for(payload: &EventPayload) -> &'static str {
    payload.schema_id()
}

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
    /// `"factor-q/triggered@1"`. See [`schema_id_for`].
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

/// Open key/value commentary. Producing agents may attach anything
/// here. Step 4 of the envelope-refactor plan introduces the
/// well-known keys module, the [`Event::annotate`] builder, and the
/// consumer-context barrier method that strips annotations before
/// they reach a downstream agent's prompt.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Annotations(pub BTreeMap<String, Value>);

impl Annotations {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Per-type event payloads, tagged by `event_type`.
///
/// `Triggered` is intentionally the largest variant — it carries the
/// full [`ConfigSnapshot`] (system prompt, sandbox, capability grants)
/// for audit/replay. It's emitted once per invocation, not on the hot
/// per-step path, so we accept the size rather than box it (which would
/// add a heap allocation to a value that is always serialized to NATS
/// anyway).
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event_type", content = "payload", rename_all = "snake_case")]
pub enum EventPayload {
    // Agent lifecycle
    Triggered(TriggeredPayload),
    LlmRequest(LlmRequestPayload),
    /// WAL middle-state for LLM calls. Emitted between
    /// `LlmRequest` and `LlmResponse` once the request has
    /// returned control to the runtime, before the response is
    /// durably written. See data-architecture.md §3.2.
    LlmDispatched(LlmDispatchedPayload),
    LlmResponse(LlmResponsePayload),
    ToolCall(ToolCallPayload),
    /// WAL middle-state for tool calls. Emitted between
    /// `ToolCall` and `ToolResult` once the tool has returned
    /// control to the runtime, before the result is durably
    /// written. See data-architecture.md §3.1.
    ToolDispatched(ToolDispatchedPayload),
    ToolResult(ToolResultPayload),
    Completed(CompletedPayload),
    Failed(FailedPayload),

    /// A durable host notice injected into the conversation at a
    /// reducer step boundary (#155, phase 1 of #88). Emitted by the
    /// runner when a queued notice is drained and WAL-persisted, so
    /// operators see notices without diffing message arrays. The WAL
    /// row — not this event — is the channel's source of truth: a
    /// notice recorded by a crashed incarnation is *not* re-emitted
    /// on resume.
    HostNotice(HostNoticePayload),

    /// A one-line operator-facing summary of an invocation (#216),
    /// emitted by the summary consumer under the reserved `summary`
    /// agent id (never by the invocation's own agent). The envelope's
    /// `invocation_id` binds it to the summarised invocation; the
    /// summariser's own LLM spend rides `envelope.cost` exactly like
    /// an `llm_response`, so `fq costs` reports it as agent `summary`
    /// without touching the invocation's totals or budget.
    InvocationSummary(InvocationSummaryPayload),

    /// An in-flight invocation could not be auto-recovered
    /// on worker restart (see data-architecture.md §3.4).
    /// The worker publishes this when its WAL categorisation
    /// finds a `dispatched`-without-`completed` row. The
    /// control-plane consumes the event to surface the case
    /// via `fq recover` (step 9).
    InvocationAmbiguous(InvocationAmbiguousPayload),

    /// Worker → control-plane archive hand-off (step 8 of
    /// data-architecture.md). Emitted after an invocation
    /// reaches terminal state with the final reducer-state blob
    /// the control-plane needs to write
    /// `invocation_archive`. The worker holds onto its local
    /// `invocation_state` row until the corresponding
    /// [`Self::InvocationArchiveAcked`] arrives.
    InvocationArchived(InvocationArchivedPayload),

    /// Control-plane → worker acknowledgement of an
    /// [`Self::InvocationArchived`] event. On receipt the worker
    /// deletes the local `invocation_state` row. The invocation
    /// id lives on the envelope; the payload carries `worker_id`
    /// only because the subject is built from it (mirroring
    /// [`Self::WorkerHeartbeat`]).
    InvocationArchiveAcked(InvocationArchiveAckedPayload),

    /// Operator → control-plane (step 9). Emitted by
    /// `fq invocation drop` and other future operator-issued
    /// recovery actions. Distinct from [`Self::InvocationArchived`]
    /// so audit can filter operator-triggered terminal
    /// transitions from worker-triggered ones. The
    /// coordination consumer writes an `invocation_archive`
    /// row and updates `coordination_invocation_owner.status`
    /// to match `final_phase`; no ack is emitted (no worker
    /// is waiting to clean up).
    InvocationOperatorRecovered(InvocationOperatorRecoveredPayload),

    // Runtime lifecycle
    SystemStartup(SystemStartupPayload),
    SystemShutdown(SystemShutdownPayload),
    SystemTaskFailed(SystemTaskFailedPayload),

    /// Emitted once per daemon startup with the counts of
    /// in-flight invocations classified by recovery category
    /// (data-architecture.md §7.1). The projection records
    /// these so operators can see recovery history via
    /// `fq events query --type=system_recovery` without
    /// needing a Prometheus-style endpoint. A live snapshot
    /// is also available via `fq status`.
    SystemRecovery(SystemRecoveryPayload),

    /// Worker liveness signal. Emitted periodically by each
    /// worker; the control-plane's heartbeat consumer updates
    /// `coordination_worker.last_heartbeat` on receipt. The
    /// timestamp lives on the envelope (`envelope.timestamp`),
    /// not in the payload, so there's only one source of
    /// truth for "when did this beat arrive."
    WorkerHeartbeat(WorkerHeartbeatPayload),
    /// A worker heartbeat lapsed without a clean shutdown. Emitted once
    /// for the alive-to-stale transition by the coordination consumer.
    WorkerOrphaned(WorkerOrphanedPayload),
    /// A log record forwarded from a connected MCP server
    /// (`notifications/message`), bridged onto the bus by the daemon's
    /// notification drain (ADR-0020). Daemon-scoped — no agent or
    /// invocation.
    McpServerLog(McpServerLogPayload),
}

/// The fixed sentinel every host-notice body is wrapped in
/// (`<host-notice>…</host-notice>`) — one marker for every producer,
/// forever (#88). It separates "runtime ambient info" from "principal
/// speaking" in the conversation, and the equivalence oracle strips
/// sentinel-prefixed user messages when comparing resumed traces
/// against uninterrupted references.
pub const HOST_NOTICE_SENTINEL: &str = "<host-notice>";

/// A durable host notice injected at a reducer step boundary (#155).
/// `body` is the producer-rendered text, sentinel included — the exact
/// string persisted in the WAL and replayed verbatim.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostNoticePayload {
    /// Producer discriminator (`resume` | `tools_changed` |
    /// `context_pressure`, …).
    pub kind: String,
    pub body: String,
}

/// Which moment of the invocation an [`EventPayload::InvocationSummary`]
/// describes. `Outcome` is terminal — the last summary an invocation
/// receives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SummaryKind {
    /// From the trigger payload: what work was expected.
    Start,
    /// Rolling update from the latest model turn: what it is doing now.
    Progress,
    /// Final line on `completed`/`failed`, naming the failure kind.
    Outcome,
}

/// A one-line operator-facing invocation summary (#216). The
/// summariser's token usage and cost ride `envelope.cost`
/// ([`CostMetadata`]) exactly as they do for `llm_response` events, so
/// the payload carries only the line itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvocationSummaryPayload {
    pub kind: SummaryKind,
    /// The single summary line (bounded by `[summary].max_line_chars`).
    pub summary: String,
}

/// Annotation keys shared by the two dead-letter emitters (#49/#169):
/// the dispatcher's inline path and the advisory watch. `trigger_*`
/// carries what a requeue needs; `trigger_stream_seq` is the dedup /
/// reconciliation key; `dead_letter_source` says which path emitted
/// (`"inline"` | `"advisory"`).
pub const DEAD_LETTER_SUBJECT_KEY: &str = "trigger_subject";
pub const DEAD_LETTER_PAYLOAD_KEY: &str = "trigger_payload";
pub const DEAD_LETTER_STREAM_SEQ_KEY: &str = "trigger_stream_seq";
pub const DEAD_LETTER_SOURCE_KEY: &str = "dead_letter_source";

/// Published when an agent invocation begins.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggeredPayload {
    pub trigger_source: TriggerSource,
    pub trigger_subject: Option<String>,
    pub trigger_payload: Value,
    pub config_snapshot: ConfigSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerSource {
    Manual,
    Subject,
    Schedule,
}

/// Snapshot of the agent's configuration at trigger time.
///
/// Captured on `triggered` so that replay is meaningful even if the agent
/// definition is later modified.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConfigSnapshot {
    pub name: String,
    pub model: String,
    pub system_prompt: String,
    pub tools: Vec<String>,
    pub sandbox: SandboxSnapshot,
    pub budget: Option<f64>,
    /// MCP capability grants (ADR-0017) captured for audit. Absent for
    /// snapshots written before Step 8 / for agents that grant nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampling: Option<crate::agent::SamplingGrant>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roots: Option<crate::agent::RootsGrant>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elicitation: Option<crate::agent::ElicitationGrant>,
    /// Per-capability validation policy (redaction + evaluator gates),
    /// captured for audit. Default-empty for agents that configure none.
    #[serde(
        default,
        skip_serializing_if = "crate::agent::CapabilityValidation::is_empty"
    )]
    pub sampling_validation: crate::agent::CapabilityValidation,
    #[serde(
        default,
        skip_serializing_if = "crate::agent::CapabilityValidation::is_empty"
    )]
    pub elicitation_validation: crate::agent::CapabilityValidation,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SandboxSnapshot {
    #[serde(default)]
    pub fs_read: Vec<String>,
    #[serde(default)]
    pub fs_write: Vec<String>,
    #[serde(default)]
    pub network: Vec<String>,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub exec_cwd: Vec<String>,
}

/// What prompted an LLM call, for cost attribution (ADR-0004 /
/// ADR-0018). Agent turns are the reducer's own reasoning steps;
/// sampling calls are server-initiated (`sampling/createMessage`) and
/// tagged with the originating MCP server so their spend is distinct
/// from the agent's own turns while still bounded by the same
/// invocation budget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LlmCallOrigin {
    /// A reducer-driven agent reasoning turn (the default).
    #[default]
    AgentTurn,
    /// A server-initiated sampling completion, attributed to the
    /// requesting MCP server.
    Sampling { server: String },
    /// A server-initiated elicitation completion (structured input),
    /// attributed to the requesting MCP server.
    Elicitation { server: String },
}

/// Published immediately before an LLM call is made.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmRequestPayload {
    pub call_id: Uuid,
    pub model: String,
    pub messages: Vec<Message>,
    pub tools_available: Vec<ToolSchema>,
    pub request_params: RequestParams,
    /// What prompted this call — agent turn vs server-initiated sampling
    /// / elicitation (ADR-0018) — mirroring the cost event's attribution
    /// so the request/response trace is self-describing. `default` =
    /// `AgentTurn` for events persisted before this field existed.
    #[serde(default)]
    pub origin: LlmCallOrigin,
}

/// WAL middle-state event for LLM dispatch. Emitted between
/// [`LlmRequestPayload`] and [`LlmResponsePayload`] once the
/// LLM call has returned control to the runtime — before the
/// response is durably written.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmDispatchedPayload {
    pub call_id: Uuid,
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: MessageRole,
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<MessageToolCall>,
    /// ID correlating a `tool` role message with a prior assistant tool
    /// call. Assigned by the LLM provider and carried through as-is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<ToolCallId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageToolCall {
    /// ID assigned by the LLM provider. Carried through unchanged so
    /// that `tool.call` and `tool.result` events can be correlated with
    /// the raw provider response.
    pub tool_call_id: ToolCallId,
    pub tool_name: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters_schema: Value,
}

/// Per-request model reasoning effort. `None` leaves the provider default.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    /// Disables (nearly) all reasoning — required for gpt-5-family
    /// models on short mechanical tasks: their default reasoning
    /// scales to fill `max_tokens`, returning empty content (found
    /// live: the #216 summariser produced nothing on gpt-5-nano).
    Minimal,
    Low,
    Medium,
    High,
    #[serde(rename = "xhigh")]
    XHigh,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}

/// Published when an LLM call returns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmResponsePayload {
    pub call_id: Uuid,
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<MessageToolCall>,
    pub stop_reason: StopReason,
    pub usage: TokenUsage,
    /// What prompted this call (see [`LlmRequestPayload::origin`]).
    #[serde(default)]
    pub origin: LlmCallOrigin,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    ToolUse,
    EndTurn,
    MaxTokens,
    StopSequence,
}

/// Token usage for one LLM call.
///
/// Invariant: `input_tokens` is the **total** prompt size;
/// `cache_read_tokens` and `cache_write_tokens` are subsets of it
/// (the uncached portion is `input - read - write`). The genai
/// adapter normalises every provider to this shape — Anthropic
/// reports the three parts separately and they are summed; OpenAI
/// and Gemini already report totals with cached counts as details.
/// Pricing depends on this (see [`crate::pricing::ModelPricing`]).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    #[serde(default)]
    pub cache_read_tokens: u32,
    #[serde(default)]
    pub cache_write_tokens: u32,
}

/// Published when the agent invokes a tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallPayload {
    pub tool_call_id: ToolCallId,
    pub tool_name: String,
    pub parameters: Value,
}

/// WAL middle-state event for tool dispatch. Emitted between
/// [`ToolCallPayload`] and [`ToolResultPayload`] once the tool
/// has returned control to the runtime — before the result is
/// durably written.
///
/// Operationally informational: downstream consumers can ignore
/// it (existing consumers do). Recovery uses the matching
/// `tool_dispatch.status = 'dispatched'` row in the worker
/// store, not this event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDispatchedPayload {
    pub tool_call_id: ToolCallId,
    pub tool_name: String,
}

/// Payload for [`EventPayload::InvocationAmbiguous`]. Carries
/// the minimum context an operator needs to make a recovery
/// decision: which kind of dispatch was stuck, and which
/// call_id it was on. The full context (parameters, request
/// payload, etc.) is in the worker's WAL and surfaced via
/// `fq recover` (step 9).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvocationAmbiguousPayload {
    /// Which entity in the WAL was stuck: `tool_dispatch` or
    /// `llm_dispatch`. Domain name, not a relational table
    /// reference (see WorkerStoreError::WalTransitionFailed).
    pub stuck_entity: String,
    /// The `tool_call_id` (for tools) or `request_id` (for LLM
    /// calls) of the stuck dispatch.
    pub stuck_call_id: String,
    /// Free-form note describing the operator-relevant context.
    pub note: String,
}

/// Payload for [`EventPayload::InvocationArchived`]. Carries
/// the data the control-plane needs to populate
/// `invocation_archive`: emitting worker, terminal phase, final
/// state blob, and the timestamps the archive row's primary
/// index uses. `agent_id` and `invocation_id` live on the
/// envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvocationArchivedPayload {
    /// Worker that owned the invocation. The control-plane uses
    /// this to address the ack back at
    /// `fq.worker.{worker_id}.invocation.archive_acked`.
    pub worker_id: crate::worker::WorkerId,
    /// Phase label as written into `invocation_state.phase` at
    /// terminal — `completed` or `failed`. Domain string, not a
    /// typed enum, because the phase vocabulary lives in the
    /// reducer harness, not the events layer.
    pub final_phase: String,
    /// Reducer state at the time of terminal. Opaque blob; the
    /// control-plane stores it as-is. Default serde encoding
    /// (JSON array of integers) is used to keep parity with the
    /// worker store's `state_blob` shape; if blob sizes start to
    /// strain the wire format, swap in `serde_bytes` here and
    /// in `InvocationStateRow`.
    pub final_state_blob: Vec<u8>,
    /// `invocation_state.started_at` (unix ms).
    pub started_at_ms: i64,
    /// `invocation_state.terminal_at` (unix ms).
    pub terminal_at_ms: i64,
}

/// Payload for [`EventPayload::InvocationArchiveAcked`]. The
/// invocation id lives on the envelope; the payload carries
/// `worker_id` only because the subject token comes from there.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvocationArchiveAckedPayload {
    pub worker_id: crate::worker::WorkerId,
}

/// Payload for [`EventPayload::InvocationOperatorRecovered`].
/// Operator-issued terminal transition for an invocation.
/// The `invocation_id` and `agent_id` live on the envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvocationOperatorRecoveredPayload {
    /// Action the operator took. v1 is always `"drop"`; the
    /// field exists so future actions (`resume`, `requeue`)
    /// can be distinguished without minting a new variant.
    pub action: String,
    /// Phase the invocation should be marked at. v1 is
    /// always `"failed"`; a future `resume` would set
    /// `"completed"`.
    pub final_phase: String,
    /// Free-form reason supplied by the operator (e.g. via
    /// `--reason`). Audit-only; consumers must not parse it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Published when a tool invocation completes (success or failure).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultPayload {
    pub tool_call_id: ToolCallId,
    pub output: String,
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<ToolErrorKind>,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolErrorKind {
    SandboxViolation,
    InvalidParameters,
    ExecutionFailed,
    Timeout,
    PermissionDenied,
}

/// Published when an invocation finishes successfully.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletedPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_summary: Option<String>,
    pub total_llm_calls: u32,
    pub total_tool_calls: u32,
    pub total_cost: f64,
    pub total_duration_ms: u64,
}

/// Published when an invocation terminates with an error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailedPayload {
    pub error_kind: FailureKind,
    pub error_message: String,
    pub phase: FailurePhase,
    pub partial_totals: InvocationTotals,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    BudgetExceeded,
    LlmError,
    /// The agent's `max_iterations` cap was reached before the model
    /// declared a final answer. Distinct from `RuntimeError`: hitting
    /// the cap is a configured limit doing its job, not a defect.
    MaxIterations,
    ToolError,
    SandboxViolation,
    RuntimeError,
    /// A transient pre-WAL trigger failure exhausted the consumer's
    /// delivery bound (#49) and was dead-lettered. Distinct from
    /// `RuntimeError` so operators can count and list dead letters
    /// (`fq doctor`, the dashboard) rather than losing them in the
    /// generic bucket.
    TriggerExhausted,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailurePhase {
    Setup,
    LlmRequest,
    LlmResponse,
    ToolCall,
    ToolResult,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct InvocationTotals {
    pub total_llm_calls: u32,
    pub total_tool_calls: u32,
    pub total_cost: f64,
    pub total_duration_ms: u64,
    /// Cumulative spend on server-initiated sampling within this
    /// invocation (a subset of `total_cost`), tracked separately so
    /// the sampling sub-budget can be enforced (ADR-0018). Defaults
    /// to 0 for totals written before sampling existed.
    #[serde(default)]
    pub sampling_cost: f64,
    /// Cumulative spend on server-initiated elicitation within this
    /// invocation (a subset of `total_cost`), tracked separately so
    /// the elicitation sub-budget can be enforced (ADR-0018).
    #[serde(default)]
    pub elicitation_cost: f64,
}

/// Published when the `fq run` daemon starts up.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemStartupPayload {
    /// Unique id for this daemon run. All system events from a
    /// single `fq run` invocation share this id.
    pub runtime_id: Uuid,
    /// Version of the fq binary (the value of `CARGO_PKG_VERSION`
    /// at build time).
    pub version: String,
    /// NATS URL the daemon is connected to.
    pub nats_url: String,
    /// Number of agents loaded from the configured agents
    /// directory at startup.
    pub agents_loaded: u32,
    /// Number of pricing entries loaded.
    pub pricing_entries: u32,
}

/// Published when the `fq run` daemon shuts down.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemShutdownPayload {
    pub runtime_id: Uuid,
    /// Short machine-readable reason, e.g. `"ctrl_c"`,
    /// `"task_failed"`, `"error"`.
    pub reason: String,
    /// True if the shutdown was requested gracefully (Ctrl-C,
    /// operator intervention), false if it was triggered by an
    /// unexpected task failure or error.
    pub clean: bool,
}

/// Published when one of the hosted tasks inside `fq run` (the
/// projection consumer, the trigger dispatcher, etc.) exits with
/// an error before a graceful shutdown was requested.
///
/// These events are the canary for "the daemon looks alive but a
/// piece of it silently stopped working". The runtime publishes
/// one per task failure and then shuts itself down so operators
/// don't unknowingly rely on a half-broken daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemTaskFailedPayload {
    pub runtime_id: Uuid,
    /// Symbolic name of the task that failed (e.g.
    /// `projection_consumer`, `trigger_dispatcher`).
    pub task_name: String,
    pub error_message: String,
}

/// Counts of in-flight invocations classified by recovery
/// category at daemon startup. Emitted once per `fq run`
/// after the worker recovery scan completes.
///
/// The same counts are surfaced live via `fq status`; this
/// event records the snapshot so historical recovery
/// behaviour is queryable through the existing event
/// projection (`fq events query --type=system_recovery`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemRecoveryPayload {
    pub runtime_id: Uuid,
    pub worker_id: String,
    /// Number of invocations classified as safe-resume
    /// (intent-only or no dispatches; can be auto-recovered
    /// by re-running from the persisted state).
    pub safe_resume: u32,
    /// Number of invocations classified as safe-replay
    /// (action completed; result fed to next reducer step).
    pub safe_replay: u32,
    /// Number of invocations classified as ambiguous
    /// (dispatched-without-completed; surfaced to operator
    /// rather than auto-recovered).
    pub ambiguous: u32,
    /// Total = safe_resume + safe_replay + ambiguous.
    pub total: u32,
}

/// Payload for [`EventPayload::WorkerHeartbeat`]. Identifies
/// which worker the heartbeat is for; the timestamp lives on
/// the envelope.
///
/// The payload is deliberately minimal. Future "what is this
/// worker up to" fields (in-flight invocation count, load,
/// version, host info) belong here when there's a consumer
/// that uses them — today the only consumer is the
/// coordination consumer's `last_heartbeat` update, which
/// reads only the `worker_id` and the envelope timestamp.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerHeartbeatPayload {
    pub worker_id: crate::worker::WorkerId,
}

/// Payload for [`EventPayload::WorkerOrphaned`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerOrphanedPayload {
    pub worker_id: crate::worker::WorkerId,
    pub last_heartbeat_ms: i64,
}

/// A log record a connected MCP server emitted (`notifications/message`),
/// forwarded to the event bus by the daemon's notification drain
/// (ADR-0020). Daemon-scoped: shared MCP servers are not tied to a
/// single agent or invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerLogPayload {
    /// The MCP server that emitted the record (its declared name).
    pub server: String,
    /// MCP log level name (`"debug"`..`"emergency"`).
    pub level: String,
    /// Optional logger / category tag from the server.
    pub logger: Option<String>,
    /// The structured log payload as the server sent it.
    pub data: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trip_triggered_event() {
        let invocation_id = Uuid::now_v7();
        let event = Event::new(
            AgentId::new("researcher").unwrap(),
            invocation_id,
            EventPayload::Triggered(TriggeredPayload {
                trigger_source: TriggerSource::Manual,
                trigger_subject: None,
                trigger_payload: json!({"topic": "rust async"}),
                config_snapshot: ConfigSnapshot {
                    name: "researcher".to_string(),
                    model: "claude-haiku".to_string(),
                    system_prompt: "You are a research agent.".to_string(),
                    tools: vec!["read".to_string(), "web_search".to_string()],
                    sandbox: SandboxSnapshot {
                        fs_read: vec!["/docs".to_string()],
                        fs_write: vec![],
                        network: vec![],
                        env: vec![],
                        exec_cwd: vec![],
                    },
                    budget: Some(0.50),
                    ..Default::default()
                },
            }),
        );

        assert_eq!(event.subject(), "fq.agent.researcher.triggered");
        assert_eq!(event.envelope.schema_version, SCHEMA_VERSION);
        assert_eq!(event.envelope.agent_id, "researcher");
        assert_eq!(event.envelope.trace_id, event.envelope.invocation_id);
        assert!(event.envelope.parent_event_id.is_none());
        assert_eq!(event.envelope.schema_id, "factor-q/triggered@1");

        let json = serde_json::to_string(&event).unwrap();
        let round_tripped: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(round_tripped.envelope.agent_id, event.envelope.agent_id);
        assert_eq!(
            round_tripped.envelope.invocation_id,
            event.envelope.invocation_id
        );
        match round_tripped.payload {
            EventPayload::Triggered(p) => {
                assert!(matches!(p.trigger_source, TriggerSource::Manual));
                assert_eq!(p.config_snapshot.name, "researcher");
            }
            _ => panic!("wrong payload type"),
        }
    }

    #[test]
    fn subjects_for_all_event_types() {
        let agent = "test-agent";
        assert_eq!(
            subjects::agent_triggered(agent),
            "fq.agent.test-agent.triggered"
        );
        assert_eq!(
            subjects::agent_llm_request(agent),
            "fq.agent.test-agent.llm.request"
        );
        assert_eq!(
            subjects::agent_llm_response(agent),
            "fq.agent.test-agent.llm.response"
        );
        assert_eq!(
            subjects::agent_tool_call(agent),
            "fq.agent.test-agent.tool.call"
        );
        assert_eq!(
            subjects::agent_tool_result(agent),
            "fq.agent.test-agent.tool.result"
        );
        assert_eq!(
            subjects::agent_completed(agent),
            "fq.agent.test-agent.completed"
        );
        assert_eq!(subjects::agent_failed(agent), "fq.agent.test-agent.failed");
    }

    #[test]
    fn invocation_archived_subject_is_agent_scoped() {
        // `InvocationArchived` rides on the same agent-scoped
        // namespace as `InvocationAmbiguous` so the coordination
        // consumer's `fq.agent.*.invocation.*` filter catches it.
        let agent_id = AgentId::new("researcher").unwrap();
        let invocation_id = Uuid::now_v7();
        let worker_id = crate::worker::WorkerId::new("worker-007").unwrap();
        let event = Event::new(
            agent_id,
            invocation_id,
            EventPayload::InvocationArchived(InvocationArchivedPayload {
                worker_id: worker_id.clone(),
                final_phase: "completed".to_string(),
                final_state_blob: vec![1, 2, 3],
                started_at_ms: 1_700_000_000_000,
                terminal_at_ms: 1_700_000_001_000,
            }),
        );
        assert_eq!(event.subject(), "fq.agent.researcher.invocation.archived");
        assert_eq!(event.envelope.schema_id, "factor-q/invocation_archived@1");
        assert_eq!(event.envelope.invocation_id, invocation_id);
        match &event.payload {
            EventPayload::InvocationArchived(p) => {
                assert_eq!(p.worker_id, worker_id);
                assert_eq!(p.final_phase, "completed");
                assert_eq!(p.final_state_blob, vec![1, 2, 3]);
            }
            other => panic!("wrong payload variant: {other:?}"),
        }
    }

    #[test]
    fn invocation_archive_acked_subject_is_worker_scoped() {
        // The ack rides on `fq.worker.{worker_id}.invocation.archive_acked`
        // so a worker can subscribe to its own acks with a
        // single filter. Coordination consumer's
        // `fq.agent.*.invocation.*` filter does not match.
        let agent_id = AgentId::new("researcher").unwrap();
        let invocation_id = Uuid::now_v7();
        let worker_id = crate::worker::WorkerId::new("worker-007").unwrap();
        let event = Event::new(
            agent_id,
            invocation_id,
            EventPayload::InvocationArchiveAcked(InvocationArchiveAckedPayload {
                worker_id: worker_id.clone(),
            }),
        );
        assert_eq!(
            event.subject(),
            "fq.worker.worker-007.invocation.archive_acked"
        );
        assert_eq!(
            event.envelope.schema_id,
            "factor-q/invocation_archive_acked@1"
        );
        // Envelope keeps the real invocation_id so the worker
        // can identify which row to delete.
        assert_eq!(event.envelope.invocation_id, invocation_id);
    }

    #[test]
    fn invocation_operator_recovered_subject_is_agent_scoped() {
        // Operator-issued; rides on the same agent-scoped
        // namespace as `InvocationArchived` so the coordination
        // consumer's `fq.agent.*.invocation.*` filter catches it.
        let agent_id = AgentId::new("researcher").unwrap();
        let invocation_id = Uuid::now_v7();
        let event = Event::new(
            agent_id,
            invocation_id,
            EventPayload::InvocationOperatorRecovered(InvocationOperatorRecoveredPayload {
                action: "drop".to_string(),
                final_phase: "failed".to_string(),
                reason: Some("stuck on flaky network call".to_string()),
            }),
        );
        assert_eq!(
            event.subject(),
            "fq.agent.researcher.invocation.operator_recovered"
        );
        assert_eq!(
            event.envelope.schema_id,
            "factor-q/invocation_operator_recovered@1"
        );
        assert_eq!(event.envelope.invocation_id, invocation_id);
        match &event.payload {
            EventPayload::InvocationOperatorRecovered(p) => {
                assert_eq!(p.action, "drop");
                assert_eq!(p.final_phase, "failed");
                assert_eq!(p.reason.as_deref(), Some("stuck on flaky network call"));
            }
            other => panic!("wrong payload variant: {other:?}"),
        }
    }

    #[test]
    fn invocation_operator_recovered_payload_omits_reason_when_none() {
        // `reason` is operator-supplied; absence should
        // serialise as missing rather than `null`.
        let event = Event::new(
            AgentId::new("r").unwrap(),
            Uuid::now_v7(),
            EventPayload::InvocationOperatorRecovered(InvocationOperatorRecoveredPayload {
                action: "drop".to_string(),
                final_phase: "failed".to_string(),
                reason: None,
            }),
        );
        let body = serde_json::to_value(&event.payload).unwrap();
        assert!(
            body.get("reason").is_none(),
            "reason should be omitted when None, got {body}"
        );
    }

    #[test]
    fn worker_heartbeat_subject_reads_from_payload_not_envelope() {
        // The subject for a WorkerHeartbeat is built from the
        // payload's `worker_id`, not from `envelope.agent_id`
        // (which is the system sentinel for runtime-tier events).
        // This is the design call made on 2026-05-16 — worker is
        // its own scope, parallel to agent.
        let runtime_id = Uuid::now_v7();
        let worker_id = crate::worker::WorkerId::new("worker-007").unwrap();
        let event = Event::system(
            runtime_id,
            EventPayload::WorkerHeartbeat(WorkerHeartbeatPayload {
                worker_id: worker_id.clone(),
            }),
        );
        assert_eq!(event.subject(), "fq.worker.worker-007.heartbeat");
        assert_eq!(event.envelope.schema_id, "factor-q/worker_heartbeat@1");
        // The envelope's agent_id remains the system sentinel —
        // worker events aren't tied to an agent. The payload is
        // where the worker_id lives.
        assert_eq!(event.envelope.agent_id.as_str(), "system");
        match &event.payload {
            EventPayload::WorkerHeartbeat(p) => assert_eq!(p.worker_id, worker_id),
            other => panic!("wrong payload variant: {other:?}"),
        }
    }

    #[test]
    fn validate_token_accepts_typical_ids() {
        for ok in [
            "agent",
            "agent_1",
            "agent-1",
            "a",
            "system",
            "worker-001",
            "01HXJABC0123456789", // ulid-shaped
        ] {
            assert!(
                subjects::validate_token(ok).is_ok(),
                "expected {ok:?} to be a valid subject token"
            );
        }
    }

    #[test]
    fn validate_token_rejects_dot_wildcard_whitespace_and_empty() {
        use subjects::SubjectTokenError;
        assert_eq!(
            subjects::validate_token(""),
            Err(SubjectTokenError::Empty),
            "empty token should be rejected"
        );
        for bad in ["foo.bar", "agent*", "agent>", "has space", "has\ttab"] {
            assert!(
                matches!(
                    subjects::validate_token(bad),
                    Err(SubjectTokenError::InvalidChar(_))
                ),
                "expected {bad:?} to be rejected as invalid"
            );
        }
    }

    #[test]
    fn tool_result_error_kind_serialises() {
        let payload = ToolResultPayload {
            tool_call_id: crate::events::ToolCallId::new("toolu_01ABC").unwrap(),
            output: "Path /etc/passwd is outside allowed scope".to_string(),
            is_error: true,
            error_kind: Some(ToolErrorKind::SandboxViolation),
            duration_ms: 1,
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["error_kind"], "sandbox_violation");
        assert_eq!(json["is_error"], true);
    }

    #[test]
    fn tool_result_success_omits_error_kind() {
        let payload = ToolResultPayload {
            tool_call_id: crate::events::ToolCallId::new("toolu_01ABC").unwrap(),
            output: "file contents".to_string(),
            is_error: false,
            error_kind: None,
            duration_ms: 12,
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json.get("error_kind"), None);
        assert_eq!(json["is_error"], false);
    }

    #[test]
    fn envelope_default_fields_on_new_event() {
        let invocation_id = Uuid::now_v7();
        let event = Event::new(
            AgentId::new("test-agent").unwrap(),
            invocation_id,
            EventPayload::LlmDispatched(LlmDispatchedPayload {
                call_id: Uuid::now_v7(),
                model: "claude-haiku".to_string(),
            }),
        );
        assert!(event.envelope.parent_event_id.is_none());
        assert_eq!(event.envelope.trace_id, invocation_id);
        assert_eq!(event.envelope.invocation_id, invocation_id);
        assert_eq!(event.envelope.agent_id, "test-agent");
        assert_eq!(event.envelope.schema_id, "factor-q/llm_dispatched@1");
        assert!(event.annotations.is_empty());
    }

    #[test]
    fn event_for_system_uses_runtime_id_as_trace_id() {
        let runtime_id = Uuid::now_v7();
        let event = Event::system(
            runtime_id,
            EventPayload::SystemStartup(SystemStartupPayload {
                runtime_id,
                version: "0.1.0".to_string(),
                nats_url: "nats://localhost:4222".to_string(),
                agents_loaded: 0,
                pricing_entries: 0,
            }),
        );
        assert_eq!(event.envelope.trace_id, runtime_id);
        assert_eq!(event.envelope.invocation_id, runtime_id);
        assert_eq!(event.envelope.agent_id, "system");
        assert!(event.envelope.parent_event_id.is_none());
    }

    #[test]
    fn annotations_skip_serialise_when_empty() {
        let invocation_id = Uuid::now_v7();
        let event = Event::new(
            AgentId::new("test-agent").unwrap(),
            invocation_id,
            EventPayload::Triggered(TriggeredPayload {
                trigger_source: TriggerSource::Manual,
                trigger_subject: None,
                trigger_payload: json!({}),
                config_snapshot: ConfigSnapshot {
                    name: "t".to_string(),
                    model: "m".to_string(),
                    system_prompt: String::new(),
                    tools: vec![],
                    sandbox: SandboxSnapshot::default(),
                    budget: None,
                    ..Default::default()
                },
            }),
        );
        let json = serde_json::to_value(&event).unwrap();
        assert!(json.get("annotations").is_none());
        assert!(json.get("envelope").is_some());
    }

    #[test]
    fn schema_version_constant_is_two() {
        assert_eq!(SCHEMA_VERSION, 2);
    }

    #[test]
    fn tool_call_id_round_trips_as_bare_string() {
        let id = ToolCallId::new("toolu_01ABC").unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"toolu_01ABC\"");
        let parsed: ToolCallId = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, id);
    }

    #[test]
    fn tool_call_id_rejects_empty_input() {
        assert!(ToolCallId::new("").is_err());
    }

    #[test]
    fn tool_call_id_deserialise_rejects_empty_string() {
        // Wire-boundary check: an event arriving with an empty
        // tool_call_id fails to parse rather than landing in the
        // runtime where downstream code assumes non-empty.
        let result: Result<ToolCallId, _> = serde_json::from_str("\"\"");
        assert!(result.is_err());
    }

    #[test]
    fn event_with_parent_sets_envelope_field() {
        let invocation_id = Uuid::now_v7();
        let event = Event::new(
            AgentId::new("agent").unwrap(),
            invocation_id,
            EventPayload::LlmDispatched(LlmDispatchedPayload {
                call_id: Uuid::now_v7(),
                model: "m".to_string(),
            }),
        );
        let parent = Uuid::now_v7();
        let event = event.with_parent(parent);
        assert_eq!(event.envelope.parent_event_id, Some(parent));
    }

    #[test]
    fn system_events_have_null_parent() {
        // Resolved decision from step 2 of the envelope-refactor
        // plan: SystemStartup, SystemRecovery, SystemShutdown,
        // SystemTaskFailed are not part of any invocation chain.
        let runtime_id = Uuid::now_v7();
        let cases = vec![
            EventPayload::SystemStartup(SystemStartupPayload {
                runtime_id,
                version: String::new(),
                nats_url: String::new(),
                agents_loaded: 0,
                pricing_entries: 0,
            }),
            EventPayload::SystemShutdown(SystemShutdownPayload {
                runtime_id,
                reason: String::new(),
                clean: true,
            }),
            EventPayload::SystemTaskFailed(SystemTaskFailedPayload {
                runtime_id,
                task_name: String::new(),
                error_message: String::new(),
            }),
            EventPayload::SystemRecovery(SystemRecoveryPayload {
                runtime_id,
                worker_id: String::new(),
                safe_resume: 0,
                safe_replay: 0,
                ambiguous: 0,
                total: 0,
            }),
        ];
        for p in cases {
            let event = Event::system(runtime_id, p);
            assert!(
                event.envelope.parent_event_id.is_none(),
                "system events must not chain to a parent: schema_id={}",
                event.envelope.schema_id
            );
        }
    }

    #[test]
    fn event_with_cost_sets_envelope_cost() {
        let invocation_id = Uuid::now_v7();
        let event = Event::new(
            AgentId::new("agent").unwrap(),
            invocation_id,
            EventPayload::LlmResponse(LlmResponsePayload {
                origin: LlmCallOrigin::AgentTurn,
                call_id: Uuid::now_v7(),
                content: None,
                tool_calls: vec![],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            }),
        );
        let cost = CostMetadata {
            call_id: Uuid::now_v7(),
            model: "claude-haiku".to_string(),
            input_tokens: 100,
            output_tokens: 50,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            input_cost: 0.0001,
            output_cost: 0.0005,
            total_cost: 0.0006,
            cumulative_invocation_cost: 0.0006,
            cumulative_agent_cost: 0.0006,
            origin: LlmCallOrigin::AgentTurn,
        };
        let event = event.with_cost(cost.clone());
        assert_eq!(event.envelope.cost.as_ref(), Some(&cost));
    }

    #[test]
    fn cost_metadata_round_trips_on_envelope() {
        let invocation_id = Uuid::now_v7();
        let cost = CostMetadata {
            call_id: Uuid::now_v7(),
            model: "m".to_string(),
            input_tokens: 1,
            output_tokens: 2,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            input_cost: 0.1,
            output_cost: 0.2,
            total_cost: 0.3,
            cumulative_invocation_cost: 0.3,
            cumulative_agent_cost: 0.3,
            origin: LlmCallOrigin::AgentTurn,
        };
        let event = Event::new(
            AgentId::new("agent").unwrap(),
            invocation_id,
            EventPayload::LlmResponse(LlmResponsePayload {
                origin: LlmCallOrigin::AgentTurn,
                call_id: Uuid::now_v7(),
                content: Some("ok".to_string()),
                tool_calls: vec![],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            }),
        )
        .with_cost(cost.clone());
        let json = serde_json::to_string(&event).unwrap();
        let parsed: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.envelope.cost.as_ref(), Some(&cost));
    }

    #[test]
    fn envelope_cost_omits_when_none() {
        let invocation_id = Uuid::now_v7();
        let event = Event::new(
            AgentId::new("agent").unwrap(),
            invocation_id,
            EventPayload::LlmResponse(LlmResponsePayload {
                origin: LlmCallOrigin::AgentTurn,
                call_id: Uuid::now_v7(),
                content: None,
                tool_calls: vec![],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            }),
        );
        let json = serde_json::to_value(&event).unwrap();
        let envelope = json.get("envelope").expect("envelope present");
        assert!(envelope.get("cost").is_none());
    }

    #[test]
    fn event_annotate_inserts_key() {
        let invocation_id = Uuid::now_v7();
        let event = Event::new(
            AgentId::new("agent").unwrap(),
            invocation_id,
            EventPayload::Triggered(TriggeredPayload {
                trigger_source: TriggerSource::Manual,
                trigger_subject: None,
                trigger_payload: json!({}),
                config_snapshot: ConfigSnapshot {
                    name: "t".to_string(),
                    model: "m".to_string(),
                    system_prompt: String::new(),
                    tools: vec![],
                    sandbox: SandboxSnapshot::default(),
                    budget: None,
                    ..Default::default()
                },
            }),
        )
        .annotate(annotation_keys::NOTES, json!("hello"))
        .annotate(annotation_keys::CONFIDENCE, json!(0.7));
        assert_eq!(
            event.annotations.0.get(annotation_keys::NOTES),
            Some(&json!("hello"))
        );
        assert_eq!(
            event.annotations.0.get(annotation_keys::CONFIDENCE),
            Some(&json!(0.7))
        );
    }

    #[test]
    fn event_annotate_replaces_existing_key() {
        let invocation_id = Uuid::now_v7();
        let event = Event::new(
            AgentId::new("agent").unwrap(),
            invocation_id,
            EventPayload::LlmDispatched(LlmDispatchedPayload {
                call_id: Uuid::now_v7(),
                model: "m".to_string(),
            }),
        )
        .annotate(annotation_keys::NOTES, json!("first"))
        .annotate(annotation_keys::NOTES, json!("second"));
        assert_eq!(
            event.annotations.0.get(annotation_keys::NOTES),
            Some(&json!("second"))
        );
        assert_eq!(event.annotations.0.len(), 1);
    }

    #[test]
    fn unknown_annotation_keys_permitted() {
        // The registry is advisory; arbitrary keys are still legal.
        let invocation_id = Uuid::now_v7();
        let event = Event::new(
            AgentId::new("agent").unwrap(),
            invocation_id,
            EventPayload::LlmDispatched(LlmDispatchedPayload {
                call_id: Uuid::now_v7(),
                model: "m".to_string(),
            }),
        )
        .annotate("my_custom_key", json!({"shape": "blob"}));
        assert!(event.annotations.0.contains_key("my_custom_key"));
    }

    #[test]
    fn well_known_annotation_keys_are_constants() {
        assert_eq!(annotation_keys::NOTES, "notes");
        assert_eq!(annotation_keys::CONFIDENCE, "confidence");
        assert_eq!(annotation_keys::REASONING, "reasoning");
        assert_eq!(annotation_keys::SOURCES_CONSIDERED, "sources_considered");
        assert_eq!(annotation_keys::FLAGS, "flags");
    }

    #[test]
    fn consumer_view_strips_annotations_round_trip() {
        // Step 4 acceptance test: an event with payload + two
        // annotations serialises via for_consumer_context with
        // envelope and payload but no annotations field.
        let invocation_id = Uuid::now_v7();
        let event = Event::new(
            AgentId::new("agent").unwrap(),
            invocation_id,
            EventPayload::LlmResponse(LlmResponsePayload {
                origin: LlmCallOrigin::AgentTurn,
                call_id: Uuid::now_v7(),
                content: Some("hello".to_string()),
                tool_calls: vec![],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            }),
        )
        .annotate(annotation_keys::NOTES, json!("thinking aloud"))
        .annotate(annotation_keys::CONFIDENCE, json!(0.9));

        let view = event.for_consumer_context();
        let json = serde_json::to_value(&view).unwrap();
        assert!(json.get("envelope").is_some(), "envelope present");
        assert!(json.get("payload").is_some(), "payload present");
        assert!(
            json.get("annotations").is_none(),
            "annotations must be stripped from consumer view"
        );
        // Original event still has the annotations — the barrier is
        // a serialisation property of the view, not a mutation of
        // the source.
        assert_eq!(event.annotations.0.len(), 2);
    }

    #[test]
    fn consumer_view_serialises_without_annotations_field_even_with_annotations() {
        // Same property as above, but with the most common attack
        // path: a producer trying to smuggle a reasoning trace
        // through the consumer barrier.
        let invocation_id = Uuid::now_v7();
        let event = Event::new(
            AgentId::new("producer").unwrap(),
            invocation_id,
            EventPayload::LlmResponse(LlmResponsePayload {
                origin: LlmCallOrigin::AgentTurn,
                call_id: Uuid::now_v7(),
                content: Some("answer: 42".to_string()),
                tool_calls: vec![],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            }),
        )
        .annotate(
            annotation_keys::REASONING,
            json!("I tried 41, then 42, and decided 42"),
        );

        let view = event.for_consumer_context();
        let serialised = serde_json::to_string(&view).unwrap();
        assert!(
            !serialised.contains("reasoning"),
            "reasoning trace must not leak through consumer view"
        );
        assert!(
            !serialised.contains("I tried 41"),
            "annotation value must not leak through consumer view"
        );
    }

    #[test]
    fn event_with_parent_round_trips_through_serde() {
        let invocation_id = Uuid::now_v7();
        let parent = Uuid::now_v7();
        let event = Event::new(
            AgentId::new("agent").unwrap(),
            invocation_id,
            EventPayload::ToolDispatched(ToolDispatchedPayload {
                tool_call_id: crate::events::ToolCallId::new("tc").unwrap(),
                tool_name: "t".to_string(),
            }),
        )
        .with_parent(parent);
        let json = serde_json::to_string(&event).unwrap();
        let parsed: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.envelope.parent_event_id, Some(parent));
    }

    #[test]
    fn schema_id_for_every_payload_variant() {
        // Exhaustive check that every payload variant resolves to a
        // non-empty `factor-q/<name>@<v>` schema_id. The match in
        // `schema_id_for` is exhaustive, so adding a new payload
        // variant without a schema_id mapping will fail to compile.
        let inv = Uuid::now_v7();
        let cases: Vec<EventPayload> = vec![
            EventPayload::Triggered(TriggeredPayload {
                trigger_source: TriggerSource::Manual,
                trigger_subject: None,
                trigger_payload: json!({}),
                config_snapshot: ConfigSnapshot {
                    name: "t".into(),
                    model: "m".into(),
                    system_prompt: String::new(),
                    tools: vec![],
                    sandbox: SandboxSnapshot::default(),
                    budget: None,
                    ..Default::default()
                },
            }),
            EventPayload::LlmRequest(LlmRequestPayload {
                origin: LlmCallOrigin::AgentTurn,
                call_id: inv,
                model: "m".into(),
                messages: vec![],
                tools_available: vec![],
                request_params: RequestParams {
                    effort: None,
                    temperature: None,
                    max_tokens: None,
                },
            }),
            EventPayload::LlmDispatched(LlmDispatchedPayload {
                call_id: inv,
                model: "m".into(),
            }),
            EventPayload::LlmResponse(LlmResponsePayload {
                origin: LlmCallOrigin::AgentTurn,
                call_id: inv,
                content: None,
                tool_calls: vec![],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            }),
            EventPayload::ToolCall(ToolCallPayload {
                tool_call_id: crate::events::ToolCallId::new("tc").unwrap(),
                tool_name: "n".into(),
                parameters: json!({}),
            }),
            EventPayload::ToolDispatched(ToolDispatchedPayload {
                tool_call_id: crate::events::ToolCallId::new("tc").unwrap(),
                tool_name: "n".into(),
            }),
            EventPayload::ToolResult(ToolResultPayload {
                tool_call_id: crate::events::ToolCallId::new("tc").unwrap(),
                output: String::new(),
                is_error: false,
                error_kind: None,
                duration_ms: 0,
            }),
            EventPayload::Completed(CompletedPayload {
                result_summary: None,
                total_llm_calls: 0,
                total_tool_calls: 0,
                total_cost: 0.0,
                total_duration_ms: 0,
            }),
            EventPayload::Failed(FailedPayload {
                error_kind: FailureKind::RuntimeError,
                error_message: String::new(),
                phase: FailurePhase::Setup,
                partial_totals: InvocationTotals::default(),
            }),
            EventPayload::InvocationAmbiguous(InvocationAmbiguousPayload {
                stuck_entity: "tool_dispatch".into(),
                stuck_call_id: "tc".into(),
                note: String::new(),
            }),
            EventPayload::SystemStartup(SystemStartupPayload {
                runtime_id: inv,
                version: String::new(),
                nats_url: String::new(),
                agents_loaded: 0,
                pricing_entries: 0,
            }),
            EventPayload::SystemShutdown(SystemShutdownPayload {
                runtime_id: inv,
                reason: String::new(),
                clean: true,
            }),
            EventPayload::SystemTaskFailed(SystemTaskFailedPayload {
                runtime_id: inv,
                task_name: String::new(),
                error_message: String::new(),
            }),
            EventPayload::SystemRecovery(SystemRecoveryPayload {
                runtime_id: inv,
                worker_id: String::new(),
                safe_resume: 0,
                safe_replay: 0,
                ambiguous: 0,
                total: 0,
            }),
            EventPayload::WorkerHeartbeat(WorkerHeartbeatPayload {
                worker_id: crate::worker::WorkerId::new("w").unwrap(),
            }),
            EventPayload::McpServerLog(McpServerLogPayload {
                server: String::new(),
                level: String::new(),
                logger: None,
                data: serde_json::Value::Null,
            }),
        ];
        for payload in cases {
            let id = schema_id_for(&payload);
            assert!(
                id.starts_with("factor-q/") && id.ends_with("@1"),
                "schema_id_for produced {id:?}"
            );
        }
    }
}
