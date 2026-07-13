# Backlog

Deferred work that isn't committed to a specific phase yet. Items
accumulate here when they're flagged during design or implementation
but aren't important enough to block a phase closing.

Ordering within a section is **priority-ish** — the things at the
top are more likely to be pulled into the next phase. Nothing in
here should be assumed scheduled.

## Deferred from phase 1

### File-watcher variant of agent reload
**Source:** phase 1 plan (`docs/plans/closed/2026-04-02-phase-1-foundation.md`), stretch goals section.

`fq reload` has shipped: it reloads definitions through a control message
and refreshes them between invocations. This item tracks only an optional
file-watcher variant that would invoke that shipped reload path after agent
definitions change.

### Second LLM provider wired end-to-end
**Source:** phase 1 plan, stretch goals section.

`LlmClient` is a trait and `genai` supports Anthropic, OpenAI,
Gemini, Ollama, xAI, and others. Structurally we can target any
of them. In practice, only Anthropic is exercised end-to-end, and
the pricing lookup only has examples tested against Anthropic
model IDs.

Work needed:
- Verify the genai adapter handles provider-specific quirks for at
  least one non-Anthropic target (OpenAI is the obvious choice)
- Update the sample agents with a second-provider variant
- Add a smoke test that exercises the second provider behind an
  opt-in env var (e.g. `OPENAI_API_KEY`)
- Document the provider-pinning story in the agent definition docs

Mostly mechanical. Real value is small until we need per-call
model routing in a multi-agent system.

## Known gaps flagged during phase 1 design

### Scheduled refresh of pricing data (internal job scheduler)
**Source:** `docs/plans/closed/2026-04-02-phase-1-foundation.md` deferred-work section, and `docs/design/committed/storage-and-scaling.md`.

Pricing is fetched once at `fq run` start from the LiteLLM JSON.
Continuously running deployments will keep stale pricing
indefinitely, which is fine operationally but produces drift over
weeks.

The right place to fix this is a general internal job scheduler
that can run both pricing refreshes and user-facing scheduled
agent triggers. See the phase 1 plan's "Scheduled refresh of
pricing data" section for full rationale.

Related future work: scheduled agent triggers themselves are in
the design space but not yet scoped.

### Container-level sandboxing (ADR-0010 — decided)
**Source:** [`docs/adrs/accepted/0010-agent-execution-isolation.md`](../adrs/accepted/0010-agent-execution-isolation.md) and the shell tool known-gaps section.

ADR-0010 is now accepted: containers by default, with a Kata +
Firecracker microVM upgrade path. A network proxy at the container
boundary enforces `sandbox.network` patterns, enables shadow mode,
and provides audit logging.

Implementation work remaining:
- Container image build pipeline for agent workloads
- Runtime integration to launch agents in containers
- Network proxy component (enforces network policy, records traffic)
- Kata + Firecracker support (deferred until trust/compliance demands it)

### Continuous learning
**Source:** `ARCHITECTURE.md` subsystems section.

Agent performance should be reviewed and instructions, prompts,
and workflows updated based on outcomes. The event bus provides
the raw material — full traces of actions and results — but the
learning loop itself does not exist. This was identified as a
core subsystem during the vision/architecture phase but is
entirely future work.

This belongs in a later phase once we have stable multi-agent
operation and real usage data to learn from.

### Agent concurrency primitives
**Source:** design discussion, April 2026.

The runtime currently executes one agent per trigger, serially.
Multi-agent workflows require concurrency primitives that agents
can use to coordinate work:

- **Spawn** — launch a sub-agent and continue without waiting
- **Join** — wait for one or more sub-agents to complete
- **Pipeline** — chain agents sequentially (output of A becomes
  input of B)
- **Fan-out / fan-in** — spawn N agents in parallel, aggregate
  their results
- **Fire and forget** — launch a sub-agent with no expectation of
  a result

These should be exposed as tools so agents can compose sub-agent
flows dynamically. The orchestrator defines the static graph;
individual agents define dynamic sub-flows at runtime using these
primitives.

Key design questions:
- Where does scheduling live — in the runtime, or as a tool the
  agent calls that publishes triggers to NATS?
- How are concurrency limits enforced (max concurrent agents,
  memory pressure)?
- How does budget propagate — does a parent's budget cover its
  children, or are they independent? **(resolved below)**

**Decided 2026-05-28** (surfaced in the MCP prompt-as-subagent
discussion — see `docs/plans/closed/2026-05-28-mcp-client-full-spec.md`
Step 4):

- **The parent owns the child's definition.** A subagent is
  spawned from a definition the parent supplies; an MCP server
  declaration may pin a default definition for prompt-seeded
  spawns. The agent's LLM may *choose* to spawn, but it does not
  author privileges — it can only select from what the parent
  already holds.
- **Default definition: "same as me."** The common case is
  delegating to a clone, so the parent's own definition is the
  default child definition — same model, tools, MCP access, and
  capability grants. Safe by construction: cloning cannot
  escalate (the child's capability set equals the parent's, so
  ⊆ holds trivially). Budget is the one field *not* cloned —
  it's consumable, so it's apportioned per spawn (a sensible
  default for the spawn-and-wait case is the parent's remaining
  budget). An ergonomics shortcut, expected to dominate in
  practice.
- **Capabilities attenuate: child set ⊆ parent set**, across
  every dimension — tools, MCP server access, the ADR-0017
  sampling/elicitation/roots grants, and sandbox
  (network/filesystem/env). A child can never hold a capability
  its parent lacks. The relation is transitive, so any
  descendant's capabilities ⊆ the root's.
