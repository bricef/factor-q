# Backlog

Deferred work that isn't committed to a specific phase yet. Items
accumulate here when they're flagged during design or implementation
but aren't important enough to block a phase closing.

Ordering within a section is **priority-ish** — the things at the
top are more likely to be pulled into the next phase. Nothing in
here should be assumed scheduled.

## Deferred from phase 1

### Hot-reload of agent definitions
**Source:** phase 1 plan (`docs/plans/closed/2026-04-02-phase-1-foundation.md`), stretch goals section.

The runtime currently loads agent definitions once at `fq run`
start. Editing a `.md` file does not affect a running daemon.

Work needed:
- File watcher on the configured agents directory (`notify` crate
  is the usual choice)
- Debounce burst changes (common when an editor writes via a
  temp file and rename)
- Parse the new definition; reject invalid changes with a clear
  log line and keep the old one
- Swap the definition in the registry atomically
- Decide what happens to in-flight invocations using the old
  definition — simplest is "they finish with the version they
  started with" (ConfigSnapshot already supports this)

Not urgent: operators can `kill` and restart the daemon. NATS
durability means no triggers are lost. But iterative development
is noticeably faster with hot-reload.

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
**Source:** `docs/plans/closed/2026-04-02-phase-1-foundation.md` deferred-work section, and `docs/design/storage-and-scaling.md`.

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
  children, or are they independent?

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
(`docs/design/tool-isolation-model.md`).

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

See `docs/design/shadow-mode-and-self-improvement.md`,
`docs/design/tool-isolation-model.md`, and ADR-0010 for
full context.

### WASM-native POSIX sandbox for shell and file tools
**Source:** `docs/design/wasm-posix-sandbox.md`, design
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
`docs/design/tool-isolation-model.md`.

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
`docs/design/tool-isolation-model.md`.

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

## Reducer boundary invariants (flagged 2026-05-15)

A design pass on validation at the reducer host/guest boundary
during the envelope-refactor work surfaced three threads. The
first two are worth doing now; the third is deferred to the
graph-executor work that will need it for its own reasons.

See `docs/design/inter-node-contracts-and-event-layers.md` §2
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
`docs/design/inter-node-contracts-and-event-layers.md`,
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

## Process and documentation gaps

### ADRs still in draft
The following ADRs are still in `docs/adrs/draft/` and should
either be accepted (with a resolution documented) or deliberately
deprecated:
- ADR-0006 API design
- ADR-0007 Inter-agent communication
- ADR-0008 Extension model

ADR-0010 (agent execution isolation) has been accepted. Phase 2
(MCP, memory, skills) will likely force ADR-0008 to resolution.
ADR-0007 can wait for multi-agent deployments.
