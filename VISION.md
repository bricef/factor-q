# factor-q Vision

## What is factor-q?

factor-q is a single-tenant, self-hosted agent runtime for power users who want to design, operate, and evolve multi-agent systems that deliver on large, ongoing projects.

It is not a chatbot. It is not an interactive coding assistant. It is a continuously running agent orchestrator where human interaction is one input among many.

## Core Thesis

The terminal-based AI agent UX (pioneered by tools like Claude Code and OpenCode) proved that agentic AI can be practical and productive. But these tools are anchored to a single interaction model: human types, agent responds, repeat.

Real-world work — building software products, operating infrastructure, analysing regulatory documents — requires agents that run continuously, react to events, coordinate with each other, and surface decisions to humans only when needed.

factor-q takes the agent paradigm beyond interactive sessions toward a long-running, event-driven, multi-agent runtime.

## Design Principles

### Single-tenant, self-hosted
factor-q runs on your infrastructure as a persistent server process. No multi-tenancy, no platform abstraction. You own and operate it. The runtime outlasts any individual user session — agents keep running whether or not a human is connected.

### Event bus as the spine
Every agent interaction, decision, and outcome is an event on a common bus. This is the foundational primitive, not an afterthought. The bus enables:
- **Auditing** — full trace of what happened and why
- **Replay** — re-run event sequences against modified agent graphs to test changes
- **Debugging** — inspect exactly where things went wrong
- **Learning** — agents or meta-agents consume the event stream to improve over time
- **Decoupling** — agents emit and react to events, not direct calls

### Model-agnostic by necessity
A single agent graph will mix models suited to their task:
- Frontier models for planning, supervision, and complex reasoning
- Fast/cheap models for classification, summarisation, and extraction
- Specialised or fine-tuned models for domain-specific work

Model selection is a per-agent configuration concern, not a global setting.

### Cost-aware by default
Autonomous agents spending money without human oversight is a first-order risk. The runtime tracks costs per agent, per task, and in aggregate. Budget limits and spending controls are built in from the start — not bolted on after an incident.

### Headless-first
The system runs autonomously. The TUI, CLI, and any other interfaces are clients of the runtime, useful for configuration, inspection, and intervention. The system does not stop when you close the terminal.

### Graph-based agent composition
Agent systems are defined as graphs — not just parent/child spawning, but user-designable topologies of agents with different roles, specialisations, and communication patterns.

### Extensible by design
Power users need to extend the system with custom tools, skills, and integrations. The tool and skill system is a first-class concern — agents are configured with specific tool sets scoped to their role, and users can author and share new capabilities without modifying the core runtime.

## Target Use Cases

### 1. Software product development
A user configures a swarm of agents to research, design, and build software products — greenfield or on existing codebases. Agents handle research, implementation, testing, and review, with human oversight at key decision points.

### 2. Automated systems operations
Operational events (alerts, notifications, deployments) arrive and are evaluated, investigated, and potentially remediated by a team of agents operating on live infrastructure.

### 3. Regulatory document analysis
A swarm of dedicated agents performs detailed analysis of documents in regulated fields. When regulations are updated, the system detects changes and propagates updated advice through the analysis pipeline.

## What factor-q is not

- **Not a chatbot** — it is an orchestrator, not a conversation partner
- **Not a platform/SaaS** — it is a tool you run, not a service you subscribe to
- **Not code-only** — it handles documents, analysis, operations, and any domain where agents can be effective
- **Not tied to one model provider** — model diversity within a single agent graph is a first-class concern
