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
