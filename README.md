# factor-q

A single-tenant, self-hosted agent runtime for designing, operating, and evolving multi-agent systems that deliver on large, ongoing projects.

factor-q is not a chatbot or an interactive coding assistant. It is a continuously running, event-driven agent orchestrator where human interaction is one input among many.

## Key properties

- **Event-driven** — every agent interaction is an event on a NATS-based event bus, enabling auditing, replay, and debugging
- **Model-agnostic** — each agent in a graph targets the model best suited to its task, mixing providers freely
- **Headless-first** — runs as a persistent server; CLI, TUI, and other interfaces are clients of the runtime
- **Cost-aware** — budget limits and spending controls built in from the start
- **Extensible** — custom tools via subprocess/MCP; agent definitions as Markdown files, graph definitions as YAML

## Documentation

- [Quickstart](QUICKSTART.md) — clone to running agent in under ten minutes
- [Vision](VISION.md) — what factor-q is and why it exists
- [Architecture](ARCHITECTURE.md) — core subsystems and implementation
- [Design principles](docs/design/design-principles.md) — cross-cutting rules that guide design decisions
- [Contributing](CONTRIBUTING.md) — development setup, test tiers, code conventions
- [Agent authoring guide](docs/guide/agent-definitions.md) — write your first agent
- [Reducer harness guide](docs/guide/reducer-harness.md) — the suspend/resume-capable execution path (`fq trigger --reducer`)
- [Event schema](docs/design/event-schema.md) — the event model everything is built around
- [Agent orchestration tools](docs/design/agent-orchestration-tools.md) — wishlist for primitives to coordinate multi-agent work (graph substrate, handles, sinks, fragment library)
- [Worker-side ergonomics](docs/design/worker-side-ergonomics.md) — primitives for what an agent has, knows, and controls while executing (introspection, checkpoints, structured errors)
- [Storage and scaling](docs/design/storage-and-scaling.md) — sizing analysis for NATS and SQLite
- [ADRs](docs/adrs/) — architectural decision records ([accepted](docs/adrs/accepted/), [draft](docs/adrs/draft/))
- [Phase 1 (closed)](docs/plans/closed/2026-04-02-phase-1-foundation.md) — what shipped in the walking skeleton
- [Phase 2 (active)](docs/plans/active/2026-04-11-phase-2-mcp-and-memory.md) — MCP, memory, and skills
- [Backlog](docs/plans/backlog.md) — deferred work

## Technology

- **Runtime:** Rust
- **Event bus:** NATS + JetStream
- **Projections:** SQLite
- **Agent definitions:** Markdown with YAML frontmatter
- **Graph definitions:** YAML with JSON Schema

## Project structure

```
factor-q/
├── services/fq-runtime/       Rust workspace (CLI + runtime + tools)
│   └── crates/
│       ├── fq-cli/              fq binary (CLI commands, daemon host)
│       ├── fq-runtime/          core library (bus, executor, projection, dispatcher)
│       └── fq-tools/            built-in tools and sandbox enforcement
│
├── infrastructure/            Deployment and local dev
│   ├── docker-compose.yml       NATS + JetStream
│   └── nats/                    NATS server configuration
│
├── agents/examples/           Sample agent definitions
├── skills/                    Skill registry (future, AgentSkills format)
├── tests/smoke/               End-to-end smoke tests against a real LLM
│
├── docs/
│   ├── adrs/                  Architectural decision records
│   ├── design/                Event schema, storage and scaling specs
│   ├── guide/                 User-facing guides (agent authoring)
│   └── plans/                 Phase plans, backlog
│
├── VISION.md
├── ARCHITECTURE.md
├── CONTRIBUTING.md
└── README.md
```

## Getting started

See [QUICKSTART.md](QUICKSTART.md) for the full step-by-step path from a fresh clone to a running agent with event-trail inspection. The short version:

```sh
just up                                 # NATS + build
mkdir my-project && cd my-project
just fq init                            # writes fq.toml, agents/, sample
export ANTHROPIC_API_KEY='sk-ant-...'
just fq trigger sample-agent "Hello."   # run the agent
just fq events tail                     # (another terminal) watch the events
```

For development setup and test tiers, see [CONTRIBUTING.md](CONTRIBUTING.md).

## Prior Art

- [Crush](research/crush.md) — architecture analysis
- [OpenCode](research/opencode.md) — architecture analysis
- [open-agent.io](https://open-agent.io/)
