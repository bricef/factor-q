# ADR-0026: A dedicated archive service is the event log's system of record

## Status

Accepted (2026-07-05). Supersedes the source-of-truth half of
[ADR-0011](0011-event-bus-and-persistence.md): NATS remains the event bus
and messaging backbone (that decision stands), but it is **no longer the
primary persistence layer / source of truth for events**. Builds on the
content-addressed store of
[ADR-0023](0023-storage-and-vector-foundation.md) and the
durability-class separation of
[ADR-0024](0024-separate-databases-storage-foundation.md). Arising from
[the 2026-07-05 project assessment](../../reviews/2026-07-05-project-assessment.md)
§5 ("Two source-of-truth patterns now coexist").

## Context

The runtime's stated stance is "NATS is the source of truth; the SQLite
projection is a rebuildable cache." In practice that stance holds only
inside a 30-day window and then fails silently:

- `fq-events` is `RetentionPolicy::Limits` with `max_age` = 30 days
  (`bus.rs:54`): past the window NATS **discards** the messages, so the
  documented `deliver_all` rebuild can only reach as far back as the
  surviving stream.
- The projection is **metadata-only** — denormalised scalar columns, no
  payloads (`projection/store.rs`). So full event bodies (system prompts,
  `llm.request` message history, `tool.result` output) live in exactly one
  place, under that 30-day clock, and cannot be reconstructed from the
  projection at any age.
- The archive hand-off persists only a final-phase + final reducer-state
  blob + timestamps per invocation — **not the event trail** — and itself
  expires (a doc/code drift to reconcile: 7 days in the docs, 30 in
  `config.rs`).

Net: past the retention horizon, only per-event **metadata** survives;
payloads and full history are unrecoverable. Meanwhile fq-store's grants
subsystem (M2) chose the inverse, sounder pattern — a relational log of
record with NATS as fan-out behind a durable outbox — so the two services
now embody opposite doctrines.

Two facts shape the fix:

1. **The runtime already trusts local storage as source of truth where it
   matters most.** Crash recovery is WAL-based against the worker's own
   SQLite (`WorkerStore`), entirely independent of NATS retention. The
   audit trail is the *one* domain still leaning on NATS-as-log. This is an
   internal inconsistency, not a whole-service philosophy.
2. **Event payloads are quadratically redundant.** An `llm.request` carries
   the full message history each turn, so within one invocation the request
   payloads repeat their prefixes: a k-turn invocation stores ~O(k²) bytes
   of which ~O(k) is new. This is the worst case for any undeduplicated
   store — SQLite, Postgres, or an ever-growing JetStream file store alike.
   Compression blunts it; only content-defined chunking collapses it.

## Decision

**NATS is transport; a dedicated archive service is the indefinitely
persistent system of record.** Concretely:

1. **NATS is demoted to transport.** It stays the event bus (ADR-0011's bus
   role is untouched), but its retention window becomes a *recovery*
   parameter — sized to cover the archive consumer's maximum downtime, not
   a history-retention horizon. It can shrink (days, even hours). No
   consumer treats replay-from-NATS as a durability guarantee.

2. **A dedicated archive service is the system of record.** It consumes the
   full event stream through a durable pull consumer, persists each event's
   full payload, and **acks only after the payload is durably stored**
   (at-least-once; duplicates deduped on `event_id`). It is append-only and
   **GC-free**: every event is permanent, so every payload reference is
   permanently live — the reclaim path never runs.

3. **Payloads are stored content-addressed, on the CAS.** The CDC-chunked
   content store collapses the quadratic within-invocation prefix
   redundancy: shared history chunks to identical blocks and is stored
   once, and at-least-once recovery duplicates are absorbed for free
   (idempotent store-if-absent). The archive pairs the CAS (payload bytes)
   with its own small ordered **event index** (`seq`, `event_id`,
   `invocation_id`, `subject`, `timestamp`, payload CID, cost, tokens) — the
   index is the archive's domain, not the CAS's.

4. **The CAS is extracted into a shared crate** — the dedup engine only
   (chunk, hash, store-if-absent), knowing nothing of grants, embeddings,
   or events. This is the pivot: it lets fq-store (document store) and the
   archive (event store) each pair the CAS with their own index without
   either depending on the other's domain.

5. **Two binaries.** The archive is its own service, not fq-store run in a
   second mode. The event firehose (high write rate, append-only, no
   retrieval) and the document store (read/retrieval-latency-sensitive)
   have genuinely different profiles; separate binaries give throughput
   isolation, independent evolution, and independent blast radius. The
   shared crate underneath keeps each binary's surface minimal.

6. **The projection is demoted to a rebuildable read-model whose rebuild
   source is the archive, not NATS.** With that, NATS exits the *durability*
   story entirely: it transports, the archive is authoritative, the
   projection is a fast queryable view rebuildable from the archive. "The
   projection is rebuildable" becomes true again — and permanent. (The
   projection's *live* source stays NATS — see decision 7.)

