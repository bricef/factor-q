# Event Schema

This document specifies the event schema emitted by the factor-q runtime. It covers the envelope, per-event-type payloads, subject hierarchy, and the design rationale for each choice.

Events are the primary observability and audit surface of factor-q. Every meaningful action in the system is an event, published to NATS JetStream, and later projected into SQLite for querying.

## Envelope

Every event carries the same envelope fields before the type-specific payload:

| Field | Type | Purpose |
|---|---|---|
| `schema_version` | `u32` | Monotonic version of the event schema. Incremented on breaking changes. |
| `event_id` | `string` (UUID v7) | Globally unique event identifier. UUID v7 gives time-ordered IDs. |
| `timestamp` | `string` (RFC3339 with nanoseconds) | When the event was generated. |
| `agent_id` | `string` | Which agent this event belongs to. Matches the `name` field in the agent definition. |
| `invocation_id` | `string` (UUID v7) | Groups events from a single agent invocation. All events within one run of an agent share this ID. |
| `event_type` | enum | Discriminator for the payload shape. |
| `payload` | type-specific | See below. |

### Rationale

- **`invocation_id` is load-bearing.** An agent can be triggered many times; reconstructing the trace of a single run requires a stable ID. This is the primary key for grouping in projections and CLI queries.
- **`schema_version` from day one.** Even if we never break the schema, declaring a version makes migrations possible without inventing a mechanism later.
- **UUID v7 for IDs.** Time-ordered UUIDs mean events sort naturally and stream in a sensible order without needing a separate timestamp index.
- **JSON encoding, not binary.** Human-readable, debuggable with `nats sub "fq.>"` during development. Throughput optimisation can come later if it proves necessary.

## Subject Hierarchy

Events are published to NATS subjects following this pattern:

```
fq.agent.{agent_id}.{event_type}[.{sub_type}]
fq.system.{event_type}
```

Concrete subjects:

| Subject | Event |
|---|---|
| `fq.agent.{agent_id}.triggered` | An invocation has started |
| `fq.agent.{agent_id}.llm.request` | LLM call about to be made |
| `fq.agent.{agent_id}.llm.response` | LLM call has returned |
| `fq.agent.{agent_id}.tool.call` | Agent is invoking a tool |
| `fq.agent.{agent_id}.tool.result` | Tool invocation has completed (success or failure) |
| `fq.agent.{agent_id}.cost` | Cost update from an LLM call |
| `fq.agent.{agent_id}.completed` | Invocation has finished successfully |
| `fq.agent.{agent_id}.failed` | Invocation has terminated with an error |
| `fq.system.startup` | Runtime lifecycle — startup |
| `fq.system.shutdown` | Runtime lifecycle — shutdown |

### Rationale

- **Agent ID in the subject, not just the payload.** This enables subject-based filtering — a consumer can subscribe to `fq.agent.researcher.>` to only see events from the researcher agent, without filtering in application code.
- **Hierarchical types** (`llm.request` vs `llm.response`). Allows wildcards: `fq.agent.*.llm.>` matches all LLM events across all agents.
- **System events are a separate namespace.** Runtime lifecycle is not tied to any agent.

## Event Types

### `triggered`

Published when an agent invocation begins. Carries a snapshot of the agent's configuration so the event log is self-contained for replay even if the agent definition is later modified.

```json
{
  "trigger_source": "manual | subject | schedule",
  "trigger_subject": "tasks.research.topic-x",
  "trigger_payload": { "...arbitrary input data..." },
  "config_snapshot": {
    "name": "researcher",
    "model": "claude-haiku",
    "system_prompt": "You are a research agent...",
    "tools": ["read", "web_search"],
    "sandbox": {
      "fs_read": ["/project/docs"],
      "network": ["*.api.internal"]
    },
    "budget": 0.50
  }
}
```

**Design notes:**
- **`config_snapshot` is a full capture.** This is what makes replay meaningful — if the agent definition is later changed, the trace still shows exactly what was running. It is the source of truth for "what did this invocation actually do."
- **`trigger_source` indicates who initiated.** For phase 1, only `manual` and `subject` matter.
- **`trigger_payload` is opaque.** Any JSON value, defined by the trigger source.

