# Phase 1: Foundation

## Goal

Prove the core architecture end-to-end: an event goes in, an agent acts, events come out, a human can see what happened. Phase 1 delivers a working single-agent runtime with event-driven execution, cost controls, and CLI-based inspection.

## Architecture Overview

```
                ┌─────────────────────────────────────┐
                │          factor-q daemon             │
                │                                      │
  .md files ──► │  Agent Definition Loader              │
                │         │                            │
                │         ▼                            │
  CLI ────────► │  Agent Executor ◄──► LLM Providers   │
                │    │        │         (Anthropic,     │
                │    │        │          OpenAI)        │
                │    │        ▼                        │
                │    │     Tool Runtime                │
                │    │     (read, write, shell)        │
                │    │                                 │
                │    ▼                                 │
                │  NATS + JetStream                    │
                │    │                                 │
                │    ▼                                 │
                │  SQLite Projection Consumer          │
                └────┬────────────────────────────────┘
                     │
  CLI ◄──────────────┘ (query events, costs, status)
```

## Scope

### 1. NATS foundation
The first thing to build. Everything else publishes to or consumes from NATS.

- Connect to a NATS server with JetStream enabled
- Define the subject hierarchy:
  - `fq.agent.{agent_id}.triggered`
  - `fq.agent.{agent_id}.llm.request`
  - `fq.agent.{agent_id}.llm.response`
  - `fq.agent.{agent_id}.tool.call`
  - `fq.agent.{agent_id}.tool.result`
  - `fq.agent.{agent_id}.completed`
  - `fq.agent.{agent_id}.failed`
  - `fq.agent.{agent_id}.cost`
  - `fq.system.>`  (runtime lifecycle events)
- Create durable streams with appropriate retention policies
- Establish event schema (likely JSON, with a versioned schema)

### 2. Agent definition loader
Parse agent definitions from Markdown files into the internal Rust representation.

- Parse YAML frontmatter (model, tools, sandbox, budget, trigger)
- Extract Markdown body as system prompt
- Validate against JSON Schema
- Build into internal `Agent` struct via the builder pattern
- Watch agent definition directories for changes and hot-reload
- Report clear errors for invalid definitions

### 3. Agent executor
The core agent loop, running within a sandboxed context.

- Receive a trigger (NATS message or manual CLI invocation)
- Build the LLM prompt from the agent's system prompt + trigger payload
- Call the configured LLM provider (streaming)
- Parse the response for tool calls
- Execute tool calls within the agent's sandbox
- Feed tool results back to the LLM for the next iteration
- Emit events to NATS at each step (triggered, llm.request, llm.response, tool.call, tool.result, completed/failed)
- Respect budget ceiling — halt execution if cost limit is reached
- Support at least two model providers (Anthropic and OpenAI) to prove model-agnosticism

### 4. Basic tool set
A minimal set of tools that make an agent capable of real work.

- **File read** — read file contents, scoped to the agent's allowed filesystem paths
- **File write** — write/create files, scoped to the agent's allowed filesystem paths
- **Shell execution** — run shell commands, scoped to the agent's sandbox (working directory, environment, timeout)

Tools enforce sandbox boundaries — a tool call that violates the agent's declared permissions is rejected, not executed.

### 5. Basic sandboxing
Enforce the "nothing by default" principle at the filesystem level.

- Agent definitions declare filesystem read/write paths and allowed environment variables
- The executor enforces these boundaries when tools are invoked
- Out-of-scope tool calls are rejected with a clear error event
- Full container isolation is deferred (ADR-0010) but the enforcement interface is designed to accommodate it later

### 6. Cost tracking
Built into the executor from the first LLM call.

- Track input/output token counts per LLM call
- Calculate cost based on model pricing (configurable per provider/model)
- Emit cost events to NATS after each LLM call
- Enforce per-agent budget ceiling — halt execution and emit a `budget.exceeded` event
- Project cost data into SQLite for querying

### 7. SQLite projection consumer
A NATS consumer that materialises events into a queryable SQLite database.

- Subscribe to `fq.>` (all factor-q events)
- Write events to SQLite tables (agents, invocations, tool calls, costs)
- Support queries: agent history, cost breakdown, recent activity, event filtering by type/agent/time

### 8. CLI
The human interface for phase 1. Communicates with the daemon and the projection store.

- `fq init` — initialise a factor-q project (create directory structure, default config)
- `fq agent list` — list registered agent definitions
- `fq agent validate <path>` — validate an agent definition file
- `fq start` — start the runtime daemon (connects to NATS, loads agents, begins listening)
- `fq stop` — stop the runtime daemon
- `fq trigger <agent> [payload]` — manually trigger an agent
- `fq events tail [--subject <filter>]` — tail the event stream in real time
- `fq events query [--agent <id>] [--type <type>] [--since <time>]` — query the projection store
- `fq costs [--agent <id>] [--since <time>]` — show cost breakdown

## Out of scope

- Graph definitions and multi-agent orchestration
- Spawn/exec and fan-out/fan-in
- Task engine and dependency management
- Memory system (long-term, collective)
- Continuous learning
- Extension model, MCP, skill library
- TUI or visual interfaces
- Session management and conversation trees
- Container-level isolation

## Success criteria

A user can:
1. Write an agent definition as a Markdown file specifying a model, tools, sandbox, and budget
2. Start the factor-q daemon, which connects to NATS and loads agent definitions
3. Trigger the agent manually via CLI or via a NATS message
4. Watch the agent execute — LLM calls, tool invocations, results — as a stream of events
5. Query the event history and cost data after execution completes
6. Modify the agent definition and see the changes take effect without restarting the daemon
7. See execution halt when the agent's budget ceiling is reached

## Implementation order

A suggested sequence that keeps each step demonstrable:

1. **Project scaffolding** — Rust workspace, dependencies, CI
2. **NATS integration** — connect, create streams, publish/subscribe, event schema
3. **Agent definition parser** — Markdown + YAML frontmatter → builder → `Agent` struct
4. **Agent executor** — core loop with a single provider (Anthropic), emitting events
5. **Basic tools** — file read/write/shell with sandbox enforcement
6. **Cost tracking** — token counting, budget enforcement, cost events
7. **SQLite projections** — consumer that materialises events for querying
8. **CLI** — commands layered on as each subsystem becomes available
9. **Second provider** — add OpenAI to prove model-agnosticism
10. **Hot-reload** — file watching and agent definition reload without restart