- **A parent's budget bounds its whole subtree's spend** —
  resolving the budget question above: children are *covered by*
  the parent, not independent. Parent spend + all descendants'
  spend never exceeds the parent's budget (recursively, the whole
  spawn tree ≤ the root budget). A per-child cap of "≤ parent's
  remaining" alone does **not** bound a fan-out (N children each
  capped at the parent's remaining could collectively spend N×).
  Now stated as the authoritative rule in
  [ADR-0004](../adrs/accepted/0004-cost-controls-from-day-one.md).
  The *enforcement mechanism* — reservation/escrow vs. the
  aggregate-and-halt "Inheritance rule" in
  `docs/design/aspirational/agent-orchestration-tools.md` — is an open choice
  deferred to spawn-build time. The ADR-0017 sampling sub-budget
  is one line item inside the same bound.
- **Enforced by the runtime at spawn time, not by the LLM.** The
  runtime rejects a child definition requesting budget or
  capabilities exceeding the parent's — mirroring ADR-0017's
  "the runtime is the only gate" principle.
- **The spawn carries a typed deliverable:
  `spawn(definition, seed, [OutputType])`.** A subagent is a
  typed function, so its result must conform to a declared
  schema — an untyped result would be the free-form API ADR-0016
  forbids. **Rule: the deliverable must be typed at one end** —
  either the agent definition's signature (`deliverable_schema`)
  or the call-site `OutputType`. A specialized agent inherits its
  definition's output; a generic / "same as me" clone is typed by
  the caller — the dynamic analog of a typed graph edge,
  `spawn::<T>(definition, seed) -> T`. If neither end types it,
  that is a definition error, not a free-form escape hatch — so
  `OutputType` is most load-bearing for the "same as me" case
  above.
- **Deliverable validation is harness-enforced, reusing existing
  machinery.** The harness validates the subagent's deliverable
  against the effective type; non-conformance drives the
  retry-as-feedback loop (inter-node-contracts §3 /
  storage-taxonomy §9) so the subagent self-corrects, and after N
  failures it surfaces as a typed spawn failure the parent
  handles. `OutputType` composes with the size-aware placement
  wrappers — `Promoted<T>` auto-routes a large deliverable to the
  artifact store so the parent receives an `ArtifactRef`.
  Fire-and-forget spawns carry no `OutputType`; only the
  result-bearing variants (Join / Pipeline / Fan-in) do.
  Exploratory agents still type their deliverable, with a looser
  schema.

Still open: where scheduling lives, and how concurrency limits
(max concurrent agents) are enforced — likely the same treatment
as budget.

Related: the task engine subsystem in `ARCHITECTURE.md` which
owns fan-out/fan-in and dependency graphs.

### External trigger adapters (input hooks)
**Source:** design discussion, April 2026.

The NATS trigger system (`fq.trigger.<agent>`) provides the
inbound port for agent execution, but there are no adapters
connecting external systems to it. Real-world use requires
triggers from:

- GitHub (issues opened, PRs created, reviews requested,
  CI status changes)
- Slack (messages in channels, DMs, mentions)
- Email (inbound messages to a monitored address)
- Webhooks (generic HTTP → NATS bridge)
- SMS / Telegram / other messaging platforms

Each adapter is a small service that listens to an external
system and publishes structured trigger messages to NATS. The
agent definition's `trigger:` pattern matches the subject, so
agents can subscribe to specific event types (e.g.,
`trigger: github.issue.opened`).

The webhook adapter is the most general-purpose starting point —
most of the above systems support outbound webhooks. A single
HTTP-to-NATS bridge with configurable subject mapping would cover
GitHub, Slack (via Events API), and generic integrations.

### Network proxy
**Source:** ADR-0010 (agent execution isolation), design
discussion on shadow mode (April 2026), and the
tool-isolation model
(`docs/design/committed/tool-isolation-model.md`).

A network proxy sitting between tools (and the host) and
external systems, serving as the trust enforcement point.
Under the per-tool isolation model this becomes
disproportionately load-bearing — every network-touching
tool passes through it. Responsibilities:

- Enforce per-tool network allowlists
- Enforce aggregate per-agent network policy
- Inject host-managed credentials at request time (tools
  never see API keys directly)
- Record/replay for shadow mode workflow evaluation
- Audit logging of all outbound requests
- Caching of repeated identical requests
- Rate limiting against external APIs
- Trust-based access control (different allowlists per model
  trust tier)

See `docs/design/aspirational/shadow-mode-and-self-improvement.md`,
`docs/design/committed/tool-isolation-model.md`, and ADR-0010 for
full context.

### WASM-native POSIX sandbox for shell and file tools
**Source:** `docs/design/aspirational/wasm-posix-sandbox.md`, design
discussion April 2026.

Investigate compiling a POSIX utility bundle (BusyBox, Rust
uutils + shell, or similar) to `wasm32-wasi-preview2` and
running it under wasmtime as the isolation tier for the
`shell` tool and related filesystem operations. Gives
container-like isolation strength with subprocess-like
startup cost, plus clean composition with workspace
snapshotting via overlay filesystems.

Not urgent. Becomes interesting after the container-based
shell tier is working and can be used as a comparison
baseline. Also gated on WASI ecosystem maturity — the
design doc lists concrete "what would need to be true"
conditions.

Investigation shape:
- Survey current WASM+WASI POSIX-toolkit projects
  (BusyBox-in-WASI, uutils, wasi-shell, wasix)
- Measure actual instantiation overhead and POSIX coverage
- Prototype an overlay-filesystem shell tool and compare
  behaviour and performance against the container tier

### Agent tool catalogue design
**Source:** Design discussion, April 2026. Related to
`docs/design/committed/tool-isolation-model.md`.

The set of tools the runtime provides (beyond the phase-1
`file_read` / `file_write` / `shell`) directly determines
what models can usefully do. Models are trained on unix-like
environments — pre-existing patterns like Claude Code's
`Read` / `Write` / `Edit` / `Bash` / `Glob` / `Grep` /
`WebFetch` set a useful benchmark.

Design questions:
- Which tools are first-party vs. MCP-supplied
- Granularity (one big `shell` vs. many specific tools)
- How closely to mirror Claude Code / Cursor conventions
- Which tools fall away when workspaces are pre-loaded
  (filesystem-as-interface reduces tool proliferation)
- Each tool's isolation tier (from the tool-isolation
  model)
