# Architecture Decision Records

ADRs capture significant, hard-to-reverse decisions and the reasoning
behind them. Each is a point-in-time record: it reflects what was
decided when, and isn't rewritten as the system evolves (later ADRs
supersede earlier ones; guides track the current state).

Accepted ADRs live in `accepted/`, drafts in `draft/`, each named
`NNNN-slug.md`.

## Accepted

| ADR | Decision |
|---|---|
| [0001](accepted/0001-internal-task-management.md) | Internal task management |
| [0002](accepted/0002-self-hosted-server.md) | Self-hosted server, not a local CLI tool |
| [0003](accepted/0003-model-agnostic-per-agent.md) | Model-agnostic, per-agent model selection |
| [0004](accepted/0004-cost-controls-from-day-one.md) | Cost controls from day one (per-invocation budget; sampling/elicitation sub-budget attribution) |
| [0005](accepted/0005-agent-definition-format.md) | Agent definition format — Markdown + YAML frontmatter |
| [0009](accepted/0009-technology-choices.md) | Technology choices (Rust runtime) |
| [0010](accepted/0010-agent-execution-isolation.md) | Agent execution isolation (containers by default; nothing-by-default sandbox) |
| [0011](accepted/0011-event-bus-and-persistence.md) | Event bus and persistence (NATS + JetStream) — persistence/source-of-truth role partially superseded by [ADR-0026](accepted/0026-event-log-system-of-record.md); bus role stands |
| [0012](accepted/0012-graph-definition-format.md) | Execution graph definition format |
| [0013](accepted/0013-memory-as-mcp-service.md) | Memory as an MCP service |
| [0014](accepted/0014-agent-harness-as-reducer.md) | Agent harness as a reducer with a runtime-owned loop |
| [0015](accepted/0015-rust-runtime-polyglot-tools.md) | Rust runtime, polyglot tools, language boundary at the event bus |
| [0016](accepted/0016-typed-operations-no-free-form-apis.md) | Typed operations exposed to agents, no free-form storage APIs |
| [0017](accepted/0017-mcp-human-in-the-loop.md) | Autonomous resolution of MCP human-in-the-loop primitives (the capability-grant policy) |
| [0018](accepted/0018-mcp-server-initiated-execution.md) | Execution model for server-initiated MCP calls (sampling/elicitation/roots) |
| [0019](accepted/0019-skill-format.md) | Skill format and discovery |
| [0020](accepted/0020-mcp-notification-handling.md) | MCP server notifications — drained in the daemon, tools refresh between invocations |
| [0021](accepted/0021-mcp-cost-control-and-memory-boundary.md) | Cost control for MCP services via `_meta` (budget hint + cost report); memory stays MCP; embedding boundary deferred to the storage design |
| [0022](accepted/0022-binary-distribution-and-licensing.md) | Binary distribution (musl/Apple Silicon release matrix, install.sh, cargo-binstall) and BSL 1.1 licensing |
| [0023](accepted/0023-storage-and-vector-foundation.md) | Storage, extraction, and vector index foundation (Phase 2 pillar #2) |
| [0024](accepted/0024-separate-databases-storage-foundation.md) | Separate databases for the storage foundation's three stores (refines ADR-0023 F9) |
| [0007](accepted/0007-inter-agent-communication.md) | Inter-agent communication — agents never touch the transport; one graph executor with two authoring surfaces (declared graphs + spawn as sugar); per-traversal budget with an ε cost floor; graduated from the April draft |
| [0026](accepted/0026-event-log-system-of-record.md) | A dedicated CAS-backed archive service is the event log's system of record (supersedes ADR-0011's source-of-truth half; NATS becomes transport) |
| [0027](accepted/0027-graceful-drain-deploys.md) | Deploys are a graceful drain — a `fq drain` control command suspends in-flight invocations to a step boundary; the new binary resumes via recovery (not kill-and-replace) |
| [0028](accepted/0028-tool-scoped-isolation-and-workspace.md) | Tool-scoped isolation + a harness-owned virtual filesystem (safe by construction); supersedes ADR-0010's agent-scoped unit of isolation |

## Draft

| ADR | Decision |
|---|---|
| [0006](draft/0006-api-design.md) | Runtime API design |
| [0008](draft/0008-extension-model.md) | Extension and plugin model |
| [0025](draft/0025-storage-gc-observability.md) | Storage GC observability |

## Related guides

- [Writing Agent Definitions](../guide/agent-definitions.md) — the live frontmatter reference (implements ADR-0005).
- [MCP](../guide/mcp.md) — the MCP capability model (implements ADR-0013/0017/0018).
- [Reducer harness](../guide/reducer-harness.md) — the execution model (implements [ADR-0014](accepted/0014-agent-harness-as-reducer.md)).
