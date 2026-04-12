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

- [Vision](VISION.md) — what factor-q is and why it exists
- [Architecture](ARCHITECTURE.md) — core subsystems and implementation
- [Contributing](CONTRIBUTING.md) — development setup, test tiers, code conventions
- [Agent authoring guide](docs/guide/agent-definitions.md) — write your first agent
- [Event schema](docs/design/event-schema.md) — the event model everything is built around
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

Prerequisites: Rust toolchain, Docker, Docker Compose, [just](https://github.com/casey/just).

```sh
# Start NATS and build the runtime
just up

# Initialise a new project (creates fq.toml, agents/, sample agent)
just fq init

# Trigger the sample agent
export ANTHROPIC_API_KEY='sk-ant-...'
just fq trigger sample-agent "Say hello in one sentence."

# Watch events stream in real time (in another terminal)
just fq events tail

# Check runtime health
just fq status

# Run the daemon (projection consumer + trigger dispatcher)
just run
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for the full development
setup and test tiers.

## Prior Art

- [Crush](research/crush.md) — architecture analysis
- [OpenCode](research/opencode.md) — architecture analysis
- [open-agent.io](https://open-agent.io/)