- Each tool's cost schedule (for budget tracking)

Not urgent in the strict sense, but becomes concrete as the
reducer-model harness lands and the first real multi-tool
agents need to do useful work.

### Workspace state: snapshotting and base layers
**Source:** Design discussion, April 2026. Related to
`docs/design/committed/tool-isolation-model.md`.

The tool-isolation model introduces per-agent workspaces as
the third tier of invocation state (alongside harness state
and external state). The workspace is the filesystem the
agent's tools operate on, owned by the invocation,
snapshotable for suspension and migration.

Open design questions:
- Snapshot mechanism: OverlayFS vs CRIU vs git-backed vs
  stable paths. Starting position is stable paths for
  phase 2; overlay is the likely phase-3 target.
- Base-layer format: container images, tarballs, git refs,
  or a factor-q-native format. Each has tradeoffs around
  caching, distribution, and layering.
- Workspace lifecycle: per-invocation ephemeral vs.
  per-agent persistent vs. configurable.
- Pre-loading story: how agent definitions declare their
  required data/context and how that gets baked into the
  base layer.
- Migration protocol: transferring a workspace snapshot
  across nodes.

This is a real design doc, not just a backlog entry.
Probably blocks any serious multi-node work but is not
needed for the reducer-model prototype (which uses stable
paths on a single host).

## Known gaps flagged during phase 1 implementation

### Agent env allowlist plumbed through ToolSandbox
**Source:** inline comment in `fq-tools/src/builtin/shell.rs`,
`allowed_env_vars` function.

The agent definition can declare an env var allowlist via
`sandbox.env: [HOME, PATH]`. The declaration is parsed, round
trips through the Agent builder, and is included in the Triggered
event's ConfigSnapshot. But it is **not** currently plumbed
through `ToolSandbox` into the shell tool's child process env.
The shell tool's child env is hardcoded to a minimal baseline
(just PATH).

Work needed:
- Extend `ToolSandbox` with an `env_allowlist: Vec<String>` field
- Update `agent::Sandbox::to_tool_sandbox` to populate it
- Update `ShellTool::execute` to layer the allowlisted vars on top
  of the default baseline at child spawn time
- Add a test that an agent declaring `env: [HOME]` actually sees
  `HOME` in `env` tool output

Small, straightforward. Moved to backlog because the shell tool
works fine for the current use cases without it.

### Multi-runtime / horizontal scaling of dispatchers
**Source:** `fq-runtime/src/bus.rs`, trigger stream design comment.

The trigger stream uses `Limits` retention (not `WorkQueue`)
because NATS disallows overlapping consumer filters on work-queue
streams, which breaks parallel test isolation. For phase 1
single-runtime this is fine — triggers age out within 24h and
the single dispatcher consumes them within seconds.

For multi-runtime horizontal scaling (multiple `fq run` instances
sharing a NATS), we need:
- Some form of work distribution that guarantees each trigger is
  handled by exactly one instance
- Either per-instance consumer names (each sees every trigger,
  each decides independently whether to handle it — risky), or
  a queue-group pattern on top of Limits, or revisiting the
  workqueue decision with a test-isolation story

Not needed until we have multi-runtime deployments to worry about.

## Observability (deferred from initial floor)

The initial observability floor shipped system lifecycle events
(`system.startup`, `system.shutdown`, `system.task_failed`),
immediate task-failure surfacing in `fq run`, and `fq status` for
runtime health checks. The following items were scoped but
deferred:

### JSON structured log output
The `tracing` subscriber uses default human-readable ANSI output.
Production deployments behind a log aggregation pipeline (ELK,
Loki, Datadog) need a JSON output format. The `tracing-subscriber`
crate supports `fmt::layer().json()` — a small configuration
change gated by a CLI flag (`--log-format json`) or config field.

### Prometheus / OpenTelemetry metrics
Exporting runtime metrics (invocations/sec, tool calls/sec, error
rates, cost/sec, projection lag) to Prometheus or an OTLP
collector. This is a real project — needs a metrics registry, an
HTTP scrape endpoint, and decisions about cardinality (per-agent
vs aggregate). Defer to a dedicated observability phase.

### Consumer lag alerting
`fq status` reports lag, but does not alert when lag grows. Future
work: a health check endpoint that external monitors (Prometheus
alerts, Grafana, etc.) can poll, or an internal watchdog that
emits a `system.lag_warning` event when the projection falls
behind by more than N events.

### Agent throughput and error-rate aggregation via CLI
`fq costs` aggregates cost. A companion `fq stats` could aggregate
invocation counts, error rates, and mean duration per agent per
time window — similar data, different dimension. Easy to build as
another SQLite query once the need arises.

### MCP server logs and progress on the operator surface
**Source:** MCP-completion plan
([`closed/2026-06-04-mcp-completion.md`](closed/2026-06-04-mcp-completion.md)),
steps B1/B2. B1/B2 shipped (the drain + the `mcp.log` bus event); the
richer operator-facing surfacing is what remains here.

Connected MCP servers emit `notifications/message` (logs) and
`notifications/progress`. The MCP-completion plan drains the
`ServerNotification` channel in the daemon and bridges server logs onto
the event bus (a new `EventPayload` variant). The **operator-facing**
half — surfacing those logs and in-flight progress through the operator
CLI / event consumer, alongside cost and invocation status — belongs to
this broader observability effort and is captured here so it is not lost
when the MCP plan closes. The MCP plan wires the minimal surface (emit +
a basic operator readout); a richer operator UX (filtering, per-server
log levels, progress display) lands with this work.

## Reducer boundary invariants (flagged 2026-05-15)

> **Update (2026-07-05):** the "Round-trip invariants on
> `HarnessState`" thread below is absorbed as slice 2 of the
> [reducer verification plan](closed/2026-07-05-reducer-verification.md)
> — **and shipped the same day** (`HarnessState::validate` at both
> persistence boundaries, with property tests). The other two threads
> are unchanged.

