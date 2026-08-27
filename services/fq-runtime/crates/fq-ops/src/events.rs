//! Event schema for factor-q.
//!
//! Every event on the bus has three structurally distinct layers:
//!
//! - [`Envelope`] — runtime-written system metadata. Closed schema.
//! - [`EventPayload`] — typed contract between graph nodes. The only
//!   thing that drives downstream agent behaviour.
//! - [`Annotations`] — open key/value commentary, written by the
//!   runtime; agents have no way to annotate (see [`annotation_keys`]).
//!   **Never** read by consuming agents: [`Event::for_consumer_context`]
//!   is the only sanctioned way into a downstream prompt.
//!
//! Each layer has different write permissions, read audiences, and
//! mutability rules; see
//! `docs/design/aspirational/inter-node-contracts-and-event-layers.md` for the
//! rationale.
//!
//! **The vocabulary, not its production.** These are the shapes an
//! event has on the wire, so anything that only *reads* events — the
//! thin client's tail, a renderer — links this crate and none of the
//! runtime. Deciding that an event has happened, and publishing it,
//! needs the reducer, the stores and the bus, and stays in
//! `fq-runtime`; `fq_runtime::events` re-exports everything here so
//! the daemon reaches it by the same path as before.

use std::collections::BTreeMap;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::agent::AgentId;

pub const SCHEMA_VERSION: u32 = 3;
/// Well-known annotation keys — a **reserved vocabulary, not an
/// agent-facing channel** (#90). Nothing lets an agent annotate; every
/// writer is host code and four of the five have no writer at all. The
/// agent path is consumer-driven, waiting on a reader — see
/// `docs/design/committed/event-schema.md`. Unknown keys still permitted.
///
/// Per §6 of `inter-node-contracts-and-event-layers.md`, every key
/// here is **advisory** — annotations are never read by consuming
/// agents. The runtime strips them at the consumer-context boundary
/// via [`Event::for_consumer_context`]; prompts see envelope +
/// payload only.
pub mod annotation_keys {
    /// Free-form commentary about the event. Reserved — no writer.
    pub const NOTES: &str = "notes";
    /// Self-reported confidence; calibrated confidence comes from a
    /// verifier node, not the producer. Reserved — no writer.
    pub const CONFIDENCE: &str = "confidence";
    /// Chain-of-thought / working. Reserved — no writer, and opening
    /// one is a retention decision, not plumbing: annotations ride
    /// every event and the log is kept.
    pub const REASONING: &str = "reasoning";
    /// Sources looked at but not used in the payload; ones actually
    /// used belong in a typed `Citation[]`. Reserved — no writer.
    pub const SOURCES_CONSIDERED: &str = "sources_considered";
    /// Markers for downstream humans (or a meta-agent). The only key
    /// with a writer, and it is the runtime: the context-pressure
    /// warning, which emits an object where the schema says array of
    /// strings (#90).
    pub const FLAGS: &str = "flags";
}

// Documented by its own `//!` header. Deliberately no outer doc here:
// rustdoc merges an outer comment with the module's inner ones and then
// resolves the whole block in *this* scope, so an intra-doc link to one
// of `subjects`' own items would not resolve.
pub mod subjects;

/// The envelope layer: [`Envelope`] and [`CostMetadata`], re-exported
/// below so `crate::events` stays the import path.
mod envelope;

/// The LLM-call payload cluster. Re-exported below, so `crate::events`
/// stays the import path for every one of these types.
pub mod llm;

/// The per-event payload structs. Re-exported below, so
/// `crate::events` stays the import path for every one of them.
mod payloads;

/// Which event types are transient — operational signal the operator
/// surface does not serve, written down once.
pub mod transient;

pub use envelope::{CostMetadata, Envelope};
pub use llm::{
    AssistantPart, Effort, LlmCallOrigin, LlmDispatchedPayload, LlmErrorKind, LlmFailurePayload,
    LlmRequestPayload, LlmResponsePayload, Message, MessageToolCall, Reasoning, ReasoningContent,
    RequestParams, StopReason, TokenUsage, ToolResult, ToolSchema, assistant_parts, assistant_text,
    assistant_tool_calls,
};
pub use payloads::*;

