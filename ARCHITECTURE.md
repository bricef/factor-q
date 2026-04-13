# factor-q Architecture Reference

factor-q is one product boundary, but may be composed of multiple
internal services. This document catalogues the core subsystems and
cross-cutting concerns the system must address.

The first half describes the **design vision** — the full scope of
what factor-q is designed to become. The second half
([Implementation status](#implementation-status-phase-1)) describes
what has been **concretely built** so far, how the Rust codebase is
structured, and where each module lives.

## Core Subsystems

### Event bus
The foundational primitive. Every agent interaction, decision, tool call, and outcome is an event. The bus provides publish/subscribe, persistent ordered storage, and queryability. All other subsystems communicate through it.

### Agent executor
Runs individual agents: receives a trigger, manages the LLM interaction loop (prompt → response → tool calls → result), and emits events for every step. Handles model provider abstraction, allowing each agent to target a different model.

Each agent runs in a **sandboxed execution context** — an explicit boundary where nothing is available by default. The agent definition declares what the agent needs and the executor provisions exactly that:
- **Filesystem scope** — which directories and files the agent can read and write
- **Tool access** — which tools are available to the agent
- **Environment** — which environment variables, credentials, and secrets are visible
- **Network access** — which endpoints the agent can reach
- **Resource limits** — CPU, memory, and time bounds to prevent runaway execution

This is a container-like isolation model, not a permissions layer on top of shared access. Content must be deliberately placed into an agent's context.

### Task engine
Tasks are the unit of work. The task engine manages task lifecycle, dependencies, and scheduling. It supports fan-out (one task spawning many parallel subtasks) and fan-in (aggregating results before proceeding). Tasks are assignable to agents or humans, and task state changes are events on the bus. The orchestrator must understand task dependencies and ordering to do its job, so it owns the task model internally rather than delegating to an external system.

### Agent graph / orchestrator
Defines and executes agent topologies — which agents exist, how they're connected, what events they react to, and how work flows between them. This is the layer above individual agent execution.

### Memory system
Agents need memory that outlasts a single invocation. Three layers:
- **Working memory** — current context window and conversation state for an active agent (managed by the executor)
- **Long-term memory** — per-agent learned knowledge, preferences, and past outcomes
- **Collective memory** — shared knowledge across agents, the "team brain"

Persistent memory (long-term and collective) is delivered as independent MCP services rather than built into the core runtime (see ADR-0013). Agents access memory through standard tool calls (`memory.store`, `memory.retrieve`, `memory.search`). Different memory backends (KV, RAG, vector search) can be different MCP servers behind the same interface. Working memory and context window management remain executor concerns.

### Tool and skill system
Tools are the capabilities available to agents — file operations, shell execution, HTTP requests, database queries, or any custom action. Skills are higher-level, reusable bundles of prompt instructions and tool configurations that encode domain expertise (e.g. "code reviewer", "incident responder").

The system must support:
- A registry of available tools and skills
- Per-agent tool scoping — each agent is configured with a specific set of tools appropriate to its role
- User-authored tools and skills that extend the system without modifying the core
- Tool discovery, so agents (or the graph definition) can reference tools by name
- MCP (Model Context Protocol) as an integration path for external tool providers

### API layer
The runtime exposes a well-defined API that all clients (CLI, TUI, web, external integrations) connect through. The runtime and its interfaces are strictly separated — no client has privileged access that bypasses the API. This is the surface through which humans and external systems interact with factor-q.

### Continuous learning
Agent performance is reviewed and instructions, prompts, and workflows are updated based on outcomes. The event bus provides the raw material — full traces of agent actions and results. The learning loop can be human-driven (review and adjust), agent-driven (a meta-agent that evaluates other agents), or both.

## Cross-Cutting Concerns

### Cost tracking and controls
- Track token usage and cost per agent, per task, and in aggregate
- Per-agent and aggregate budget limits (hard ceilings that halt execution)
- Cost data emitted as events on the bus, queryable via CLI

### Security and access control
- Per-agent permission scoping — which tools, resources, and environments an agent can access
- Credential management — provisioning, scoping, and rotating API keys, tokens, and secrets that agents need
- Approval gates — configurable points where execution pauses for human sign-off before proceeding

### Scheduling and triggers
The system is event-driven, but "event" covers several trigger types:
- Reactive triggers — external events arriving (alerts, webhooks, messages)
- Scheduled triggers — cron-style recurring execution
- File/resource watchers — react to changes in files, repos, or external resources
- Manual triggers — human-initiated via CLI or other interfaces

### Concurrency and resource management
- Parallel agent execution with configurable limits
- Queuing and backpressure when capacity is exceeded
- Priority levels for tasks and agents
- Rate limiting against external APIs (for both cost and politeness)

### Observability
The event bus provides audit data, but the system itself also needs to be observable as infrastructure:
- System health metrics (agent throughput, error rates, queue depth)
- Alerting on system-level anomalies (stuck agents, loops, capacity exhaustion)
- Structured logging for operational debugging

### Error handling and recovery
- Retry policies with configurable backoff per agent or tool
- Dead-letter handling for persistently failing tasks
- Graceful degradation — a single agent failure must not cascade
- Recovery from mid-task failures (resume or clean restart)

### Versioning and rollback
Agent definitions, prompts, and workflows change over time. The system needs:
- Versioned agent configurations
- Ability to roll back to a previous version
- Replay past events against old or new configurations to compare behaviour

### Human-in-the-loop
Headless-first does not mean headless-only. The system needs well-defined interaction points:
- Approval requests surfaced to humans when confidence is low or stakes are high
- Pluggable notification channels (CLI, web, Slack, email)
- Human ability to inspect, intervene, pause, and override at any point

### Multi-project scoping
A single factor-q instance may manage agents across multiple codebases, infrastructure environments, or document corpora. The system needs a concept of workspace or project that scopes agent access, memory, and task state.

### Extension model
How users extend factor-q with custom tools, skills, and integrations. Includes questions of packaging, distribution, versioning, and whether extensions are code, configuration, or both.

---

## Implementation status (phase 1)

Phase 1 built a working walking skeleton that proves the core
architecture end-to-end. This section maps the vision-level
subsystems above to the concrete modules and types that implement
them. For full detail, see the
[phase 1 closing summary](docs/plans/closed/2026-04-02-phase-1-foundation.md).

### Event bus (`fq-runtime/src/bus.rs`)

Two JetStream streams:
- **`fq-events`** — subjects `fq.agent.>` and `fq.system.>`,
  Limits retention (30 days), S2 compression. Holds the full
  event trail: agent lifecycle events plus system lifecycle events
  (startup, shutdown, task failure).
- **`fq-triggers`** — subject `fq.trigger.>`, Limits retention
  (24 hours). Holds pending agent triggers published via
  `fq trigger --via-nats`.

`EventBus::connect()` ensures both streams exist on startup.
Publishing awaits the JetStream ack. Subscription uses core NATS
subscribe (for live tailing) or durable pull consumers (for the
projection and dispatcher).

### Agent executor (`fq-runtime/src/executor.rs`)

`AgentExecutor::run()` drives the agent loop:
1. Emit `Triggered` event (with a full `ConfigSnapshot`)
2. Build the LLM prompt (system prompt + user message from trigger payload)
3. Call the LLM via the `LlmClient` trait
4. If the response contains tool calls, dispatch each one:
   - Validate the tool is in the agent's allowed list
   - Check the tool's sandbox dimension (fs_read, fs_write, exec_cwd)
   - Execute, emit `tool.call` and `tool.result` events
   - Feed results back as tool-role messages
5. Repeat until the LLM stops calling tools, the budget is hit, or
   max iterations (20) are reached
6. Emit `Completed` or `Failed`

Budget enforcement runs after every LLM call and compares the
cumulative cost against the agent's declared budget ceiling.

### LLM abstraction (`fq-runtime/src/llm/`)

- **`LlmClient` trait** — single-method async interface with
  factor-q's own `ChatRequest`/`ChatResponse` types. No external
  library type leaks into the event schema or the executor.
- **`GenAiClient`** (`llm/genai.rs`) — production adapter wrapping
  the `genai` crate. Converts between factor-q types and
  genai's types at the boundary.
- **`FixtureClient`** (`llm/fixture.rs`) — test double returning
  canned responses in order, recording every request for
  assertions.

### Tool sandbox (`fq-tools/src/sandbox.rs`)

`ToolSandbox` enforces four dimensions, each canonicalising paths
before comparison:
- `fs_read` — file_read tool
- `fs_write` — file_write tool
- `exec_cwd` — shell tool (working directory)
- `env` — (declared in agent defs, plumbing to shell tool deferred)

Path traversal (`..`) and symlinks are resolved to their real
filesystem location before the containment check. Sandbox
violations are reported as `tool.result` events with
`is_error: true` and fed back to the LLM so it can adapt.

### Built-in tools (`fq-tools/src/builtin/`)

| Tool | Module | Sandbox check |
|---|---|---|
| `file_read` | `file_read.rs` | `check_read` |
| `file_write` | `file_write.rs` | `check_write` |
| `shell` | `shell.rs` | `check_exec_cwd` |

The shell tool uses argv (no shell invocation), mandatory timeout,
output cap, and a fresh child env with only a pinned PATH.

### Projection consumer (`fq-runtime/src/projection/`)

A durable JetStream consumer on `fq.agent.>` + `fq.system.>` that
materialises every event into a SQLite database. Only envelope
fields and denormalised columns (model, tokens, cost, error_kind,
duration) are stored — no full payloads. NATS is the source of
truth; the projection is rebuildable by replaying from the stream.

### Trigger dispatcher (`fq-runtime/src/dispatcher.rs`)

A durable JetStream consumer on `fq.trigger.>` that dispatches
incoming trigger messages to the correct agent via the executor.
Extracts the agent id from the NATS subject, looks it up in the
registry, parses the JSON payload, and runs the executor. Errors
are acked (not NAK'd) because the executor already emits
`Failed` events for executor-level errors.

### Pricing (`fq-runtime/src/pricing.rs`)

`PricingTable` loads the LiteLLM pricing JSON at startup (fetched
from GitHub, cached locally, fallback to stale cache on network
failure). Provides cost calculation for ~2000 models across
providers.

### System lifecycle events

Three system event types on `fq.system.*`:
- `system_startup` — emitted when `fq run` connects and is ready
- `system_shutdown` — emitted on clean Ctrl-C or task-failure exit
- `system_task_failed` — emitted when the projection consumer or
  trigger dispatcher exits unexpectedly

`fq run` watches both hosted tasks via `tokio::select!`. If
either dies before a Ctrl-C, the daemon publishes
`system.task_failed`, shuts down the other task, and exits
non-zero.

### CLI (`fq-cli/src/main.rs`)

| Command | What it does |
|---|---|
| `fq init` | Scaffold a project from embedded templates |
| `fq run` | Long-running daemon (projection + dispatcher) |
| `fq trigger` | In-process execution or `--via-nats` publish |
| `fq agent list/validate` | Registry inspection |
| `fq events tail` | Live event stream |
| `fq events query` | Historical query via SQLite |
| `fq costs` | Per-agent cost aggregation |
| `fq status` | Runtime health check |

### What is NOT yet built

These subsystems from the vision are not implemented:
- Task engine (fan-out/fan-in, dependency graph)
- Agent graph / multi-agent orchestration
- Memory system (MCP services — scoped to phase 2)
- Skill registry (AgentSkills format — scoped to phase 2)
- API layer (REST/gRPC/WebSocket — ADR-0006 is still draft)
- Continuous learning
- Container-level isolation (ADR-0010 accepted: containers by default, Kata+Firecracker upgrade path)
- Scheduled triggers / internal job scheduler