### `llm.request`

Published immediately before an LLM call is made.

```json
{
  "call_id": "llm-01HXJ...",
  "model": "claude-haiku",
  "messages": [
    { "role": "system", "content": "..." },
    { "role": "user", "content": "..." },
    { "role": "assistant", "content": "...", "tool_calls": [...] },
    { "role": "tool", "tool_call_id": "...", "content": "..." }
  ],
  "tools_available": [
    { "name": "read", "description": "...", "parameters_schema": {...} }
  ],
  "request_params": {
    "temperature": 0.7,
    "max_tokens": 4096
  }
}
```

**Design notes:**
- **Full message history is sent every time.** Reconstructing context from earlier events would be fragile; carrying the full history makes each `llm.request` self-contained. The cost is larger events, accepted for correctness and replay simplicity.
- **`tools_available` is a snapshot.** Tool schemas can change between calls (rare in phase 1, possible later with dynamic skill activation). Capturing the snapshot per call preserves replay fidelity.
- **`call_id` correlates with the response.** Matches the `call_id` in the corresponding `llm.response` event.

### `llm.response`

Published when an LLM call returns. Includes token usage, which drives the subsequent `cost` event.

```json
{
  "call_id": "llm-01HXJ...",
  "content": "I will research the topic by first...",
  "tool_calls": [
    {
      "tool_call_id": "tool-01HXJ...",
      "tool_name": "read",
      "parameters": { "path": "/project/docs/overview.md" }
    }
  ],
  "stop_reason": "tool_use | end_turn | max_tokens | stop_sequence",
  "usage": {
    "input_tokens": 1234,
    "output_tokens": 567,
    "cache_read_tokens": 0,
    "cache_write_tokens": 0
  }
}
```

**Design notes:**
- **`tool_call_id` is assigned by the LLM** (or normalised from its response) and is referenced by subsequent `tool.call` and `tool.result` events.
- **`usage` carries raw token counts.** The `cost` event computes monetary cost from these and the provider's pricing.
- **Cache token fields are present from the start.** Some providers (Anthropic) return these; including them now avoids schema migrations later.

### `tool.call`

Published when the agent invokes a tool. Each tool call in a single LLM response produces its own `tool.call` event.

```json
{
  "tool_call_id": "tool-01HXJ...",
  "tool_name": "read",
  "parameters": { "path": "/project/docs/overview.md" }
}
```

**Design notes:**
- **`tool_call_id` correlates with the `tool.result`.** Needed for parallel tool calls even though phase 1 only uses sequential calls. Cheap to include from the start.

### `tool.result`

Published when a tool invocation completes — successfully or with an error. Sandbox violations are reported here with `is_error: true`, not as a separate event.

```json
{
  "tool_call_id": "tool-01HXJ...",
  "output": "# Overview\n\nThis project...",
  "is_error": false,
  "duration_ms": 12
}
```

Error case:

```json
{
  "tool_call_id": "tool-01HXJ...",
  "output": "Path /etc/passwd is outside the agent's allowed filesystem scope",
  "is_error": true,
  "error_kind": "sandbox_violation",
  "duration_ms": 1
}
```

**Design notes:**
- **One shape for success and failure.** Sandbox violations, execution errors, and invalid parameters all become `tool.result` events with `is_error: true`. This keeps the LLM's view consistent — a failed tool is just a tool that returned an error message — and simplifies downstream consumers.
- **`error_kind` is enumerated when `is_error` is true.** Values: `sandbox_violation`, `invalid_parameters`, `execution_failed`, `timeout`, `permission_denied`.

### `cost`

Published after each `llm.response` event. Tracks cost attribution at multiple levels.

```json
{
  "call_id": "llm-01HXJ...",
  "model": "claude-haiku",
  "input_tokens": 1234,
  "output_tokens": 567,
  "cache_read_tokens": 0,
  "cache_write_tokens": 0,
  "input_cost": 0.000308,
  "output_cost": 0.000710,
  "total_cost": 0.001018,
  "cumulative_invocation_cost": 0.004523,
  "cumulative_agent_cost": 0.127890
}
```