/// A complete event: envelope + payload + annotations.
///
/// The three layers are kept as separate fields rather than
/// flattened so the trust/visibility boundary between them is
/// expressed in the type system. Producing agents do not touch the
/// envelope, and consuming agents never see annotations — enforced
/// by [`Event::for_consumer_context`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub envelope: Envelope,
    #[serde(deserialize_with = "payload_or_unknown")]
    pub payload: EventPayload,
    #[serde(default, skip_serializing_if = "Annotations::is_empty")]
    pub annotations: Annotations,
}

/// Read a payload, degrading an `event_type` this binary has never
/// heard of to [`EventPayload::Unknown`] rather than failing the whole
/// event (see that variant for why).
///
/// `#[serde(other)]` alone is not enough: it catches the unknown *tag*,
/// but serde then tries to read the adjacent `payload` body into a unit
/// variant and refuses. So the parse is attempted twice — once whole,
/// and if that fails, once with the body dropped. The second attempt
/// succeeds only when `other` fires, i.e. exactly when the tag is
/// unknown; a *known* tag with a malformed body fails both times and
/// keeps its original error, which is what it should be.
fn payload_or_unknown<'de, D>(deserializer: D) -> Result<EventPayload, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    let err = match EventPayload::deserialize(&value) {
        Ok(payload) => return Ok(payload),
        Err(err) => err,
    };
    let tag = value.get("event_type").cloned().unwrap_or(Value::Null);
    EventPayload::deserialize(&serde_json::json!({ "event_type": tag }))
        .map_err(|_| serde::de::Error::custom(err))
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
            Self::LlmFailure(_) => subjects::agent_llm_failure(agent),
            Self::ToolCall(_) => subjects::agent_tool_call(agent),
            Self::ToolDispatched(_) => subjects::agent_tool_dispatched(agent),
            Self::ToolResult(_) => subjects::agent_tool_result(agent),
            Self::LlmDispatched(_) => subjects::agent_llm_dispatched(agent),
            Self::InvocationAmbiguous(_) => subjects::agent_invocation_ambiguous(agent),
            Self::InvocationArchived(_) => subjects::agent_invocation_archived(agent),
            Self::InvocationOperatorRecovered(_) => {
                subjects::agent_invocation_operator_recovered(agent)
            }
            Self::InvocationOperatorResumed(_) => {
                subjects::agent_invocation_operator_resumed(agent)
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
            Self::Unknown => subjects::SYSTEM_UNKNOWN.to_string(),
        }
    }

    pub fn schema_id(&self) -> &'static str {
        match self {
            Self::Triggered(_) => "factor-q/triggered@1",
            Self::LlmRequest(_) => "factor-q/llm_request@1",
            Self::LlmDispatched(_) => "factor-q/llm_dispatched@1",
            Self::LlmResponse(_) => "factor-q/llm_response@1",
            Self::LlmFailure(_) => "factor-q/llm_failure@1",
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
            Self::InvocationOperatorResumed(_) => "factor-q/invocation_operator_resumed@1",
            Self::SystemStartup(_) => "factor-q/system_startup@1",
            Self::SystemShutdown(_) => "factor-q/system_shutdown@1",
            Self::SystemTaskFailed(_) => "factor-q/system_task_failed@1",
            Self::SystemRecovery(_) => "factor-q/system_recovery@1",
            Self::WorkerHeartbeat(_) => "factor-q/worker_heartbeat@1",
            Self::WorkerOrphaned(_) => "factor-q/worker_orphaned@1",
            Self::McpServerLog(_) => "factor-q/mcp_server_log@1",
            Self::Unknown => "factor-q/unknown@1",
        }
    }

    pub fn event_type(&self) -> &'static str {
        self.schema_id()
            .strip_prefix("factor-q/")
            .unwrap()
            .strip_suffix("@1")
            .unwrap()
    }

    /// Whether this event is **transient** — operational signal the
    /// operator surface does not serve. A property of the type, not
    /// of the instance: see [`transient`] for what that means and why
    /// the set lives in one place.
    pub fn is_transient(&self) -> bool {
        transient::includes(self.event_type())
    }
}

