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
| Annotations | Producing agent | Humans, meta-agents, learning loop | **Never read by consuming agents** |

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
| `cost` | object, optional | Cost metadata for cost-bearing events (today: `llm.response`). See [Cost metadata](#cost-metadata) below. |

### Rationale

- **`parent_event_id` for happens-before reconstruction.** The projection (and any future replay or graph runtime) reconstructs causal order from the envelope chain rather than timestamps. Two events generated in the same nanosecond have unambiguous ordering; clocks across machines do not need to be tightly synchronised.
- **`trace_id` as a separate field even when redundant.** It lets multi-invocation graph traces land later without a wire-format change.
- **`schema_id` per payload variant.** Payloads will evolve. Versioned ids let consumers degrade gracefully (or refuse) when the producer is ahead.
- **`cost` on the envelope, not as its own event.** Cost is system-level accounting, not part of the typed contract between graph nodes (ADR-0016 §7). Riding on the envelope means one publish per LLM response instead of two, and consumers can filter on `envelope.cost IS NOT NULL` instead of subscribing to a separate subject.

### Cost metadata

When present (on `llm.response` events), `envelope.cost` has:

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

All currency amounts are USD.

## Annotations

Open key/value commentary from the producing agent. `Map<string, JsonValue>` with a registry of well-known keys; unknown keys are permitted.

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

| Key | Type | Semantics |
|---|---|---|
| `notes` | string | Free-form commentary from the producing agent. |
| `confidence` | number | Self-reported confidence. Advisory only — calibrated confidence comes from a verifier node, not from the producer. |
| `reasoning` | string | Chain-of-thought / working. The fresh-context discipline depends on this never reaching a downstream agent's prompt. |
| `sources_considered` | array of `Citation` | Sources looked at but not directly used in the payload. Sources actually used belong in a typed `Citation[]` field on the payload. |
| `flags` | array of strings | Markers the producer wants downstream humans (or a meta-agent) to see. |

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
| `fq.agent.{agent_id}.tool.call` | Agent is invoking a tool |
| `fq.agent.{agent_id}.tool.dispatched` | Tool has returned to the runtime (WAL middle-state) |
| `fq.agent.{agent_id}.tool.result` | Tool invocation has completed (success or failure) |
| `fq.agent.{agent_id}.invocation.ambiguous` | An invocation is in recovery limbo — an ambiguous WAL row on restart, or a failed automatic resume — and needs operator attention |
| `fq.agent.{agent_id}.invocation.archived` | Worker → control-plane: invocation reached terminal; hand off the final state |
| `fq.agent.{agent_id}.invocation.operator_recovered` | Operator → control-plane: operator-issued terminal transition (`fq invocation drop`) |
| `fq.agent.{agent_id}.completed` | Invocation has finished successfully |
| `fq.agent.{agent_id}.failed` | Invocation has terminated with an error |
| `fq.worker.{worker_id}.heartbeat` | Worker liveness signal (periodic) |
| `fq.worker.{worker_id}.orphaned` | Worker heartbeat lapsed without clean shutdown — emitted once per alive→stale transition by the coordination sweep (#64); payload carries `worker_id` and `last_heartbeat_ms` |
| `fq.worker.{worker_id}.invocation.archive_acked` | Control-plane → worker: archive row written; safe to delete local `invocation_state` |
| `fq.system.startup` | Runtime lifecycle — startup |
| `fq.system.shutdown` | Runtime lifecycle — shutdown |
| `fq.system.task_failed` | A hosted task inside `fq run` exited with an error |
| `fq.system.recovery` | Daemon-startup snapshot of in-flight invocation categorisation |

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
- **`config_snapshot` is a full capture.** This is what makes replay meaningful — if the agent definition is later changed, the trace still shows exactly what was running.
- **`trigger_source` indicates who initiated.** `manual`, `subject`, or `schedule`.
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
- **Full message history is sent every time.** Reconstructing context from earlier events would be fragile.
- **`tools_available` is a snapshot per call.** Tool schemas can change between calls.
- **`call_id` correlates with the response.**

### `llm.dispatched`

WAL middle-state event for LLM calls. Emitted between `llm.request` and `llm.response` once the request has returned control to the runtime, before the response is durably written.

```json
{
  "call_id": "llm-01HXJ...",
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
- **Cost rides on the envelope.** See [Cost metadata](#cost-metadata) above. Consumers query `WHERE event_type = 'llm_response' AND total_cost IS NOT NULL` for cost-bearing events.
- **`tool_call_id` is assigned by the LLM.**
- **`usage` carries raw token counts**, mirrored in `envelope.cost` along with the computed dollar values.

### `tool.call`

Published when the agent invokes a tool. Each tool call in a single LLM response produces its own `tool.call` event.

```json
{
  "tool_call_id": "tool-01HXJ...",
  "tool_name": "read",
  "parameters": { "path": "/project/docs/overview.md" }
}
```

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

`error_kind` values: `sandbox_violation`, `invalid_parameters`, `execution_failed`, `timeout`, `permission_denied`.

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
- **Operator-triage event.** The control-plane consumes this and surfaces the case via `fq recover` (a follow-up step in the data-architecture-v1 plan); the github-watcher treats it as a failed, operator-attention outcome.
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
when absent (pre-#125 events, and runs that never declare). Orthogonal
to the runtime axis: `failed` events with a `FailureKind` model runtime
failure; `task_status` models "the runtime worked — was the goal
achieved?". Declared via the terminal `report_outcome` tool, which the
reducer harness intercepts as the terminal transition (never
dispatched); a turn ending with no tool calls declares `success`
implicitly.

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
- **Derived, not authoritative.** The projection maintains `invocation_summary` (current line per invocation, last write wins). A reprojection replays these events; the LLM is never re-called.
- **The summariser never writes into the invocation.** No WAL row, no conversation message — the reducer's resume/drain equivalence is untouched by construction.

### `system.startup`

```json
{
  "runtime_id": "01HXJ...",
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
  "runtime_id": "01HXJ...",
  "reason": "ctrl_c | task_failed | error",
  "clean": true
}
```

### `system.task_failed`

A hosted task inside `fq run` (the projection consumer, the trigger dispatcher, the coordination consumer, etc.) exited with an error before a graceful shutdown was requested. The daemon then shuts itself down so operators don't unknowingly rely on a half-broken daemon.

```json
{
  "runtime_id": "01HXJ...",
  "task_name": "coordination_consumer",
  "error_message": "..."
}
```

### `system.recovery`

Emitted once per daemon startup with the counts of in-flight invocations classified by recovery category (see `docs/design/committed/data-architecture.md` §7.1).

```json
{
  "runtime_id": "01HXJ...",
  "worker_id": "worker-001",
  "safe_resume": 2,
  "safe_replay": 0,
  "ambiguous": 1,
  "total": 3
}
```

## Invariants

The following invariants hold across the event stream and are assumed by consumers:

1. **Events within one `invocation_id` are totally ordered** by the envelope chain. Sorting by `event_id` (UUID v7 is time-sortable) is a good fallback; following `parent_event_id` is authoritative.
2. **Every invocation starts with a `triggered` event** and ends with either `completed` or `failed`. The `triggered` event is the chain root (`parent_event_id` absent).
3. **Every `llm.request` is followed by `llm.dispatched` then `llm.response`** in the reducer path. The legacy executor path skips `llm.dispatched` (no WAL).
4. **Every `tool.call` is followed by `tool.dispatched` then `tool.result`** in the reducer path. Tool failures surface as `tool.result` with `is_error: true`, not as missing results.
5. **`envelope.cost` is present on `llm.response`** events that bill. There is no separate cost event.
6. **`config_snapshot` in `triggered` is immutable for the invocation.** Config changes during an invocation are ignored; they apply to the next invocation.
7. **`invocation.ambiguous` is emitted by the worker on startup** for any invocation whose WAL classification returns "ambiguous" — or whose automatic resume fails (`stuck_entity: "recovery"`). It fires at most once per invocation across restarts (the worker store's `ambiguous_reported_at` stamp). The chain root for that emission is the new event itself (`parent_event_id` absent — recovery starts a fresh chain; see the recovery rationale in `data-architecture.md` §3.4).
8. **`invocation.archived` immediately follows the terminal lifecycle event** (`completed` or `failed`) in the same invocation chain. The worker's retry sweeper may republish `invocation.archived` if the control-plane ack does not arrive; republishes keep the same `invocation_id` and the control-plane's insert is idempotent on it. `invocation.archive_acked` is the control-plane's reply on the worker-scoped subject and closes the hand-off.
9. **`invocation.operator_recovered` is operator-initiated** and rooted on its own envelope (the operator's `fq` process is not the original worker, so the chain is fresh). Terminal status set by this event is sticky — the coordination consumer's `invocation.archived` handler will not downgrade an already-terminal owner status if a still-alive worker emits `archived` after the operator's drop.
10. **`worker.orphaned` fires exactly once per alive→stale transition** — the coordination sweep's conditional store update consumes the transition, and a publish failure after that is logged, not retried (at-most-once; the stale row remains visible via `fq workers list --stale-only`).

## Storage and Retention

- All events are published to JetStream streams with file-based persistence.
- Retention policy is `LimitsPolicy` with a configurable `MaxAge` (default: 30 days).
- Events are projected into SQLite for complex queries.
- The projection store is a read-optimised view, not the source of truth. Events can be re-projected from the NATS stream at any time.

### Retention and the trail's lifetime

The event trail has no payload-bearing system of record beyond JetStream retention. The projection is a long-lived but lossy, derived view, while the CAS archive preserves invocation outcomes rather than their event trail.

| Surface | Lifetime | What survives and record status |
|---|---|---|
| NATS `fq-events` | 30 days by default | Complete payload-bearing event trail; deleted after retention. Trigger and advisory streams retain messages for 24 hours by default. |
| SQLite projection (`events`) | 30 days by default (`[state].retention_days`); cost-bearing rows kept indefinitely | Typed columns only, without event payloads. The daemon prunes it on the scheduled retention sweep, except rows carrying `total_cost` (`llm_response`, `invocation_summary`) — cost accounting is a primary platform concern and spend figures must survive retention. |
| CAS archive (`fq-cas`) | Archive retention policy | Invocation final-state blobs only, not the event trail. ADR-0026 designates these invocation outcomes as the system of record. |

After a projection sweep, replaying the retained NATS stream is the supported recovery path and can recover only events still inside JetStream retention. Events older than stream retention are gone by design; the projection intentionally does not preserve typed rows past that boundary. ADR-0026's system-of-record guarantee covers invocation outcomes, not the trail. A stronger re-projection guarantee is tracked in [#139](https://github.com/bricef/factor-q/issues/139) and [#163](https://github.com/bricef/factor-q/issues/163).

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
