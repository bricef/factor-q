# factor-q Architecture Reference

factor-q is one product boundary, but may be composed of multiple internal services. This document catalogues the core subsystems and cross-cutting concerns the system must address.

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
