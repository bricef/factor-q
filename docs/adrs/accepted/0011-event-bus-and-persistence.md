# ADR-0011: Event Bus and Persistence Technology

## Status
Accepted

## Context
factor-q's event bus is the foundational primitive — every subsystem communicates through it. It must provide persistent ordered event storage, pub/sub with topic filtering, replay, and request/reply semantics. The choice must also align with the self-hosted, single-tenant deployment model while leaving room to grow.

## Decision: NATS with JetStream

### Event bus and messaging: NATS + JetStream

NATS with JetStream is adopted as the event bus, messaging backbone, and primary persistence layer for events.

### Rationale

**Native coverage of factor-q's requirements:**
- Append-only ordered streams with per-message sequence numbers and timestamps — a natural event log
- Hierarchical subject-based pub/sub with wildcards (e.g. `agents.{id}.tool_call`, `tasks.{id}.completed`)
- Replay from any sequence number or timestamp, at original or instant speed
- Native request/reply with scatter-gather support
- Fan-out (all subscribers receive) and queue groups (load-balanced consumers)
- Built-in backpressure via `MaxAckPending` and flow control
- At-least-once and exactly-once delivery guarantees
- Key-value store built on JetStream — usable for agent configuration, state, and metadata without a separate database
- Per-message TTL alongside stream-level retention policies
- Optimistic concurrency via expected-sequence headers — useful for task state transitions

**Operational simplicity:**
- Single binary, sub-50 MB memory footprint, zero external dependencies
- Starts in milliseconds, configured via a single file
- As lightweight as an external dependency can be — appropriate for the self-hosted model

**Growth path:**
- NATS supports multi-node clustering natively. A single-node deployment today can scale to a cluster without architectural changes if the system ever needs to grow beyond a single server.
- Stream mirroring and sourcing support multi-project and cross-environment patterns.

**Rust client:**
- `async-nats` is the official Rust client, maintained by Synadia. Tokio-native, production-ready, with full JetStream support.

### Queryable projections: SQLite

NATS does not support arbitrary SQL-like queries over the event log. For complex queries (e.g. "all tool calls by agent X in the last hour that cost more than $0.10"), events will be projected into SQLite via consumers. SQLite serves as a read-optimised query store for CLI inspection, cost reporting, and debugging — not as the source of truth.

`sqlx` with compile-time query checking will be used for the SQLite layer.

### Tradeoffs accepted

- **External process dependency** — factor-q requires a NATS server running alongside it. This is mitigated by NATS's minimal footprint and single-binary deployment. It can be bundled, co-deployed, or managed as a sidecar.
- **No built-in projections or materialised views** — unlike EventStoreDB, NATS does not compute derived views. factor-q must build its own projection consumers. This is additional code but provides full control over the query model.
- **Learning curve** — NATS's subject hierarchy, consumer types, and retention policies are a conceptual surface area that contributors need to understand.

## Consequences
- NATS server is a required component of a factor-q deployment
- The event schema will use NATS subject hierarchy as the primary organising structure
- All subsystems (agent executor, task engine, cost tracking, CLI) communicate through NATS subjects
- SQLite is used as a projection store for complex queries, not as the event source of truth
- JetStream's key-value store may reduce or eliminate the need for a separate configuration database
- The system can scale from single-node to clustered NATS without architectural changes