**Design notes:**
- **Cost is separate from `llm.response`** even though it's derived from it. This means cost enforcement, queries, and budget alerts can subscribe to `fq.agent.*.cost` without processing every LLM event. It also means the cost calculation is an explicit, auditable step.
- **Cumulative costs are included.** Avoids requiring downstream consumers to aggregate across events. The cumulative values are computed by the executor using its running state.
- **All currency amounts are USD.** Phase 1 assumes a single currency; multi-currency is deferred.

### `completed`

Published when an agent invocation finishes successfully (no errors, budget not exceeded).

```json
{
  "result_summary": "Completed research task and wrote findings to /project/output/report.md",
  "total_llm_calls": 4,
  "total_tool_calls": 7,
  "total_cost": 0.004523,
  "total_duration_ms": 12345
}
```

**Design notes:**
- **`result_summary` is optional and freeform.** It may be produced by the agent itself as its final output, or omitted.
- **Totals are denormalised.** Querying "how many tool calls did this invocation make" without aggregating across events is common enough to justify the denormalisation.

### `failed`

Published when an invocation terminates with an error.

```json
{
  "error_kind": "budget_exceeded | llm_error | tool_error | sandbox_violation | runtime_error",
  "error_message": "Agent budget of $0.50 exceeded after 5 LLM calls",
  "phase": "llm_request | llm_response | tool_call | tool_result | setup",
  "partial_totals": {
    "total_llm_calls": 5,
    "total_tool_calls": 3,
    "total_cost": 0.512,
    "total_duration_ms": 8234
  }
}
```

**Design notes:**
- **`phase` indicates where in the loop the failure occurred.** Useful for debugging and metrics — distinguishing "LLM failed to respond" from "tool execution failed" matters operationally.
- **`partial_totals` captures what got done before the failure.** Same denormalisation rationale as `completed`.

### `system.startup` and `system.shutdown`

Runtime lifecycle events, published when the factor-q process starts and stops.

```json
{
  "version": "0.1.0",
  "config_hash": "sha256:..."
}
```

**Design notes:**
- **`config_hash` captures the runtime's configuration.** Useful for correlating events with a specific configuration state — if behaviour changes, a new hash indicates config was reloaded.
- **Only startup and shutdown for now.** Config reload events can be added later if hot-reload is implemented.

## Invariants

The following invariants hold across the event stream and are assumed by consumers:

1. **Events within one `invocation_id` are totally ordered** by `timestamp` and `event_id` (UUID v7 is time-sortable).
2. **Every invocation starts with a `triggered` event** and ends with either `completed` or `failed`. No orphan invocations.
3. **Every `llm.request` is followed by exactly one `llm.response` or one `failed`.** No dangling requests.
4. **Every `tool.call` is followed by exactly one `tool.result`.** Tool failures are reported as `tool.result` with `is_error: true`, not as missing results.
5. **`cost` events follow `llm.response` events.** The `call_id` links them.
6. **`config_snapshot` in `triggered` is immutable for the invocation.** Config changes during an invocation are ignored; they apply to the next invocation.

## Storage and Retention

- **All events are published to JetStream streams** with file-based persistence.
- **Retention policy is `LimitsPolicy` with configurable `MaxAge`** (default: 30 days for phase 1).
- **Events are projected into SQLite** for complex queries (see the projection consumer in the phase 1 plan).
- **The projection store is a read-optimised view, not the source of truth.** Events can be re-projected from the NATS stream at any time.

## Open Questions

- **Binary payloads.** Large tool outputs (e.g. file contents, command output) inflate event sizes. Options: compression, reference-by-hash to an object store, truncation with a pointer to full content. Deferred until it's a problem.
- **Schema evolution.** When a breaking change is needed, how are old events handled? Options: migration scripts, version-aware consumers, never break. Deferred.
- **Cross-agent events.** When multi-agent orchestration arrives, events like "agent A spawned agent B" need a place in the schema. Likely a new `agent.spawned` event type with both IDs.