/// Stable identifier for an event's payload schema. Versioned from day one.
pub fn schema_id_for(payload: &EventPayload) -> &'static str {
    payload.schema_id()
}

/// Open key/value commentary, written by the runtime. Nothing lets a
/// producing agent attach one (#90) — every writer is host code.
///
/// The three pieces that make that hold are already here: the
/// well-known keys in [`annotation_keys`], the [`Event::annotate`]
/// builder, and [`Event::for_consumer_context`], the barrier that
/// strips annotations before an event reaches a downstream agent's
/// prompt. Unknown keys are still permitted.
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
    /// The other terminal outcome of an LLM call (#447): the provider
    /// errored, or returned nothing. Sibling of `LlmResponse` rather
    /// than a nullable variant of it — see [`LlmFailurePayload`].
    /// Publishing it is what makes "the call failed" distinguishable
    /// from "the event was lost", which are the two cases an operator
    /// most needs told apart.
    LlmFailure(LlmFailurePayload),
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
    /// via `fq invocation resume`/`drop` (step 9).
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

    /// Operator-triggered interrupted-result injection for an ambiguous invocation.
    InvocationOperatorResumed(InvocationOperatorResumedPayload),

    // Runtime lifecycle
    SystemStartup(SystemStartupPayload),
    SystemShutdown(SystemShutdownPayload),
    SystemTaskFailed(SystemTaskFailedPayload),

    /// Emitted once per daemon startup with the counts of
    /// in-flight invocations classified by recovery category
    /// (data-architecture.md §7.1). The projection records
    /// these so operators can see recovery history via
    /// `fq events query --event-type=system_recovery` without
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

    /// An `event_type` this binary has never heard of — a payload
    /// minted by a newer daemon and read by an older one.
    ///
    /// Without this variant an unknown tag is a hard deserialisation
    /// error, and every consumer handles that differently: the durable
    /// consumers warn and ack (dropping the event permanently), `fq
    /// events tail` propagates the error and terminates the stream,
    /// and the advisory watch skips silently. Adding a new event type
    /// is therefore a backwards-breaking change for the whole
    /// mixed-version window. With it, an old binary reads the envelope
    /// — agent, invocation, timestamp, chain, cost — and simply has no
    /// typed payload, which is what every one of those consumers
    /// actually wants.
    ///
    /// The runtime never constructs or publishes this variant; it
    /// exists only as a deserialisation landing pad, and the payload
    /// body is discarded on the way in.
    #[serde(other)]
    Unknown,
}

/// One recorded event, with the log position it was read at.
///
/// The Event atom's state is the event itself, unabridged. It is the
/// substrate every other resource folds from, so an atom that dropped
/// the payload would not be the fact; the projection's `events` table
/// is an *index* over these, not the atom. The event's **identity**
/// is `event.envelope.event_id`, which is what `event.get` takes.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EventState {
    /// Where in the log this event sits — the universal cursor (P5):
    /// the same number that cursors `event.stream`, feeds `min_seq`
    /// gates, and appears in a command receipt's `watermarks`.
    ///
    /// It does *not* appear in the receipt's `AtomRef`, which names
    /// the event by `event_id`. A receipt separates the two on
    /// purpose: what was written is addressed by identity, how far the
    /// log got is addressed by position.
    ///
    /// A cursor, never an identity. It says where the read landed,
    /// and it is only meaningful against the log that produced it:
    /// recreate the stream and the number means something else. Ask
    /// for an event by `event_id`; use this to resume.
    pub seq: u64,
    /// The event exactly as published.
    ///
    /// Declared to the surface as an opaque object rather than a
    /// reflected schema: an event already names its own payload
    /// contract in `envelope.schema_id` (`factor-q/llm_response@1`),
    /// which is the versioned reference a reader resolves, and
    /// reflecting the whole payload tree here would need schemars'
    /// chrono and uuid integrations — a wider change than this atom.
    #[schemars(with = "serde_json::Value")]
    pub event: Event,
}

#[cfg(test)]
mod tests;
