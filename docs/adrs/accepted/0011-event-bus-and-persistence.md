# ADR-0011: Event Bus and Persistence Technology

## Status

Accepted. **Partially superseded by
[ADR-0026](0026-event-log-system-of-record.md) (2026-07-05):** the
source-of-truth / "primary persistence layer for events" decision below is
overturned — NATS is demoted to transport and a dedicated CAS-backed
archive service becomes the event log's system of record, with the SQLite
projection *to be* rebuilt from that archive rather than from NATS. The
NATS-with-JetStream **event-bus and messaging-backbone** decision (subjects,
pub/sub, request/reply, replay-within-window) remains in force.

Implementation: partial — the bus half shipped and carries the whole system
(NATS + JetStream over `async-nats`, SQLite projections through `sqlx`, split
into three stores by #262/#269). ADR-0026's half did not: **the archive
service does not exist**, so the projection is still replayed from NATS, not
from any archive — `services/fq-runtime/crates/fq-runtime/src/db.rs` calls
`projection.db` "derived from the NATS stream; disposable (delete + replay
rebuilds it)". Read the supersession note above as the decided direction, not
as a description of today. § Where events durably live today records what
actually holds.

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
- All *runtime* subsystems (agent executor, task engine, cost tracking)
  communicate through NATS subjects. The CLI stopped:
  [ADR-0006](0006-registry-first-api.md) D8 and Appendix C narrowed `fq.*` to
  internal infrastructure, and the migration reached zero remaining call
  points (#498), so `fq` reaches the daemon over the authenticated edge and
  links no NATS at all. The first-party Go adapters remain direct publishers
  on the internal SPI (#478, #479)
- SQLite is used as a projection store for complex queries, not as the event
  source of truth. True of `projection.db`, and only of it — the split into
  three stores (#262) left `worker.db` and `control-plane.db` declared as
  non-rebuildable sources of truth for in-flight state, coordination,
  schedules and the invocation archive
- JetStream's key-value store may reduce or eliminate the need for a separate configuration database
- The system can scale from single-node to clustered NATS without architectural changes

## Where events durably live today

Neither this ADR nor [ADR-0026](0026-event-log-system-of-record.md) describes
the current shape, because the archive service ADR-0026 decides on is
unbuilt. What holds as of 2026-08:

| Surface | Lifetime | Record status |
|---|---|---|
| NATS `fq-events` | 30 days, fixed in code (`bus::DEFAULT_MAX_AGE`) | The only complete payload-bearing event trail |
| `projection.db` (`events`) | 30 days default (`[state].retention_days`) | Derived and disposable; typed columns, no payloads. Cost-bearing rows (`llm_response`, `llm_failure`, `invocation_summary`) are exempt and kept indefinitely |
| `invocation_archive` in `control-plane.db` | 30 days default — the same `[state].retention_days`, keyed on `archived_at` | Per-invocation final phase and state blob, not the trail. Non-rebuildable while it lives |

SQLite therefore does carry genuine source-of-truth duty today — for
coordination and for invocation outcomes — under a 30-day sweep, and nothing
beyond the cost-bearing projection rows outlives retention. The maintained
version of this table is in
[event-schema.md](../../design/committed/event-schema.md); consult that when
the answer has to be current.