A design pass on validation at the reducer host/guest boundary
during the envelope-refactor work surfaced three threads. The
first two are worth doing now; the third is deferred to the
graph-executor work that will need it for its own reasons.

See `docs/design/aspirational/inter-node-contracts-and-event-layers.md` §2
("validation runs at both ends") for the longer-term picture.
The items below are the *cheap* pieces that pay off before any
graph executor exists.

### Stronger types at the reducer boundary

**Source:** discussion 2026-05-15 (envelope-refactor closeout).

Status update (2026-05-16): **partly landed.** Original list
of four candidates re-evaluated through implementation; the
useful subset shipped, the rest were retired with rationale.

Done:
- **`AgentId(String)`** — promoted from a parse-time wrapper to
  the actual type on `Envelope::agent_id` and
  `AgentConfig::agent_id`, with NATS-subject-safety validation
  at the wire boundary (deserialise runs the same predicate as
  construction). Commit `ce16740`.
- **`ToolCallId(String)`** — wraps the correlation key across
  events, the WAL, and the reducer types. Validation: non-empty
  only (providers vary in shape, so we don't enforce one).
  Commit `11ed9c3`.

Retired (won't do, with reasons preserved for future context):
- **`MaxIterations(NonZeroU32)`** — the framing turned out to
  be wrong. `0` is a *meaningful* value (the natural reading is
  "stop signal — no LLM turns allowed"), so wrapping in
  `NonZeroU32` would forbid an expressive case. The real bug
  was the runtime's sentinel-zero hack treating `0` as "use
  default" — fixed separately by removing
  `effective_max_iterations` and making producers pass
  `DEFAULT_MAX_ITERATIONS` explicitly. The field stays `u32`
  with literal semantics. A rich type may earn its keep at the
  agent-definition parser boundary later (see "Successor:
  rich MaxIterationsConfig at the agent-definition boundary"
  below).
- **`StepIndex` with monotonicity constructor** — the existing
  `for step_index in step_index_start..HOST_STEP_BUDGET` loop
  in the reducer runner already enforces monotonicity at the
  type level (it's a `Range<u32>` iterator). Wrapping the
  primitive doesn't catch a bug class the loop doesn't already
  prevent.

Out of scope going forward: `InvocationId`, `EventId`,
`TraceId`. Already `Uuid`, which is strong enough for the bug
classes they participate in.

#### Successor: rich MaxIterationsConfig at the agent-definition boundary

When agent definitions gain a `max_iterations` field (likely
soon — the markdown frontmatter parser has an obvious slot for
it), introduce a richer type *at the parser boundary*:

```rust
pub enum MaxIterationsConfig {
    Default,                       // resolve to DEFAULT_MAX_ITERATIONS
    Stop,                          // 0 — agent explicitly disabled
    Explicit(NonZeroU32),          // an explicit positive cap
}

impl MaxIterationsConfig {
    pub fn resolve(self, default: NonZeroU32) -> u32 { ... }
}
```

The resolved `u32` flows into `AgentConfig::max_iterations`;
the runtime hot path never sees the variants. The enum earns
its keep because (a) it lets the parser distinguish "user
explicitly disabled this agent" from "user didn't set anything"
— useful for observability and for `fq invocation list`
filtering, and (b) the construction shape prevents
`Iterations(0)` at compile time.

Don't do it before the parser slot exists. Doing it now means
designing for an audience that doesn't consume the
distinctions.

### Round-trip invariants on `HarnessState`

**Source:** discussion 2026-05-15. See
`services/fq-runtime/crates/fq-runtime/src/worker/reducer/harness.rs:88`
(`HarnessState::load` / `save`).

The opaque state blob crosses the host/guest boundary as
`Vec<u8>` and is deserialised by `HarnessState::load`. Serde
catches structural malformation; nothing today catches
semantic violations of the state machine — e.g.
`phase == AwaitingModel` with empty `messages`, or
`phase == DispatchingTools` with no pending tool calls in the
last assistant message.

Work needed:
- Add `HarnessState::validate(&self) -> Result<(), HarnessError>`
  with the phase ↔ contents invariants the state machine
  actually enforces (write them out by reading
  `initial_step` / `model_response_step` / `tool_results_step`
  in `harness.rs`).
- Call it from `load` (after deserialise) and `save` (before
  serialise). Both are the right boundary — `load` catches a
  corrupt or stale persisted state; `save` catches a reducer
  bug that produced an inconsistent state in-memory.
- Surface as `HarnessErrorKind::InternalError` (existing
  variant) with a message naming which invariant failed.

Specific invariants to encode (non-exhaustive — add as they
surface):
- `phase == Initial` ⇒ `messages.is_empty()`.
- `phase == AwaitingModel` ⇒ `!messages.is_empty()` and the
  last message is `System` or `User` or `Tool`.
- `phase == DispatchingTools` ⇒ the last message is
  `Assistant` and carries non-empty `tool_calls`.
- `iteration >= max(message-count-implied-iterations)` (rough
  check — exact form depends on how iteration is incremented).

This is the validation hook the longer design conversation
was reaching for, in its cheap and concrete form: a single
function with a small set of invariants, on the one boundary
that actually has an opaque payload to validate. Future graph-
executor work can grow the *system-wide* validation story
without touching this.

### Tool-parameter schema validation against `parameters_schema`

**Source:** discussion 2026-05-15. Related: design doc §2
("validation runs at both ends") in
`docs/design/aspirational/inter-node-contracts-and-event-layers.md`,
ADR-0016 (typed operations).

When the LLM produces a tool call, the runtime today executes
it without validating `parameters` against the tool's declared
`parameters_schema`. The LLM provider does some validation
upstream; each tool's `execute` impl catches gross errors via
its own `serde_json::from_value` parse. The gap is the middle
ground: syntactically valid JSON that matches the wrong
fields.

**Deferred** until either:
- The bug class shows up in practice (frontier models
  produce malformed-but-syntactically-valid tool calls
  effectively never against schemas the provider has seen),
  or
- The graph-executor work begins, where typed contracts
  between graph nodes become load-bearing. At that point,
  schema-validation infrastructure (`jsonschema` crate,
  schemas attached to graph-node declarations, retry-as-
  feedback semantics) lands as a system, and tool-parameter
  validation falls out as one site among many that uses it.

Doing it standalone now would mean picking the validator
crate, building schemas that today are mostly hand-written
placeholders, and designing the retry semantics — for a bug
class that hasn't been observed. The cost is real; the value
is marginal until one of the triggers above fires.

## Data-architecture follow-ups (flagged 2026-05-16)

### Stuck-invocation detection

**Source:** discussion 2026-05-16 (worker heartbeat design).
Related: `docs/plans/closed/2026-04-28-data-architecture-v1.md`
steps 5 (state persisted at every step boundary) and 7
(control-plane recovery).

The worker heartbeat we're about to land covers
**worker-process liveness**. A separate concern is
**per-invocation liveness**: is *this specific* harness
making progress, or has it wedged inside a tool call, a model
call, or a tight loop? The two are different — a healthy
worker can host a wedged invocation.

The good news: a per-invocation heartbeat event is *not*
needed. The data-architecture-v1 step 5 work persists
`invocation_state.updated_at` at every reducer step
boundary. That column is already a "last activity" timestamp
per invocation. Plus every emitted event for an invocation
carries an envelope timestamp.

So the detection is a periodic sweep:

```sql
SELECT invocation_id, agent_id, phase, updated_at
  FROM invocation_state
 WHERE terminal_at IS NULL
   AND updated_at < (now_ms - stuck_threshold_ms)
```

Each offender gets an `invocation.stuck` event (new payload
variant, similar shape to `invocation.ambiguous` but with
`last_progress_ms` instead of `stuck_entity`/`stuck_call_id`).
The operator triages via `fq recover` alongside ambiguous
cases.

Open design questions:

- **Threshold.** A flat threshold is too coarse — the
  TradingAgents reference workload (minute-scale invocations)
  would be flagged stuck on a normal LLM turn under any
  threshold tight enough to catch real wedges. Probably needs
  to be per-agent (agent definition declares its expected
  step latency) with a generous default. Don't ship with a
  flat 60s default; that's a foot-gun.
- **Where the sweep lives.** Natural home is the control-plane
  side, alongside the existing stale-worker sweep in
  `coordination_consumer.rs`. Same cadence, same shape.
- **What "stuck" does next.** Emit-and-surface for operator
  triage is the minimum. A more aggressive option is to
  attempt a recovery (re-emit intent? kill the worker? mark
  ambiguous?). Defer the "what to do" question until we have
  a real stuck-invocation example to reason from.

**Cost:** small (~80 lines). The new event type, the sweep
function, the consumer arm (or reuse coordination consumer's
existing infrastructure). Should land after the heartbeat
work and likely after step 8 (archive hand-off), so the
"stuck" event has all the context it needs to be useful.

**Deferred** until either:
- A real stuck-invocation example shows up in practice and
  forces the threshold decision, or
- Step 9's `fq recover` CLI is being built and the stuck
  case fits naturally alongside ambiguous in its UI.

## MCP client gaps (flagged 2026-05-31)

### Audio content in MCP prompt messages (blocked on rmcp)
**Source:** MCP full-spec plan Step 4
(`docs/plans/closed/2026-05-28-mcp-client-full-spec.md`);
`crate::prompt::PromptContent::Audio`,
`crate::mcp::prompt_content_from_rmcp`.

factor-q's `PromptContent` is a 1:1 capture of the MCP 2025-11-25
`ContentBlock` union, including `Audio`. But `rmcp`'s
`PromptMessageContent` omits the `Audio` variant (through 1.7), and
because it is an internally-tagged enum with no catch-all, a
spec-conformant `{"type":"audio",...}` prompt content block fails to
deserialize inside `get_prompt` — so audio prompt content never
reaches our capture layer (the fetch errors first).

**Status: reported and fixed upstream** —
- issue: `modelcontextprotocol/rust-sdk#864`
- PR: `modelcontextprotocol/rust-sdk#865` (adds the `Audio` variant +
  `new_audio` + schema snapshot)

When that PR merges and we pick up the release, the `Audio` arm in
`crate::prompt` becomes reachable with **no factor-q change required**;
the handler stub (`PromptContent::to_text` →
`NotImplemented("audio")`) is what we'd implement if/when audio
prompts need rendering. The unit test
`prompt::tests::capture_round_trips_losslessly_for_all_variants`
already exercises the owned `Audio` type end-to-end.

The sibling embedded-resource gap (`rust-sdk#842` / `#843`) was
already fixed upstream and is resolved here by the rmcp 1.4 → 1.7
bump (it unblocked the `resource-prompt` integration tests).

## MCP full-spec follow-ups (flagged 2026-06-01)

**Source:** Steps 5–7 of the MCP full-spec plan
(`docs/plans/closed/2026-05-28-mcp-client-full-spec.md`); commits
`f85a965` (sampling), `c8eaa4c` / `4bfec85` (roots / elicitation),
`f18d1e1` / `4a646d7` / `b5bcd02` (utilities). Each capability shipped
as a *mechanism + grant + tests*; these are the pieces deliberately
left for later. The grant-parsing + daemon-enforcement wiring lives in
**Step 8** of that plan; the items below are polish / robustness /
feature-gated work that does not block the plan closing.

**Status (2026-06-12): the in-plan items shipped.** The dedicated
completion plan
[`2026-06-04-mcp-completion.md`](closed/2026-06-04-mcp-completion.md)
(now **closed**) took the in-scope items to done — validation
(incl. **config-driven validators**), `origin` on the trace,
multi-server channel, daemon notification drain + logs→bus, and the
paginating/mutating mock — and added **Streamable HTTP transport** for
full 2025-11-25 spec compliance. The **[in plan]** bullets below are
therefore **done**; the **[deferred]** ones remain open: `includeContext`
injection + inbound redact chain, roots non-`file://` schemes, the
mid-invocation hot-swap + cancellation trigger (ADR-0020), and
audio-in-prompts (under *MCP client gaps* above, blocked on rmcp).

- [ ] **[in plan] `origin` on the `llm.request` / `llm.response` trace.** Today
      `LlmCallOrigin` (agent-turn vs `sampling` / `elicitation{server}`)
      rides only the **cost** event (`CostMetadata`) +
      `InvocationTotals.{sampling,elicitation}_cost`. ADR-0018
      envisioned it on request/response too; they correlate by
      `call_id` for now. `LlmCallOrigin` is public — spreading it is a
      few struct fields + construction sites.
- [ ] **[in plan] Multi-server server-initiated channel.** `run_with_server_requests`
      takes a single `SamplingChannel`; servicing several grant-bearing
      servers in one invocation needs a merged, server-tagged stream
      (the `select!` arm + gate already key on the server name).
- [ ] **[deferred] `includeContext` injection + inbound redact chain.** Sampling
      forces `includeContext: none` (no agent/MCP context is injected
      into a server's prompt yet). When context injection lands, honor
      `thisServer` / `allServers` through the inbound validate-and-redact
      chain (ADR-0018 §4).
- [ ] **[in plan] Elicitation schema-validation depth.** v1 enforces object shape
      + required-field presence (`validate_against_elicitation_schema`).
      Add per-field primitive / format (email, uri, date) / enum /
      numeric-range checks, and reject schemas outside MCP's flat-object
      subset rather than passing them through.
- [ ] **[in plan] Extract the reusable "structured completion against a schema"
      primitive.** The validate → bounded-retry → decline loop is inline
      in `handle_elicitation`; ADR-0018 wants it as a shared runner
      primitive that the sampling evaluator-validator and the
      spawn-deliverable typing (agent-concurrency backlog item) reuse.
- [ ] **[in plan] Concrete validators for the validation seam.** The sampling
      outbound, elicitation inbound/outbound, and roots outbound
      `ValidatorChain`s ship default-allow. Implement the policy-surface
      validators: `HighEntropyRedactor`, `ValidateRequestPolicy`, and the
      sampling evaluator-validator.
- [ ] **[in plan] Server logs → event bus.** `on_logging_message` folds records
      into `tracing` + the `ServerNotification::Log` sink; the plan's
      "/ the event bus" half — a `Logging` event payload bridged onto the
      bus (needs an `EventPayload` variant + `schema_id_for` entry) — is
      unbuilt.
- [ ] **[in plan] Daemon notification→action loops.** Nothing drains the
      `ServerNotification` channel in the running daemon yet: react to
      `ToolListChanged` by calling `refresh_tools` *and re-registering
      into the live `ToolRegistry`* (the manager refreshes its own
      `tool_names`; registry re-registration is the open piece); cancel
      in-flight calls on invocation abort (`call_tool_cancellable`
      exists; no abort trigger — timeout / budget / shutdown — is wired);
      surface progress / logs to an operator.
- [ ] **[in plan] Mock paginating / mutating MCP server (test infra).** The
      everything server doesn't paginate and only emits
      `tools/list_changed` once at startup, so a real multi-page
      pagination test and a server-driven `list_changed`→`refresh_tools`
      test can't be written against it. A small in-process or
      child-process mock would unblock both. Pagination *itself* is
      correct (cursor-following `list_all_*`); this is a coverage gap,
      not an implementation gap.
- [ ] **[deferred] Roots: non-`file://` URI schemes.** `roots_from_sandbox` emits
      `file://` only. (The dynamic-workspace `roots/list_changed`
      *trigger* — recompute roots from a changed sandbox — is owned by
      the "Workspace state: snapshotting and base layers" item above;
      only the `RootsHandle::set_roots` mechanism exists today.)

## Process and documentation gaps

### ADRs still in draft
The following ADRs are still in `docs/adrs/draft/` and should
either be accepted (with a resolution documented) or deliberately
deprecated:
- ADR-0006 API design
- ADR-0008 Extension model
- ADR-0025 Storage GC observability

ADR-0010 (agent execution isolation) has been accepted. Phase 2
(MCP, memory, skills) will likely force ADR-0008 to resolution.
ADR-0025 needs a resolution for storage lifecycle and observability.

## CI / test flakiness (flagged 2026-06-29)

### Flaky EPIPE crash in the MCP stdio integration test

**Source:** intermittent CI failure in the `Rust CI` job (`just rust-ci`,
the `fq-runtime` half), seen while landing the fq-store `object` CLI rename
(commit `7ef5fdd`); the same suite passed on the runs immediately before and
after, and a re-run went green.

An MCP integration test spawns a Node MCP server
(`@modelcontextprotocol/sdk`) over stdio (via `npx`). On teardown the test
closes the pipe while the server is mid-write, so `StdioServerTransport.send`
hits `EPIPE`. The SDK's stdio transport has no `error` handler on the socket,
so Node throws on the unhandled `'error'` event and the process exits 101,
failing the whole step.

Because `just rust-ci` runs the runtime CI **before** the store CI (and stops
on first failure), a flake here also masks the fq-store results.

**Impact:** spurious CI reds; a re-run passes. Not a product bug, but it will
keep costing CI runs and eroding signal until fixed.

**Work needed:**
- Identify the MCP stdio integration test(s) in `fq-runtime` that launch the
  Node server and reproduce the teardown race.
- Shut the MCP server down gracefully before closing its stdin/pipe (send
  EOF/close and await exit), and/or attach a server-side `error` handler that
  swallows `EPIPE` during shutdown.
- **Done** (https://github.com/bricef/factor-q/issues/38): `just rust-ci` is
  split into `runtime-ci` and `store-ci` jobs, so a runtime flake no longer
  masks the store results.
- If the root cause is upstream, pin/patch or wrap the SDK's stdio transport.



### Local NATS-gated tests flake when a daemon shares the broker

**Source:** dogfood day one (2026-07-05). With a live `fq run` daemon
on the same NATS as the test suite, the NATS-gated lib tests
(`dispatcher_executes_published_trigger`, the heartbeat-consumer and
retry-sweeper end-to-end tests) flake at a high rate: the daemon's
trigger dispatcher consumes test triggers before the test's own
dispatcher sees them (its log shows "trigger for unknown agent,
dropping" for `dispatch-test-*` ids), and its consumers add races on
shared streams. With the daemon stopped, the same gate passes cleanly
back-to-back; GitHub CI never sees this because its NATS starts fresh
and daemon-free.

**Resolved for the dogfood box (2026-07-05):** the dogfood project
now runs its own broker (`~/fq-dogfood/infra/docker-compose.yml`,
loopback-only on 4223) so the daemon and the suite never share
streams — verified by running the full gate concurrently with a live
invocation, green. The general hazard stands for anyone running a
daemon against the dev broker: per-run subject prefixes or unique
stream names per suite run would fix it structurally in the tests
themselves.

## Dogfood findings (flagged 2026-07-05)

**Source:** the first live runs of the v0 dogfood loop (the `doc-drift`
agent reviewing this repo daily; project lives outside the repo at
`~/fq-dogfood`). Real workload, real spend — each item below was
observed, not hypothesised.

### No prompt caching on the Anthropic path

**Addressed (2026-07-05, `2889c44`).** The genai adapter now marks the
system prompt and the final message as cache breakpoints on every
request, and `ModelPricing` prices uncached/read/write tokens
separately. Measured on the dogfood loop the same day: per-turn costs
flattened from an escalating $0.007→$0.084 to a steady ~$0.005–0.009,
36% off the total on a like-for-like run. Residue: the genai
system-marker quirk (own entry below) and the missing cache token
columns in the projection (see the corrected costs finding below).

### Agent definition changes require an explicit reload

Already tracked as [File-watcher variant of agent reload](#file-watcher-variant-of-agent-reload)
(§ Deferred from phase 1); the dogfood loop hit it in practice on day
one — an agent budget bump was silently ignored until `fq reload`, and
the run failed at the old ceiling. Evidence that the watcher deferral
now has a real cost: bump its priority, and until it lands,
document the restart requirement loudly in the agent-definitions guide.

### `fq status | head` panics on SIGPIPE

**Addressed (2026-07-05).** `fq` restores the default SIGPIPE
disposition for query-style commands (everything except `run` and
`trigger`, whose long-running paths must not die from a closed
stdout), so `fq status | head` now exits 141 silently like any Unix
filter. Pinned by `fq-cli/tests/sigpipe.rs`, which spawns the real
binary against a closed pipe and was verified to fail (exit 101)
against the unfixed code.

### `fq costs` filtering — finding corrected

**Corrected (2026-07-05): the filters already existed.** `fq costs`
takes `--agent` and `--since` (RFC3339 prefix), wired through
`ProjectionStore::cost_summary` and covered by store tests; both
verified working against the live dogfood projection. The original
finding was filed from a bare `fq costs` run without checking
`--help` — a process lesson for dogfood reports: verify a gap exists
before filing it (the same discipline the reducer plan applies to its
pre-registered findings).

What genuinely remains from this thread: the projection does not
store cache read/write token columns, so `fq costs` can't show cache
efficiency (hit rates, cached vs uncached spend) — relevant now that
prompt caching landed (`2889c44`). Needs a projection schema bump plus
consumer/CLI plumbing; worth doing when cost observability next gets
attention.


### genai 0.4.4 drops the cache marker on a single system message

Found while wiring prompt caching (`2889c44`): genai 0.4.4's
Anthropic adapter only renders the system prompt as cache-marked
parts when the marked index is `> 0` (`last_cache_idx` in
`adapter_impl.rs`), so the common case — exactly one system message,
marked — silently loses its `cache_control`. Harmless for us: the
final-message breakpoint covers the whole prefix, and our wire test
(`cache_control_reaches_the_wire_and_usage_round_trips`) pins the
current behaviour while accepting the fixed shape.

**No upstream PR needed** — verified against genai `main`
(2026-07-05): the cache path was rewritten with per-message
breakpoint tracking that explicitly includes system-role messages,
plus TTL variants (5m/1h/24h) and request-level placement. Resolve
by upgrading genai past 0.4.x when convenient; expect API churn
(`CacheControl` gained variants, `ChatRequest` gained request-level
cache options) and a small bonus (1h-TTL support). Our test's
accept-both assertion already tolerates the post-upgrade wire shape.

### M0 change-loop: trigger acked on completion caused a redelivery storm

Found on the M0 "close the loop" agent's first run (2026-07-06): one
trigger produced three back-to-back invocations (three duplicate
branches). Root cause — the dispatcher acked the trigger only after the
invocation *completed*, but the consumer's ack-wait is JetStream's 30s
default, so a ~100s change-and-validate invocation blew the deadline and
NATS redelivered. Invisible to the fast `doc-drift` agent (seconds,
under the ack-wait); fatal to slow agents, which are the runtime's whole
point. **Fixed** (`1efc67a`): ack on dispatch, not completion — the
reducer WAL owns in-flight durability and recovery; a discriminating
regression test pins it.

**Follow-up (open): ack-after-durable-start.** Acking on dispatch leaves
a sub-second window — a crash between the ack and the first WAL write is
a missed (re-triggerable) run, not corruption. Closing it needs the
invocation to signal "durably started" out through the `Worker` seam so
the dispatcher acks at that point. Low priority (tiny, non-corrupting
window), but the principled close.

### Agent network sandbox is declared but not enforced

Surfaced while wiring the M0 loop's GitHub step (2026-07-06): agent
definitions carry a `sandbox.network` allowlist, but it is **not
enforced** — agents have ambient network access (they can reach any
host, e.g. via `gh`/`git`). This violates
[design principle 3](../design/committed/design-principles.md)
("safe by construction, not by restriction" — capability granted exactly,
nothing ambient), and it is the exposure
[assessment §4](../design/2026-07-05-project-assessment.md) flags:
agents grow more capable and longer-lived, so unrestricted network is a
gap to close before they gain reach. Scope: enforce the declared
`network` allowlist at the sandbox boundary (the `fs_*` / `exec_cwd`
dimensions already are). Until then, treat any agent as
network-unrestricted regardless of what its definition declares.

### `bus.rs` is a growing typed facade — split transport from per-domain subjects?

Surfaced in human review of PR #3 (the `fq reload` feature the M0 loop
built, 2026-07-06). `bus.rs` mixes two roles: generic transport
(`connect`, `publish(&Event)`, `subscribe`) *and* per-domain typed
pub/sub carrying its own subject constants — `publish_trigger` +
`ALL_TRIGGERS_SUBJECT`, and now `publish_control_reload` +
`CONTROL_RELOAD_SUBJECT`. It has been quietly accumulating domain
knowledge; each new subject family adds another typed method pair.

The reload methods themselves are **correctly placed for the current
convention** — they mirror `publish_trigger` exactly, and the actual
dispatch (the listener loop + `reload_agents` registry swap) lives in the
control plane, not the bus. So this is not a PR-3 defect; the change
followed the strongest local precedent, and the transport-vs-handling
split already partly holds. But the review raised the latent question:
should the typed facade be split into a generic transport plus per-domain
subject/pub-sub modules (a `subjects` registry, or methods hanging off
each consumer)? That is a deliberate, codebase-level call to make across
triggers, events, *and* control at once — not piecemeal on one PR. Low
priority (tidiness / layering; no functional impact), but the file's
responsibility is worth bounding before it grows further.

### Missing cost-control layer for external / automatic triggers

Surfaced designing the GitHub issue-watcher (2026-07-07). Once a trigger
fires from an external event — an issue labelled `ready` — rather than an
operator command, the *triggering action becomes a money-spending action*,
with no cost-control layer between it and the agent fleet. A labelling
spree, a buggy watcher, or (later) a less-trusted actor could spawn many
Opus invocations and burn budget unchecked. Fine for the current
single-dev private project — the operator controls the labels — but a real
gap for any multi-actor or higher-autonomy setting. **To review later:** a
cost-control layer at the trigger boundary — per-source rate limits, a
concurrency ceiling, and an aggregate spend cap / authorization — so that
"who or what may trigger" is bounded in *cost*, not only in identity. Ties
to [Principle 4](../design/committed/design-principles.md) (cost is a
first-order safety concern) and the trigger-authz question in the
[identity design](../design/aspirational/agent-identity-and-attestation.md).


 

## Schema-migration testing (flagged 2026-07-05)

**Priority: high — release gate for v1.0.0.** No schema migration
should ship after v1.0.0 without having run against populated
databases in CI first.

### Populated-database migration tests for every versioned store

**What exists today.** Two stores carry versioned schemas — the
worker store (`WORKER_SCHEMA_VERSION`, currently v5) and the
control-plane store (`CONTROL_PLANE_SCHEMA_VERSION`, v1) — with a
 shared mechanism: `check_compatibility` decides
FreshInstall / Current / NeedsUpgrade / BinaryTooOld, and
`run_migrations(from, to)` applies additive SQL steps. fq-store
(M3, the Layer-2 extraction) will add a third.

**The gap.** `check_compatibility` has pure unit tests, and every
store test opens a fresh database — so migration SQL executes in CI
only via the `FreshInstall` path, always against an **empty**
database. The `NeedsUpgrade` path — migrating a *populated* vN
database to vN+1 — has never run in CI. The v4→v5 migration
(trigger persistence, 2026-07-05) executed against real data for
the first time on the dogfood box's live worker DB. It worked, but
that is the wrong place for a first run: a defective migration
could corrupt or strand in-flight invocations, and nothing today
would catch it before deployment.

**The shape of the fix** (sketch agreed 2026-07-05):

1. **Build vN, populate, migrate, verify.** For each historical
   step N→N+1 (and the full ladder 0→latest): create a schema at
   vN via `run_migrations(0, N)` — the migration history *is* the
   vN schema definition, no frozen snapshots needed while
   migrations stay additive. Populate with representative data,
   run the remaining migrations, then assert: no error; row counts
   preserved; every pre-existing row readable through the current
   readers; new columns take their documented defaults (e.g.
   pre-v5 rows exercise `resume()`'s warn-and-degrade trigger
   path); `PRAGMA integrity_check` clean.
2. **Generate the data by simulation, not hand-written fixtures.**
   Drive `test_support::sim::SimWorld` invocations — completed,
   crashed mid-flight at span boundaries, and budget-failed — so
   the populated database contains realistic WAL shapes
   (intent/dispatched/completed rows, terminal and non-terminal
   state rows, archive-pending rows). Wrinkle to solve: the
   current binary's writers emit latest-schema rows, so the
   generator runs at HEAD and the rows are projected down to vN's
   column set for insertion (mechanical while migrations are
   additive; revisit if a migration ever transforms data).
3. **Guard the other paths too.** BinaryTooOld still errors;
   reopening at Current does not re-run migrations (some steps,
   e.g. `ALTER TABLE ADD COLUMN`, are not idempotent).
4. **Hermetic, default tier.** SQLite in a tempdir, plain
   `cargo test` — no NATS gate, runs in every CI pass.

Applies to both existing stores now and to fq-store's conformance
suite when M3 lands (that suite's proptest style is the natural
home for a randomised variant). Cross-refs: the reducer
verification plan's sim harness (slice 3) provides the data
generator; the v5 migration commit is the motivating example.

## Brice's Grab bag of ideas.

- [ ] Graph Exec
- [ ] GUI observability over running invocations
- [ ] Crypto verification for topic triggers (so only validated processes can emit to NATS
- [ ] Cost tracking at provider
- [ ] Redundant models and model fallback policies
- [x] Issue template