7. **The archive is off every critical path (invariant).** "System of
   record" is a *durability* role, not an availability or hot-path one, and
   the design keeps them apart. The write path rides NATS + the worker WAL —
   both already able to proceed with the archive down — so the archive is
   never on the write path. Live reads are served by **read models, never
   the raw archive**: concretely, the metadata projection tails **NATS live**
   for its hot path and touches the archive only for **cold rebuild /
   backfill**, and the archive's own read surface serves only cold rebuild
   and on-demand payload fetch (both rare, both latency-insensitive). The
   standing rule: any critical-path read is served by a replaceable,
   rebuildable read model — never by the archive directly. This is what
   keeps the archive's single-node availability from ever becoming the
   system's, and it forecloses the obvious future temptation (e.g. Memory
   reading invocation history *during* an invocation) from silently
   coupling live execution to the archive.

## Rationale

- **The substrate matches the workload.** Quadratic redundancy makes any
  undeduplicated store the worst case; content-defined chunking is the
  precise remedy, and we already built and tested one. GC-off removes that
  store's one throughput contention (reclaim-vs-write), so the archive
  reuses the CAS's clean half.
- **Retention stops being a latent bug and becomes a tuning knob.** Once a
  downstream copy is permanent, NATS retention need only exceed the
  archive's maximum outage. The 30 days becomes slack, not a correctness
  horizon.
- **A single global consumer yields a canonical total order for free** —
  JetStream's per-stream sequence gives the system of record a well-defined
  global order without merging per-worker logs.
- **It resolves the internal inconsistency**, not just the cross-service
  one: the audit trail now follows the same local-source-of-truth principle
  the execution WAL already does.
- **Two binaries on a shared crate buy the seam cheaply.** The value is the
  boundary (independent evolution), not scale we don't have; the crate
  makes the boundary real while keeping each service lean.

## Consequences

- **First shared crate in the repo.** fq-store and fq-runtime are today
  disjoint Cargo workspaces with no shared code. Extracting the CAS forces
  a small workspace-topology decision (where the shared crate lives). This
  is also the first real cross-service integration — the step
  [assessment critique #2](../../reviews/2026-07-05-project-assessment.md)
  was implicitly asking for.
- **A new cross-boundary seam: the projection rebuild path.** With the
  archive as a separate service, the projection rebuilds *from the archive*
  across a service boundary — a replay/query API to be designed once this
  ADR lands (see Open questions).
- **A delivery-guarantee constraint replaces the retention worry.** NATS
  retention must exceed the archive consumer's maximum downtime plus
  margin; at-least-once delivery plus `event_id` dedup make redelivery
  safe. An archive outage longer than the window is a gap — surfaced as an
  operational alarm, not silent data loss.
- **Ordering holds at current scale; horizontal scaling is deferred.** One
  consumer / one instance carries the total order. Scaling the archive out
  later must preserve per-partition order — a problem for a load we do not
  have.
- **The archive's HA need is durability, not consensus — so we never
  rebuild NATS's hard half.** NATS already owns the distributed,
  high-throughput write path; the archive must not try to re-earn that. It
  doesn't have to: because it is append-only and immutable, it is made
  durable by *replicating immutable data* (CAS objects to a second disk /
  object store; the index via streaming replication), not by distributed
  consensus over mutable state. The single active writer is a *feature* — it
  mints the canonical `seq` order — so the archive is deliberately not
  multi-master (a multi-master archive would reintroduce the ordering
  problem decision 2 removed). If read availability or ingest throughput
  ever demand more, the shape is a warm-standby-promotable writer plus read
  replicas, and — for throughput — sharding by agent/subject (per-shard
  order preserved, global order relaxed). All categorically simpler than
  NATS clustering, and deferred until real load demands it; content-dedup
  also *lowers* archive write volume below the raw event rate.
- **Reconcile in implementation:** the projection stops being
  authoritative-by-nobody, and the archive-retention doc/code drift (7 vs
  30 days) is retired — the archive no longer expires at all.
- **Scope discipline:** this establishes the boundary now and defers the
  scaling (single instance, single consumer) until throughput demands it.

## Open questions (deferred by decision)

1. **Compliance / targeted-deletion posture.** A content-addressed, deduped
   system of record makes targeted erasure (e.g. GDPR-style "delete this
   agent's history") genuinely hard — a chunk shared across many events
   cannot be deleted in place; the standard answer is per-subject
   encryption with key-shredding (crypto-shred). **Noted and deferred**: a
   known consequence of the dedup substrate, to be decided when a real
   erasure requirement lands.
2. **The projection rebuild API surface** (archive → projection). Topology
   now settled (decision 7): the projection tails NATS live and pulls from
   the archive only for cold rebuild/backfill, keeping the archive off the
   live path. The leading design is a cursor read over the archive's dense
   append position — `read_from(after_seq, limit) -> {rows, next_seq,
   caught_up}` (exclusive-after; the cursor is the archive's `seq`, not the
   producer-assigned `event_id`), which makes rebuild resumable in segments
   and, with idempotent apply on `event_id`, crash-safe — plus an on-demand
   `get_payload(cid)` for the rare full-fidelity consumers. Remaining
   mechanics (transport, batching, whether NATS also serves as a pull
   wake-up) to be finalized when the archive is built.
3. **Worker-side durable outbox** (event durable locally *before* NATS,
   surviving even total NATS loss). Not adopted now: it would trade away the
   clean single global order for stronger delivery. Filed as "later, if the
   recovery-window guarantee proves too weak."
