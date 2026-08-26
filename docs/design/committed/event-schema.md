# Event Schema

This document specifies the event schema emitted by the factor-q runtime. It covers the three structural layers of every event (envelope, payload, annotations), per-event-type payloads, subject hierarchy, and the design rationale for each choice.

Events are the primary observability and audit surface of factor-q. Every meaningful action in the system is an event, published to NATS JetStream, and later projected into SQLite for querying.

**Schema version: 2.** See the [v1 → v2 changelog](#changelog-v1--v2) at the bottom for the breaking changes.

## The three-layer model

Every event has three structurally distinct layers, each with different write permissions, read audiences, and rules. The rationale lives in `docs/design/aspirational/inter-node-contracts-and-event-layers.md` and ADR-0016; the table below summarises:

| Layer | Written by | Read by | Mutability |
|---|---|---|---|
| Envelope | Runtime | Everyone | Immutable, closed schema |
| Payload | Producing agent | Consuming agents | Validated against producer + consumer schemas |
| Annotations | **Runtime today** (agent write path not built — see below) | Humans, meta-agents, learning loop | **Never read by consuming agents** |

The on-the-wire JSON shape:

```json
{
  "envelope": { "...closed system metadata..." },
  "payload":  { "...typed contract between graph nodes..." },
  "annotations": { "notes": "...", "confidence": 0.7 }   // omitted when empty
}
```

The three layers are separate JSON keys (not flattened) so the trust boundary is structurally enforced.

## Envelope

Closed schema — if a new field is needed, the runtime grows. Producing agents do not touch the envelope; the runtime stamps it.

| Field | Type | Purpose |
|---|---|---|
| `schema_version` | `u32` | Always `2`. Monotonic version of the envelope shape. |
| `event_id` | `string` (UUID v7) | Globally unique event identifier. UUID v7 gives time-ordered IDs. |
| `parent_event_id` | `string` (UUID v7), optional | The previous event in this invocation, if any. Omitted on the `triggered` event, on system events, and on the first event of a recovery re-emit. |
| `trace_id` | `string` (UUID v7) | Trace correlation id. Equal to `invocation_id` for now; reserved as a separate field so multi-invocation traces (graph workflows spanning multiple invocations) can be stitched together later without a wire-format change. |
| `agent_id` | `string` | Which agent this event belongs to. The sentinel value `"system"` is used for runtime-lifecycle events. |
| `invocation_id` | `string` (UUID v7) | Groups events from a single agent invocation. The primary key for grouping in projections and CLI queries. |
| `schema_id` | `string` | Stable identifier for the payload schema, e.g. `"factor-q/triggered@1"`. Versioned from day one so payloads can evolve without becoming an archaeological dig. |
| `timestamp` | `string` (RFC3339 with nanoseconds) | When the event was generated. |
| `cost` | object, optional | Cost metadata for cost-bearing events (today: `llm.response`, and `llm.failure` where the provider's usage was recoverable). See [Cost metadata](#cost-metadata) below. |

### Rationale

- **`parent_event_id` for happens-before reconstruction.** The projection (and any future replay or graph runtime) reconstructs causal order from the envelope chain rather than timestamps. Two events generated in the same nanosecond have unambiguous ordering; clocks across machines do not need to be tightly synchronised.
- **`trace_id` as a separate field even when redundant.** It lets multi-invocation graph traces land later without a wire-format change.
- **`schema_id` per payload variant.** Payloads will evolve. Versioned ids let consumers degrade gracefully (or refuse) when the producer is ahead.
- **`cost` on the envelope, not as its own event.** Cost is system-level accounting, not part of the typed contract between graph nodes (ADR-0016 §7). Riding on the envelope means one publish per LLM response instead of two, and consumers can filter on `envelope.cost IS NOT NULL` instead of subscribing to a separate subject.

### Cost metadata

When present (on `llm.response` events, and on the `llm.failure` events that bill), `envelope.cost` has:

```json
{
  "call_id": "0198f2a1-4c3b-7d21-9e88-5a0b1c2d3e4f",
  "model": "claude-haiku",
  "input_tokens": 1234,
  "output_tokens": 567,
  "cache_read_tokens": 0,
  "cache_write_tokens": 0,
  "input_cost": 0.000308,
  "output_cost": 0.000710,
  "total_cost": 0.001018,
  "cumulative_invocation_cost": 0.004523,
  "cumulative_agent_cost": 0.127890,
  "origin": { "kind": "agent_turn" }
}
```

All currency amounts are USD. `origin` mirrors the payload field of the
same name (`agent_turn`, or `sampling` / `elicitation` naming the MCP
server that asked): sampling spend is attributable to its server and
still counts toward the invocation total, which needs both facts on the
one record.

## Annotations

Open key/value commentary about an event. `Map<string, JsonValue>` with a registry of well-known keys; unknown keys are permitted.

> **Written by the runtime today. Agents have no way to annotate, and the
> keys below are a reserved vocabulary rather than an available channel.**
>
> The layer is live and load-bearing — the dispatcher and the advisory
> watch record dead-letter metadata here, and the runner attaches the
> context-pressure warning — but every writer is host code. No built-in,
> tool or reducer intent lets an agent attach an annotation to its own
> event, and nothing in an agent's prompt or tool surface offers one.
>
> Read the well-known keys below as the shape the channel will take, not
> as something an agent can use now. Anything claiming to have "flagged
> this for review" via annotations is describing a capability that does
> not exist (#90).

### When the agent write path arrives

**Consumer-driven: the write path lands when something reads it.** The
readers this layer exists for — the annotation registry and the learning
loop — are still design, not code
(`docs/design/aspirational/inter-node-contracts-and-event-layers.md`).
Building the producer first would give agents somewhere to write that
nothing consumes, which is the same trap one layer down: annotations are
stripped at the consumer barrier by design, so no agent ever reads them
back.

The interface it should take is already decided, in
[ADR-0016](../../adrs/accepted/0016-typed-operations-no-free-form-apis.md):
typed operations such as `annotations.add_note(text)` and
`annotations.record_confidence(score)`, never a free-form
`annotations.set(key, value)`. That ADR is a point-in-time record and
describes the intended interface, not a shipped one.

Note that `reasoning` needs an answer before any write path opens: it is
chain-of-thought, annotations ride every event, and the log is retained —
so the retention and payload cost of an agent-written trace is a decision
in its own right, not a detail of the plumbing.

```json
{
  "notes": "tried two approaches before settling on this",
  "confidence": 0.7,
  "reasoning": "...chain-of-thought...",
  "sources_considered": [ "...citation array..." ],
  "flags": [ "needs_human_review" ]
}
```

The field is omitted entirely when empty.

### Well-known keys

The reserved vocabulary. Four of the five have **no writer at all** today;
`flags` is written by the runtime. The "Status" column says which is which,
so a reader can tell the shipped part from the reserved part.

| Key | Type | Status | Semantics |
|---|---|---|---|
| `notes` | string | Reserved — no writer | Free-form commentary from the producing agent. |
| `confidence` | number | Reserved — no writer | Self-reported confidence. Advisory only — calibrated confidence comes from a verifier node, not from the producer. |
| `reasoning` | string | Reserved — no writer; see the retention question above | Chain-of-thought / working. The fresh-context discipline depends on this never reaching a downstream agent's prompt. |
| `sources_considered` | array of `Citation` | Reserved — no writer | Sources looked at but not directly used in the payload. Sources actually used belong in a typed `Citation[]` field on the payload. |
| `flags` | array of strings | **Runtime writes this** | Markers the producer wants downstream humans (or a meta-agent) to see. |

The host also writes `dead_letter_*` keys (trigger id, payload, stream
sequence, source) on dead-letter events. Those are runtime bookkeeping
rather than commentary, and are deliberately not part of the reserved
vocabulary above.

> **Known divergence:** the runtime's one `flags` writer — the
> context-pressure warning in `worker/reducer/runner/llm.rs` — emits an
> object (`{"context_pressure": "…"}`), not the array of strings this
> table specifies. `Annotations` is `Map<string, JsonValue>`, so nothing
> catches it. Recorded rather than silently resolved: which shape is right
> is a decision, and it is tracked on #90.

### The annotation barrier

The single rule that makes the three-layer model work: **the executor strips annotations from the input context when building the prompt for a consuming agent.** A consuming agent sees the payload and selected envelope fields, never the annotations from upstream events.

This is enforced by [`Event::for_consumer_context`](#consumer-view) in the runtime, not by convention.

### Consumer view

`Event::for_consumer_context()` returns a `ConsumerView { envelope, payload }` whose `Serialize` impl produces `{"envelope": ..., "payload": ...}` with no `annotations` key — even when the underlying event has annotations. This is the only sanctioned way to feed an upstream event into a downstream agent's prompt context.

Direct access to `event.annotations` remains available for humans, meta-agents, and the learning loop. Only the consumer-prompt path is barred.

## Subject Hierarchy

Events are published to NATS subjects following this pattern:

```
fq.agent.{agent_id}.{event_type}[.{sub_type}]
fq.worker.{worker_id}.{event_type}[.{sub_type}]
fq.system.{event_type}
```

Concrete subjects:

| Subject | Event |
|---|---|
| `fq.agent.{agent_id}.triggered` | An invocation has started |
| `fq.agent.{agent_id}.llm.request` | LLM call about to be made |
| `fq.agent.{agent_id}.llm.dispatched` | LLM call has returned to the runtime (WAL middle-state) |
| `fq.agent.{agent_id}.llm.response` | LLM call has returned and the response is durably written (carries `envelope.cost`) |
| `fq.agent.{agent_id}.llm.failure` | LLM call ended without a response — a provider error, or a 200 with nothing in it (carries `envelope.cost` only when the provider's usage was recoverable) |
| `fq.agent.{agent_id}.tool.call` | Agent is invoking a tool |
| `fq.agent.{agent_id}.tool.dispatched` | Tool has returned to the runtime (WAL middle-state) |
| `fq.agent.{agent_id}.tool.result` | Tool invocation has completed (success or failure) |
| `fq.agent.{agent_id}.invocation.ambiguous` | An invocation is in recovery limbo — an ambiguous WAL row on restart, or a failed automatic resume — and needs operator attention |
| `fq.agent.{agent_id}.invocation.archived` | Worker → control-plane: invocation reached terminal; hand off the final state |
| `fq.agent.{agent_id}.invocation.operator_recovered` | Operator → control-plane: operator-issued terminal transition (`fq invocation drop`) |
| `fq.agent.{agent_id}.invocation.operator_resumed` | Operator → worker: interrupted-result injection (`fq invocation resume`), with completed call ids and optional reason |
| `fq.agent.{agent_id}.host_notice` | A durable host notice was injected into the conversation at a step boundary |
| `fq.agent.{agent_id}.invocation_summary` | One-line operator-facing summary; always published under the reserved `summary` agent id |
| `fq.agent.{agent_id}.completed` | Invocation has finished successfully |
| `fq.agent.{agent_id}.failed` | Invocation has terminated with an error |
| `fq.worker.{worker_id}.heartbeat` | Worker liveness signal (periodic) |
| `fq.worker.{worker_id}.orphaned` | Worker heartbeat lapsed without clean shutdown — emitted once per alive→stale transition by the coordination sweep (#64); payload carries `worker_id` and `last_heartbeat_ms` |
| `fq.worker.{worker_id}.invocation.archive_acked` | Control-plane → worker: archive row written; safe to delete local `invocation_state` |
| `fq.system.startup` | Runtime lifecycle — startup |
| `fq.system.shutdown` | Runtime lifecycle — shutdown |
| `fq.system.task_failed` | A hosted task inside `fqd` exited with an error |
| `fq.system.recovery` | Daemon-startup snapshot of in-flight invocation categorisation |
| `fq.system.mcp.log` | A log record forwarded from a connected MCP server (ADR-0020); daemon-scoped, so no agent or invocation |

### Rationale

- **Agent ID in the subject, not just the payload.** A consumer can subscribe to `fq.agent.researcher.>` to only see events from the researcher agent without filtering in application code.
- **Hierarchical types** (`llm.request` vs `llm.response`). Allows wildcards: `fq.agent.*.llm.>` matches all LLM events across all agents.
- **System events are a separate namespace.** Runtime lifecycle is not tied to any agent.
- **Worker-scoped subjects (`fq.worker.>`)** for events whose audience is one specific worker rather than every consumer of the agent's lifecycle: heartbeats, and the control-plane → worker `invocation.archive_acked` reply. Worker-scoped subscriptions stay narrow with a single filter (`fq.worker.{worker_id}.>`) and avoid cross-worker delivery noise. The fan-out subjects (`fq.agent.>`) remain the canonical place for invocation lifecycle events the rest of the system should see. `worker.orphaned` also rides this namespace — not because its audience is the (dead) worker, but because it is worker- not agent-scoped; system-wide reactors subscribe with the `fq.worker.*.orphaned` wildcard.
- **WAL middle-state events** (`llm.dispatched`, `tool.dispatched`) sit between the request and result. They're an operational signal — recovery uses the SQLite WAL rows, not these events, but they let observers see "the call has returned, we're about to write the result."

## Event Types

### `triggered`

Published when an agent invocation begins. Carries a snapshot of the agent's configuration so the event log is self-contained for replay even if the agent definition is later modified.

```json
{
  "trigger_id": "01890000-0000-7000-8000-000000000001",
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
      "fs_write": ["/project/output"],
      "network": ["*.api.internal"],
      "env": ["RESEARCH_API_KEY"],
      "exec_cwd": ["/project"]
    },
    "budget": 0.50,
    "sampling": { "...": "MCP capability grants, omitted when nothing is granted" },
    "sampling_validation": { "...": "redaction + evaluator gates, omitted when empty" }
  }
}
```

**Design notes:**
- **`config_snapshot` is a partial capture, and the gap matters.** The intent is that the trace shows exactly what was running even after the definition changes. `ConfigSnapshot` carries eleven of `Agent`'s sixteen fields; **`max_iterations`, `effort`, `trigger`, `mcp_servers` and `static_resources` are not captured**, and every one of them changes what actually ran. Two invocations of the same agent across an `fq reload` that altered any of them produce identical snapshots. Treat the snapshot as the configuration that shaped the *conversation*, not as the full definition — closing the gap is a code change, tracked as one.
- **`trigger_id` names the trigger this invocation came from.** UUIDv7, minted or honoured per the [trigger wire contract](trigger-wire-contract.md#trigger-identity-fq-trigger-id), where it travels as the `Fq-Trigger-Id` header. Before it existed, an invocation was linked to its trigger only by *content* — matching subject and payload — which cannot distinguish two identical triggers and cannot be keyed on. This is that link, by identity.
- **`trigger_id` is optional on read, always written.** Every `triggered` event published since 2026-08-10 carries one; events already on the log do not, and the field deserialises as absent for them. It is optional for exactly that reason and no other — a required field would fail replay of the existing log and refuse events from older peers (invariant 11).
- **`trigger_source` indicates who initiated.** `manual`, `subject`, or `schedule`.
- **`trigger_payload` is opaque.** Any JSON value, defined by the trigger source.

### `llm.request`

Published immediately before an LLM call is made.

```json
{
  "call_id": "0198f2a1-4c3b-7d21-9e88-5a0b1c2d3e4f",
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
  },
  "origin": { "kind": "agent_turn" }
}
```

**Design notes:**
- **Full message history is sent every time.** Reconstructing context from earlier events would be fragile.
- **`tools_available` is a snapshot per call.** Tool schemas can change between calls.
- **`call_id` correlates with the response.** It is a UUID the runtime mints for the call, not a provider identifier — unlike `tool_call_id`, which comes from the provider and is carried through verbatim in whatever shape it arrives.
- **`origin` says what prompted the call.** `agent_turn` for a reducer-driven reasoning turn, or `sampling` / `elicitation` with the requesting MCP server's name (ADR-0018), so the request/response trace is self-describing about whose spend it is. Absent on events written before the field existed, which read as `agent_turn`.

### `llm.dispatched`

WAL middle-state event for LLM calls. Emitted between `llm.request` and `llm.response` once the request has returned control to the runtime, before the response is durably written.

```json
{
  "call_id": "0198f2a1-4c3b-7d21-9e88-5a0b1c2d3e4f",
  "model": "claude-haiku"
}
```

**Design notes:**
- **Operationally informational.** Downstream consumers can ignore it; recovery uses the `llm_dispatch.status = 'dispatched'` row in the worker store, not this event.
- **Same call_id as the matching `llm.request` / `llm.response`.**

### `llm.response`

Published when an LLM call returns and the response is durably written. The envelope carries cost metadata (`envelope.cost`) — there is no separate cost event.

```json
{
  "round": 3,
  "call_id": "0198f2a1-4c3b-7d21-9e88-5a0b1c2d3e4f",
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
  },
  "origin": { "kind": "agent_turn" }
}
```

**Design notes:**
- **Cost rides on the envelope.** See [Cost metadata](#cost-metadata) above. Consumers query `WHERE event_type IN ('llm_response', 'llm_failure') AND total_cost IS NOT NULL` for cost-bearing per-call events.
- **`tool_call_id` is assigned by the LLM.**
- **`usage` carries raw token counts**, mirrored in `envelope.cost` along with the computed dollar values.

### `llm.failure`

The other terminal outcome of an LLM call (#447): the provider errored, or returned a 200 with no content and no tool calls. Sibling of `llm.response` rather than a nullable-fields variant of it, so a consumer's match site says which case it is in.

```json
{
  "round": 4,
  "call_id": "0198f2a1-4c3b-7d21-9e88-5a0b1c2d3e4f",
  "model": "claude-haiku-4-5",
  "error_kind": "auth | rate_limited | invalid_response | request_failed | unpriced_model | empty_response",
  "error_message": "rate limited",
  "duration_ms": 4200,
  "usage": {
    "input_tokens": 1234,
    "output_tokens": 0,
    "cache_read_tokens": 0,
    "cache_write_tokens": 0
  }
}
```

**Design notes:**
- **`usage` is optional, and `None` is not zero.** Absent means "we do not know what the provider billed" — a transport failure yields no parsed body. Present means the counts are real: an empty completion still bills for the prefill. `envelope.cost` follows the same rule and is *absent*, never zeroed, when usage is unknown.
- **`error_kind` mirrors `LlmError`**, plus `empty_response`, which has no error counterpart and is the one failure kind that can bill. A 429 currently arrives as `request_failed` with the status in `error_message`; [#278](https://github.com/bricef/factor-q/issues/278)'s `Retry-After` work is where `rate_limited` starts being produced.
- **A failed call is not a failed invocation.** The agent-turn case publishes `failed` separately; a failed sampling or elicitation call declines the server's request and the invocation continues (ADR-0018).
- **`duration_ms` includes the hidden retry attempts** inside `RetryingLlmClient`, which is the tell for a rate limit that eventually gave up. The attempt count itself is not surfaced — it lives below `LlmClient::chat` and belongs with #278.

### `tool.call`

Published when the agent invokes a tool. Each tool call in a single LLM response produces its own `tool.call` event.

```json
{
  "round": 3,
  "tool_call_id": "tool-01HXJ...",
  "tool_name": "read",
  "parameters": { "path": "/project/docs/overview.md" }
}
```

**Design notes:**
- **`round` is the initiating assistant turn's Round**, so every call and result from one model response shares a number and a reader can group them without walking the chain. It reads `0` on events written before the field existed, which is not a real Round.

### `tool.dispatched`

WAL middle-state event for tool calls, mirroring `llm.dispatched`. Emitted between `tool.call` and `tool.result`.

```json
{
  "tool_call_id": "tool-01HXJ...",
  "tool_name": "read"
}
```

### `tool.result`

Published when a tool invocation completes. Sandbox violations and other tool errors surface here with `is_error: true`, not as separate events.

```json
{
  "round": 3,
  "tool_name": "read",
  "tool_call_id": "tool-01HXJ...",
  "output": "# Overview\n\nThis project...",
  "is_error": false,
  "duration_ms": 12
}
```

Error case:

```json
{
  "round": 3,
  "tool_name": "read",
  "tool_call_id": "tool-01HXJ...",
  "output": "Path /etc/passwd is outside the agent's allowed filesystem scope",
  "is_error": true,
  "error_kind": "sandbox_violation",
  "duration_ms": 1
}
```

`error_kind` values: `sandbox_violation`, `invalid_parameters`, `execution_failed`, `timeout`, `permission_denied`.

**Design notes:**
- **`tool_name` is restated here**, even though the initiating `tool.call` already carries it, so a result renders on its own. The parameters are not restated — those stay on the call, reachable via `parent_event_id`. Empty on events written before the field existed.

### `host_notice`

Published by the runner when a queued host notice is drained into the conversation at a step boundary (#155, phase 1 of #88). It exists so an operator can see that the host spoke to the agent without diffing message arrays.

```json
{
  "kind": "resume",
  "body": "<host-notice>resumed</host-notice>"
}
```

**Design notes:**
- **The WAL row is the source of truth, not this event.** `queue_host_notice` persists the notice into `worker.db`'s `host_notice` table before the `StepInput` that carries it is built, and resume replays that row verbatim. A notice recorded by an incarnation that then crashed is *not* re-emitted on resume — the event is observability, the row is the channel.
- **`body` arrives fully rendered**, `<host-notice>` sentinel included. Producers render once; replay never re-renders, so the exact string is what the model saw.
- **The channel is wired ahead of its producers.** Nothing in the daemon queues a notice today; the only callers are the simulation harness. Expect the event on the wire when phase 2 of #88 lands, not before.

### `invocation.ambiguous`

Published by the worker on startup for an invocation in recovery limbo (#64), in either of two modes:

1. **Ambiguous WAL categorisation** — a `dispatched`-without-`completed` row. See `docs/design/committed/data-architecture.md` §3.4.
2. **Failed automatic resume** — a safe-resume/safe-replay invocation whose `resume()` errored. `stuck_entity` is the sentinel `"recovery"`, `stuck_call_id` carries the invocation id, and `note` carries the resume error.

```json
{
  "stuck_entity": "tool_dispatch | llm_dispatch | recovery",
  "stuck_call_id": "tool-01HXJ...",
  "note": "Tool returned but no completion record"
}
```

**Design notes:**
- **Operator-triage event.** The control-plane consumes it and marks the ownership row `ambiguous`, which is what `fq invocation list --status=ambiguous` surfaces; `fq invocation resume` and `fq invocation drop` are the two ways out. The github-watcher treats it as a failed, operator-attention outcome.
- **Full context lives in the worker's WAL**, not on the wire. This payload is the minimum needed for an operator to find the row.
- **Once per invocation, across restarts.** Emission is guarded by the worker store's `ambiguous_reported_at` stamp, so a persistently-broken invocation does not re-fire on every daemon restart.

### `completed`

Published when an invocation finishes without a runtime failure. Note
that "the runtime finished cleanly" and "the task was achieved" are
different axes — the latter is `task_status`.

```json
{
  "task_status": "success",
  "result_summary": "Completed research task and wrote findings to /project/output/report.md",
  "total_llm_calls": 4,
  "total_tool_calls": 7,
  "total_cost": 0.004523,
  "total_duration_ms": 12345
}
```

**`task_status`** (#125): the agent's own declaration of how the *task*
went — `success | failed | blocked | partial`, defaulting to `success`
when absent (pre-#125 historical events; current runs cannot complete
without declaring — an undeclared run ends as a `failed` event via
budget/iteration exhaustion, not as `completed`). Orthogonal
to the runtime axis: `failed` events with a `FailureKind` model runtime
failure; `task_status` models "the runtime worked — was the goal
achieved?". Declared via the terminal `report_outcome` tool, which the
reducer harness intercepts as the terminal transition (never
dispatched) — the only completion path; a turn ending with no tool
calls is answered with a corrective host notice and another model
turn, never an implicit `success`.

### `failed`

Published when an invocation terminates with an error.

Projection `error_kind` values use this same snake_case wire vocabulary. Rebuild the projection from NATS to normalize rows written by older versions that used concatenated names.

```json
{
  "error_kind": "budget_exceeded | llm_error | max_iterations | tool_error | sandbox_violation | runtime_error | trigger_exhausted",
  "error_message": "Agent budget of $0.50 exceeded after 5 LLM calls",
  "phase": "llm_request | llm_response | tool_call | tool_result | setup | reducer | host_step_budget | budget",
  "partial_totals": {
    "total_llm_calls": 5,
    "total_tool_calls": 3,
    "total_cost": 0.512,
    "total_duration_ms": 8234
  }
}
```

### `invocation.archived`

Published by the worker after an invocation reaches terminal state, carrying the final state blob the control-plane writes into `invocation_archive`. Emitted *after* the terminal lifecycle event (`completed` or `failed`), in the same invocation chain.

```json
{
  "worker_id": "worker-001",
  "final_phase": "completed | failed",
  "final_state_blob": [/* opaque bytes; the reducer's terminal state */],
  "started_at_ms": 1716640123456,
  "terminal_at_ms": 1716640135789
}
```

**Design notes:**
- **Canonical position:** `... → completed|failed → invocation.archived → invocation.archive_acked`. See data-architecture.md §9.3.
- **`worker_id` rides on the payload, not the subject.** The subject is agent-scoped so the coordination consumer's existing `fq.agent.*.invocation.*` filter picks it up; the control-plane needs the `worker_id` to address the ack back at `fq.worker.{worker_id}.invocation.archive_acked`.
- **`final_state_blob` is opaque.** The control-plane stores it as-is into `invocation_archive.state_blob`. Default serde encoding (JSON array of integers) is used today; if blob sizes start to strain the wire format, swap in `serde_bytes` here and in `InvocationStateRow`.
- **Idempotent on the receiver.** The control-plane's `insert_archive` is `ON CONFLICT(invocation_id) DO NOTHING`; a redelivered `invocation.archived` is safe.

### `invocation.archive_acked`

Published by the control-plane on the worker-scoped subject after a successful (or idempotent no-op) `insert_archive`. Receipt tells the worker the archive row is durably written and the local `invocation_state` row can be deleted.

```json
{
  "worker_id": "worker-001"
}
```

**Design notes:**
- **Worker-scoped subject.** `fq.worker.{worker_id}.invocation.archive_acked` so each worker subscribes with a single filter on its own id. The coordination consumer does not double-consume the ack.
- **`invocation_id` rides on the envelope** — see ADR-0016 on payload vs envelope. The payload carries `worker_id` only as a defense-in-depth check on the receiving worker (the subject token already routes by `worker_id`).
- **Emitted on every successful insert, including the idempotent no-op.** Otherwise a redelivered `invocation.archived` would never re-trigger the ack and a worker that missed the first one would never clean up.
- **Subscription is core NATS, not durable JetStream.** Acks missed while the consumer is offline are recovered by the worker's retry sweeper republishing `invocation.archived` until a fresh ack arrives.

### `invocation.operator_recovered`

Published by `fq invocation drop` (and any future operator-issued recovery action) so audit can distinguish operator-triggered terminal transitions from worker-triggered ones. The coordination consumer's handler writes an `invocation_archive` row (with an empty `state_blob` in v1 — the control-plane doesn't have the worker's state for an ambiguous invocation) and updates `coordination_invocation_owner.status` to match `final_phase`. No ack is emitted.

```json
{
  "action": "drop",
  "final_phase": "failed",
  "reason": "stuck on flaky network call"
}
```

**Design notes:**
- **`action` is `"drop"` in v1.** The field exists so future actions (`resume`, `requeue`) can be distinguished without minting a new variant.
- **`final_phase` is `"failed"` in v1.** A future `resume` would set `"completed"`.
- **`reason` is operator-supplied free-form.** Audit-only; consumers must not parse it. Omitted on the wire when absent.
- **Resume semantics are deferred.** The control-plane doesn't currently hold the worker's `state_blob` for ambiguous invocations; honest resume would require either enriching `invocation.ambiguous` with the blob or adding an operator-RPC to the worker. See the step-9 plan (`docs/plans/closed/2026-05-22-operator-cli.md`).
- **No ack.** Unlike `invocation.archived`, no worker is waiting to clean up. The `invocation.archived` handler has a no-downgrade guard so a late `archived` event from a still-alive worker doesn't override the operator's `Failed`.

### `invocation.operator_resumed`

Published by `fq invocation resume` — the other half of the ambiguous-invocation exit, and the sibling of `invocation.operator_recovered` where progress is preserved rather than abandoned. The daemon durably completes each stuck tool dispatch with an interrupted result, then re-drives normal SafeReplay recovery; this event records which calls it closed.

```json
{
  "completed_call_ids": ["tool-01HXJ...", "tool-01HXK..."],
  "reason": "provider recovered; re-driving"
}
```

**Design notes:**
- **Audit-only, and published after the fact.** The WAL injection is the source of truth and has already committed by the time this goes out — a failed publish is warned about, never retried, because retrying it could not change what happened. That is why the resume reply carries `completed_call_ids` even when it reports failure.
- **The coordination consumer ignores it.** No ownership status follows from a resume: the invocation is being re-driven, and its eventual `completed`/`failed` is what moves the row.
- **`completed_call_ids` is the operator's receipt**, naming exactly the dispatches that were closed out. An empty list means the WAL held nothing stuck, which is itself worth seeing.

### `invocation_summary`

Published by the daemon's summary consumer (#216) — never by an agent — under the reserved sentinel `agent_id` of `"summary"`, with `invocation_id` binding the line to the summarised invocation. Subject: `fq.agent.summary.invocation_summary`.

```json
{
  "kind": "progress",
  "summary": "Fixing #7: tests green, opening the PR"
}
```

**Design notes:**

- **`kind` is `start` | `progress` | `outcome`.** `start` summarises the trigger payload (what work is expected), `progress` is a rolling update from the latest model turn, `outcome` is the final line on `completed`/`failed`.
- **Cost rides the envelope.** The summariser's own token usage and spend are attached as `envelope.cost` exactly as on `llm.response` — so `fq costs` (and the dashboard's per-model split) report the summariser as its own `summary` agent row, and no invocation's totals or budget are touched (operator overhead by design).
- **Reported everywhere, allocated nowhere (#466).** That splits the cost queries along one axis: a query that answers *what was spent* counts this event (per-agent, per-model, per-time-bucket), and a query that answers *what did this invocation cost* does not (`cost_by_invocation`, `cost_of_invocation`). Per-invocation figures therefore do not sum to the agent total, which is correct and must not be silently so: the aggregate carries the shortfall as `framework_cost` — engine spend no invocation caused — and `total_cost = <per-invocation costs> + framework_cost` holds for every agent. Zero for an ordinary agent, the whole row for `summary`.
- **Derived, not authoritative.** The projection maintains `invocation_summary` (current line per invocation, last write wins). A reprojection replays these events; the LLM is never re-called.
- **The summariser never writes into the invocation.** No WAL row, no conversation message — the reducer's resume/drain equivalence is untouched by construction.

### `system.startup`

```json
{
  "runtime_id": "01997c4e-9b1a-7c33-8f0d-2a5b6c7d8e9f",
  "version": "0.1.0",
  "nats_url": "nats://localhost:4222",
  "agents_loaded": 3,
  "pricing_entries": 12
}
```

System events share a sentinel `agent_id` of `"system"`; their envelope's `invocation_id` and `trace_id` are set to `runtime_id` so all events from a single daemon run share a correlation key. `parent_event_id` is always absent on system events.

### `system.shutdown`

```json
{
  "runtime_id": "01997c4e-9b1a-7c33-8f0d-2a5b6c7d8e9f",
  "reason": "ctrl_c | task_failed | error",
  "clean": true
}
```

### `system.task_failed`

A hosted task inside `fqd` (the projection consumer, the trigger dispatcher, the coordination consumer, etc.) exited with an error before a graceful shutdown was requested. The daemon then shuts itself down so operators don't unknowingly rely on a half-broken daemon.

```json
{
  "runtime_id": "01997c4e-9b1a-7c33-8f0d-2a5b6c7d8e9f",
  "task_name": "coordination_consumer",
  "error_message": "..."
}
```

### `system.recovery`

Emitted once per daemon startup with the counts of in-flight invocations classified by recovery category (see `docs/design/committed/data-architecture.md` §7.1).

```json
{
  "runtime_id": "01997c4e-9b1a-7c33-8f0d-2a5b6c7d8e9f",
  "worker_id": "worker-001",
  "safe_resume": 2,
  "safe_replay": 0,
  "ambiguous": 1,
  "total": 3
}
```

### `mcp_server_log`

A log record a connected MCP server emitted (`notifications/message`), bridged onto the bus by the daemon's notification drain (ADR-0020). Subject: `fq.system.mcp.log`.

```json
{
  "server": "github",
  "level": "warning",
  "logger": "rate-limiter",
  "data": { "message": "secondary rate limit; backing off 30s" }
}
```

**Design notes:**
- **Daemon-scoped, so it carries no agent or invocation.** Shared MCP servers outlive any one invocation and serve several agents; attributing their logs to whichever agent happened to be running when they spoke would be a fiction. This is why the event sits in the `fq.system.*` namespace rather than under `fq.agent.*`.
- **`data` is passed through as the server sent it.** The daemon does not reshape or validate the body — it is another process's log line, and the value of forwarding it is that it arrives unedited.
- **`level` is the MCP level name** (`"debug"` through `"emergency"`), not the runtime's own `tracing` vocabulary.

## Invariants

The following invariants hold across the event stream and are assumed by consumers:

1. **Events within one `invocation_id` are totally ordered** by the envelope chain. Sorting by `event_id` (UUID v7 is time-sortable) is a good fallback; following `parent_event_id` is authoritative.
2. **Every invocation starts with a `triggered` event** and ends with either `completed` or `failed`. The `triggered` event is the chain root (`parent_event_id` absent).
3. **Every `llm.request` is followed by `llm.dispatched`, then exactly one
   terminal outcome — `llm.response` on success, `llm.failure` on a provider
   or validation error** — all three bearing the same `call_id`.
   Provider-level retries happen below this boundary (`RetryingLlmClient`)
   and are not event-visible, so one request yields one outcome. The
   invariant is keyed on `call_id`, not on invocation: an invocation may
   contain several `llm.failure` events, because a failed *sampling* or
   *elicitation* call declines the server's request without ending the
   agent's invocation, while a failed *agent turn* additionally emits
   `failed`.
4. **Every `tool.call` is followed by `tool.dispatched` then `tool.result`.** Tool failures surface as `tool.result` with `is_error: true`, not as missing results. Both this invariant and the one above hold unconditionally: `ReducerRunner` is the only implementation of the `Worker` trait, so every invocation goes through the WAL and there is no dispatch-free path left to carve out.
5. **`envelope.cost` is present on `llm.response` events that bill, and on
   `llm.failure` events where the provider's usage was recoverable** (an
   empty completion). It is *absent* — never zeroed — when usage is
   unknown, so `total_cost IS NULL` continues to mean "no known spend"
   rather than "zero spend". There is no separate cost event.
6. **`config_snapshot` in `triggered` is immutable for the invocation.** Config changes during an invocation are ignored; they apply to the next invocation.
7. **`invocation.ambiguous` is emitted by the worker on startup** for any invocation whose WAL classification returns "ambiguous" — or whose automatic resume fails (`stuck_entity: "recovery"`). It fires at most once per invocation across restarts (the worker store's `ambiguous_reported_at` stamp). The chain root for that emission is the new event itself (`parent_event_id` absent — recovery starts a fresh chain; see the recovery rationale in `data-architecture.md` §3.4).
8. **`invocation.archived` immediately follows the terminal lifecycle event** (`completed` or `failed`) in the same invocation chain. The worker's retry sweeper may republish `invocation.archived` if the control-plane ack does not arrive; republishes keep the same `invocation_id` and the control-plane's insert is idempotent on it. `invocation.archive_acked` is the control-plane's reply on the worker-scoped subject and closes the hand-off.
9. **`invocation.operator_recovered` is operator-initiated** and rooted on its own envelope (the operator's `fq` process is not the original worker, so the chain is fresh). Terminal status set by this event is sticky — the coordination consumer's `invocation.archived` handler will not downgrade an already-terminal owner status if a still-alive worker emits `archived` after the operator's drop.
10. **`worker.orphaned` fires exactly once per alive→stale transition** — the coordination sweep's conditional store update consumes the transition, and a publish failure after that is logged, not retried (at-most-once; the stale row remains visible via `fq workers list --stale-only`). The row is not kept forever: the daemon's retention sweep deletes stale registrations older than `state.stale_worker_retention_days` (default 7 days), but never one that still owns `in_flight` or `ambiguous` invocations.
11. **A payload field added after events exist is optional on read, and
    only for that reason.** Deserialisers accept its absence and readers
    treat absence as "not recorded", never as a default value that could
    be mistaken for a recorded one — the log is append-only, so a
    required addition breaks replay of everything already written and
    refuses events from peers that predate it. (This is a rule the tree
    has already paid for once: a required `EventView::seq` broke the
    dashboard.) It is *not* licence to write the field inconsistently:
    every producer of the new shape writes it. `trigger_id` on
    `triggered` is the current instance.

## Storage and Retention

- All events are published to JetStream streams with file-based persistence.
- Retention policy is `LimitsPolicy` with a `MaxAge` of 30 days. That window is a compile-time constant (`bus::DEFAULT_MAX_AGE`), not an operator setting: changing it takes a rebuild, which [Design Principle 8](design-principles.md#8-tunable-parameters-are-configuration-not-code) says a tunable should not.
- Events are projected into SQLite for complex queries.
- The projection store is a read-optimised view, not the source of truth. Events can be re-projected from the NATS stream at any time.

### Retention and the trail's lifetime

The event trail has no payload-bearing system of record beyond JetStream retention. The projection is a long-lived but lossy, derived view, and the invocation archive keeps outcomes rather than the trail that produced them.

| Surface | Lifetime | What survives and record status |
|---|---|---|
| NATS `fq-events` | 30 days, fixed in code | Complete payload-bearing event trail; deleted after retention. The trigger and advisory streams keep messages for 24 hours, likewise fixed. |
| SQLite projection (`events`) | 30 days by default (`[state].retention_days`); cost-bearing rows kept indefinitely | Typed columns only, without event payloads. The daemon prunes it on the scheduled retention sweep, except rows carrying `total_cost` (`llm_response`, `llm_failure`, `invocation_summary`) — cost accounting is a primary platform concern and spend figures must survive retention. |
| `invocation_archive` in `control-plane.db` | 30 days by default — the same `[state].retention_days`, keyed on `archived_at` | Per-invocation final phase, final reducer-state blob and timestamps; not the event trail. Non-rebuildable while it lives, and swept on the same tick as the projection above. |

**No CAS archive appears in that table, because none is wired up.**
[ADR-0026](../../adrs/accepted/0026-event-log-system-of-record.md) accepted a
dedicated content-addressed archive service as the event log's system of
record; it has not been built. No runtime crate depends on `fq-store`, and
`fq-cas` ships as a standalone binary the daemon never calls. What holds
invocation outcomes today is the control-plane table above, expiring on the
same 30-day clock as everything else — so nothing here outlives retention
except the cost-bearing projection rows. Read ADR-0026 as the direction, not
as a durability guarantee you already have.

After a projection sweep, replaying the retained NATS stream is the supported recovery path and can recover only events still inside JetStream retention. Events older than stream retention are gone by design; the projection intentionally does not preserve typed rows past that boundary. A stronger re-projection guarantee is tracked in [#139](https://github.com/bricef/factor-q/issues/139) and [#163](https://github.com/bricef/factor-q/issues/163).

Three consumer consequences follow. First, **cost figures are exempt from the sweep**: rows with `total_cost` set are retained indefinitely, so all-time spend totals (dashboard cost pages, `fq costs`) and per-invocation cost display survive retention by design. Second, non-cost aggregates computed from projected events — event counts, failure tallies — cover at most the retention window, not all time. Third, the retained cost rows are deliberately *not* rebuildable once older than stream retention: the projection is their only copy, so `projection.db` must be included in backups; a durable re-sourcing of cost aggregates (per ADR-0026's outcome-record direction) remains open as follow-up work. For non-cost rows, keep `[state].retention_days` at or below the NATS stream retention — a longer window re-creates the sole-holder inversion this sweep exists to remove.

## Open Questions

- **Cross-incarnation chain stitching.** Recovery re-emits start a fresh chain (`parent_event_id` absent). If audit code ever needs to thread the pre-crash and post-recovery chains together, an optional `envelope.recovered_from_event_id: Uuid` could be added.
- **Binary payloads.** Large tool outputs inflate event sizes. Options: compression, reference-by-hash to an object store, truncation with a pointer. Deferred until it's a problem.
- **Schema evolution past v2.** When a breaking change is needed, the version field and per-payload `schema_id` give us the substrate; the migration story (rolling upgrades, multi-version consumers) is a separate design.
- **Cross-agent graph events.** When multi-agent graph orchestration arrives, events like "edge fired" or "node spawned" need their own types. The `trace_id` field is reserved for that.

## Changelog: v1 → v2

This was a breaking change with no wire-format compatibility shim. v1 events do not parse against v2 deserialisers and vice versa. Acceptable because no production deployment existed at the time of the change.

| Change | Reason |
|---|---|
| `schema_version` bumped 1 → 2 | Marks the wire-format break. |
| Top-level shape split: `{schema_version, event_id, timestamp, agent_id, invocation_id, event_type, payload}` → `{envelope, payload, annotations}` | Three structurally distinct layers express the trust/visibility boundary in the type system rather than by convention (ADR-0016, `inter-node-contracts-and-event-layers.md`). |
| Envelope gains `parent_event_id` | Reconstruct happens-before from the chain rather than timestamps. |
| Envelope gains `trace_id` | Reserved for multi-invocation graph traces; equal to `invocation_id` for now. |
| Envelope gains `schema_id` per payload variant | Versioned-from-day-one payload evolution. |
| Envelope gains optional `cost` | Cost is system-level accounting, not a typed contract between graph nodes (ADR-0016 §7). |
| `Cost` event type removed; `fq.agent.*.cost` subject removed | Cost folds into `envelope.cost` on `llm.response` events. |
| `Annotations` layer added (top-level `annotations` field, omitted when empty) | Substrate for advisory commentary; never read by consuming agents. The runtime enforces the consumer barrier via `Event::for_consumer_context`. |
| Well-known annotation keys: `notes`, `confidence`, `reasoning`, `sources_considered`, `flags` | Stable vocabulary for the learning loop; unknown keys still permitted. |
