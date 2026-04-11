# Phase 1: Foundation

## Goal

Prove the core architecture end-to-end: an event goes in, an agent acts, events come out, a human can see what happened. Phase 1 delivers a working single-agent runtime with event-driven execution, cost controls, and CLI-based inspection.

This is a walking skeleton — breadth over depth, proving integration across all components.

## Architecture Overview

```
                ┌─────────────────────────────────────┐
                │          factor-q (fq run)           │
                │                                      │
  .md files ──► │  Agent Definition Loader              │
                │         │                            │
                │         ▼                            │
  CLI ────────► │  Agent Executor ◄──► LLM Provider    │
                │    │        │         (Anthropic)     │
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

## Core Scope

### 1. Configuration
Runtime configuration for NATS connection, API keys, agent directories, and model pricing.

- Configuration file (`fq.toml` or similar) with sensible defaults
- Environment variable overrides for secrets (API keys)
- `fq init` creates a project directory structure with default configuration
- Clear error messages for missing or invalid configuration

### 2. NATS foundation
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

### 3. Agent definition loader
Parse agent definitions from Markdown files into the internal Rust representation.

- Parse YAML frontmatter (model, tools, sandbox, budget, trigger)
- Extract Markdown body as system prompt
- Validate against JSON Schema
- Build into internal `Agent` struct via the builder pattern
- Report clear errors for invalid definitions

### 4. Agent executor
The core agent loop, running within a sandboxed context.

- Receive a trigger (NATS message or manual CLI invocation)
- Build the LLM prompt from the agent's system prompt + trigger payload
- Call the configured LLM provider (streaming)
- Parse the response for tool calls
- Execute tool calls within the agent's sandbox
- Feed tool results back to the LLM for the next iteration
- Emit events to NATS at each step (triggered, llm.request, llm.response, tool.call, tool.result, completed/failed)
- Respect budget ceiling — halt execution if cost limit is reached
- Support one model provider (Anthropic) with a well-defined trait interface for future providers

### 5. Basic tool set
A minimal set of tools that make an agent capable of real work.

- **File read** — read file contents, scoped to the agent's allowed filesystem paths
- **File write** — write/create files, scoped to the agent's allowed filesystem paths
- **Shell execution** — run shell commands, scoped to the agent's sandbox (working directory, environment, timeout)

Tools enforce sandbox boundaries — a tool call that violates the agent's declared permissions is rejected, not executed.

### 6. Basic sandboxing
Enforce the "nothing by default" principle at the filesystem level.

- Agent definitions declare filesystem read/write paths and allowed environment variables
- The executor enforces these boundaries when tools are invoked
- Out-of-scope tool calls are rejected with a clear error event
- Full container isolation is deferred (ADR-0010) but the enforcement interface is designed to accommodate it later

### 7. Cost tracking
Built into the executor from the first LLM call.

- Track input/output token counts per LLM call
- Calculate cost based on model pricing loaded from the LiteLLM pricing JSON
- Emit cost events to NATS after each LLM call
- Enforce per-agent budget ceiling — halt execution and emit a `budget.exceeded` event
- Project cost data into SQLite for querying

Pricing is fetched from the LiteLLM repository at startup, cached locally
(default `$XDG_CACHE_HOME/factor-q/pricing.json`), and parsed into a
`PricingTable`. On fetch failure the runtime falls back to the cached
copy; on cache miss too it runs with an empty table (costs reported as
$0 with a warning per unknown model). This gives us automatic coverage
of hundreds of models without hand-maintaining entries.

### 8. SQLite projection consumer
A NATS consumer that materialises events into a queryable SQLite database.

- Subscribe to `fq.>` (all factor-q events)
- Write events to SQLite tables (agents, invocations, tool calls, costs)
- Support queries: agent history, cost breakdown, recent activity, event filtering by type/agent/time

### 9. CLI
The human interface for phase 1. Communicates with the runtime and the projection store.

- `fq init` — initialise a factor-q project (create directory structure, default config, sample agent)
- `fq agent list` — list registered agent definitions
- `fq agent validate <path>` — validate an agent definition file
- `fq run` — start the runtime in the foreground (connects to NATS, loads agents, listens for triggers)
- `fq trigger <agent> [payload]` — manually trigger an agent
- `fq events tail [--subject <filter>]` — tail the event stream in real time
- `fq events query [--agent <id>] [--type <type>] [--since <time>]` — query the projection store
- `fq costs [--agent <id>] [--since <time>]` — show cost breakdown

### 10. Structured logging
Runtime diagnostics via the `tracing` crate.

- Structured log output for connection failures, parse errors, LLM timeouts, and runtime lifecycle
- Configurable log level via config or environment variable
- Distinct from the event bus — logging is for operators debugging the runtime, events are for agent activity

### 11. Sample agent
A working example agent shipped with `fq init`.

- Demonstrates the agent definition format (frontmatter + prompt)
- Uses at least two tools (e.g. file read + shell)
- Works out of the box with a valid API key
- Serves as documentation by example

## Stretch goals
Items that belong in phase 1 conceptually but can be deferred without compromising the walking skeleton.

### Daemon mode
Run the runtime as a background process with `fq start` / `fq stop` instead of the foreground `fq run`. Requires process management, pidfiles, IPC between CLI and daemon, signal handling, and graceful shutdown.

### Second model provider
Add OpenAI as a second provider to prove model-agnosticism in practice, not just in the trait interface. The trait design from the core scope should make this mechanical.

### Hot-reload
Watch agent definition directories for file changes and reload definitions without restarting the runtime. Requires file watching, debouncing, and safe swapping of definitions while the executor may be mid-run.

## Deferred work (known phase 1 gaps)

These are pieces we know we'll need once phase 1 is in place, but they
don't block the walking skeleton.

### Scheduled refresh of pricing data
Phase 1 loads the LiteLLM pricing JSON once at startup. That's fine
while the runtime is a foreground process that restarts often during
development, but once factor-q is a continuously running service (per
the self-hosted vision in ADR-0002), startup-only loading is not
acceptable — prices drift, new models ship, and the runtime would keep
using a stale cache indefinitely.

The right place to fix this is a general internal job scheduler that
also supports agent triggers (scheduled agents are already required by
the phase 1 plan under "triggers"). When we build that, pricing refresh
becomes one internal job among many:

- Cron-style or interval-based
- Refreshes the LiteLLM JSON, atomically replaces the `PricingTable`
- Emits events on successful refresh and on failures
- Observable via the same `fq events` commands as agent activity

Tracking this as deferred rather than a hard phase 1 dependency because
the cache fallback makes stale-but-usable the default behaviour, and
phase 1's primary purpose is proving the walking skeleton.

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
1. Run `fq init` and get a working project with configuration and a sample agent
2. Start the runtime with `fq run`, which connects to NATS and loads agent definitions
3. Trigger the agent manually via `fq trigger`
4. Watch the agent execute — LLM calls, tool invocations, results — as a stream of events
5. Query the event history and cost data after execution completes
6. See execution halt when the agent's budget ceiling is reached

## Implementation order

A suggested sequence that keeps each step demonstrable:

1. **Project scaffolding** — Rust workspace, dependencies, CI
2. **Configuration** — config file parsing, environment variables, `fq init`
3. **Structured logging** — `tracing` setup, log levels
4. **NATS integration** — connect, create streams, publish/subscribe, event schema
5. **Agent definition parser** — Markdown + YAML frontmatter → builder → `Agent` struct
6. **Sample agent** — working example for testing the pipeline
7. **Agent executor** — core loop with Anthropic provider, emitting events
8. **Basic tools** — file read/write/shell with sandbox enforcement
9. **Cost tracking** — token counting, budget enforcement, cost events
10. **SQLite projections** — consumer that materialises events for querying
11. **CLI** — commands layered on as each subsystem becomes available
