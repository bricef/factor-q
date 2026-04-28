# Data Architecture Requirements

## Status

Draft. Requirements-gathering only. Deliberately proposes no
stores, no schemas, and no solutions. The point is to make the
problem space visible before any persistence-related code or
infrastructure choice is committed to.

This document exists because durable suspension came up as a
near-term need and the first cut at solving it
([context](./2026-04-19-design-assessment.md))
anchored on a single store before surveying the broader data
architecture. Solving one persistence problem in isolation
risks locking in answers that fight the next four problems
([backlog](../plans/backlog.md)). This doc structures the
broader picture so the next decision is made across the full
surface.

## Audience and scope

For a designer or contributor who is about to make a decision
that introduces persistence — a new feature that needs to
remember something, a new background task that needs to wake up
later, a new store backing some subsystem. Read it before
choosing.

In scope: anything factor-q itself produces or consumes that
must outlive a single process invocation.

Out of scope: data inside MCP services (memory, per
[ADR-0013](../adrs/accepted/0013-memory-as-mcp-services.md)),
data inside agent-spawned tools (their problem, scoped by the
sandbox), and the contents of the LLM provider's own systems.

---

## 1. Data shapes

The shapes factor-q has data in. Each shape has a characteristic
write/read profile that points to a store family — a relational
DB, a log, a KV, a blob store, a filesystem. Two shapes may end
up in the same physical store (good operability) or in different
stores (good fit). The point of enumerating them is to make
that mapping a deliberate choice rather than an accident.

| Shape | Description | Write profile | Read profile | Size profile | Durability requirement |
|---|---|---|---|---|---|
| **Append-only audit log** | Every event factor-q emits, ordered, with a stable subject hierarchy. | Bursty during invocations; idle otherwise. Append-only. | Live tail (real-time consumers) and bounded historical query (recent debugging). | Per-message ~KB. Per-invocation ~tens of messages. | Operator-set retention (currently 30 days for events, 24h for triggers). |
| **Work queue** | Pending agent triggers awaiting dispatch. | Low (one per trigger). | Single-consumer, exactly-once dispatch. | Per-message ~hundreds of bytes. | Until consumed; capped retention as a safety net. |
| **Queryable projection** | Aggregated, denormalised view over the audit log: who ran when, what cost, what failed. | Driven by the projection consumer; idempotent on event_id. | Operator queries ("costs", "events query"), dashboarding. | Bounded by audit-log retention. | Rebuildable from the audit log; no independent durability requirement. |
| **Latest state per invocation** | Invocation-scoped state that must survive between steps if an invocation pauses, suspends, or crashes. | Read-modify-write per step. One writer per key. | On resume / on operator inspection. | Variable. Small in the common case (KB), bounded by `max_iterations × tool_output_size` (could reach MB). | Until the invocation reaches a terminal status; debatable grace period after. |
| **Pending approvals / waits** | An invocation is paused waiting for an external signal (human sign-off, scheduled time, webhook). | One write at suspension; one update on resolution. | When the signal arrives, or operator inspection. | Small (~KB) plus a pointer to the invocation state shape above. | Until the wait resolves or is cancelled. May be days. |
| **Schedules / wakeups** | Future-firing entries: "at time T, trigger agent X" or "at time T, refresh pricing." | Low. Cron-shaped or one-shot. | A scheduler reads upcoming entries continuously; fires entries when due. | Small per row; row count grows with cron entries. | Until fired or cancelled. |
| **Per-invocation workspaces** | Filesystem an agent's tools work against during an invocation. May be empty, may be a pre-loaded base layer plus deltas. | Tool-frequency: many small writes. | Tools read what they wrote and what was pre-loaded. | Highly variable. Small for "no-fs" agents; could reach hundreds of MB for code-handling agents with checked-out repos. | Lifetime of the invocation, plus optional snapshotting for replay/audit. |
| **Static configuration** | `fq.toml` — runtime configuration. | Operator edits, runtime never writes. | Read at startup, occasionally re-read on hot-reload. | KB. | Operator-managed. |
| **Agent definitions** | Markdown files declaring agents. | Operator edits today. Hot-reload (backlog) doesn't change ownership. | Read at startup; potentially re-read on file changes. | KB per agent. | Operator-managed. Future: agents authoring agents would change this. |
| **Caches** | Fetched data with a backing source of truth elsewhere — pricing JSON from LiteLLM, possibly model schemas, possibly future model-specific quirks. | Periodic refresh. | Hot path during invocation. | KB to low MB. | Rebuildable from network on miss; durability is "convenience-only". |
| **Secrets at rest** | Anything sensitive that ends up persisted: a tool returned a credential, an agent received PII, a user typed an API key. Currently *also* the contents of conversation history insofar as it overlaps with this. | Inherited from whatever shape produced it. | Inherited. | Inherited. | A retention/deletion policy concern more than a storage concern. |
| **Cost/budget windows** | Running totals per agent, per time window (daily, monthly), used for budget enforcement and reporting. | Implicit — derived from the audit log via the projection. | Budget gate before/after each LLM call. Reporting via `fq costs`. | Bounded. | Rebuildable from the audit log. |

Shapes most worth pulling on:

- **Latest state per invocation** and **Pending approvals / waits** look like the same shape with different metadata. They probably share an answer.
- **Schedules / wakeups** has a queue-of-future-events shape that overlaps the work-queue shape but has different read semantics (time-driven, not consumer-driven). It might share a store or might not.
- **Per-invocation workspaces** are the only shape that involves large blobs. Almost certainly wants its own answer.
- **Caches** are the only shape where durability is optional. Important not to over-engineer them.

---

## 2. Categories of persistence problem

Mapping the open problems on factor-q's roadmap to the shapes above. The point is to expose where multiple problems share an answer, and where they don't.

| Problem (open) | Source | Shape | Notes |
|---|---|---|---|
| Durable suspension of in-flight invocations | discussed during reducer prototype | Latest state per invocation | The motivating example. |
| Approval gates | [`ARCHITECTURE.md` cross-cutting concerns](../../ARCHITECTURE.md#security-and-access-control) | Pending approvals / waits | Same shape as suspension plus metadata about what's needed. |
| Scheduled refresh of pricing data | [backlog](../plans/backlog.md) | Schedules / wakeups | Tiny, low-frequency, but durable — pricing should refresh even if the operator restarts. |
| Scheduled agent triggers | [backlog](../plans/backlog.md) | Schedules / wakeups | Same shape; user-facing rather than runtime-internal. |
| Workspace snapshotting | [`tool-isolation-model.md`](./tool-isolation-model.md) backlog | Per-invocation workspaces | Likely needs its own store; large blobs. |
| Hot-reload of agent definitions | [backlog](../plans/backlog.md) | Static configuration (read-side) | Doesn't introduce new persistence; introduces a watcher. |
| Long-lived agent waits (e.g. webhook receivers) | [`agent-os-architecture.md`](./agent-os-architecture.md) | Latest state + a wakeup trigger | Composite — needs both the suspension shape and the wakeup shape. |
| Crash recovery | discussed during reducer prototype | Latest state + a "what was in-flight" index | Composite — uses suspension state plus a way to enumerate it on startup. |

**What this groups into:**

- A *single* persistent latest-state-per-invocation answer covers durable suspension, approval gates, long-lived waits, and crash recovery — most of the immediate roadmap.
- A *single* schedules/wakeups answer covers pricing refresh, scheduled triggers, and the trigger half of long-lived waits.
- Workspace snapshotting is its own thing.

That clustering is itself a decision point. Forcing a single answer where shapes diverge has a cost; allowing many answers where shapes are the same has a different cost (operational complexity, more things to back up, more migration paths).

---

## 3. Forces and constraints

The non-functional requirements that any answer has to honour.
Listed roughly in order of how strong a constraint each is.

| Force | Source | Implication |
|---|---|---|
| **Single-tenant, self-hosted** | [`VISION.md`](../../VISION.md) | No tenant isolation requirements. No multi-tenant rate-limiting. Operator owns the deployment end-to-end. Stores can assume a single trust boundary. |
| **Single-node default, multi-node aspiration** | [`ARCHITECTURE.md`](../../ARCHITECTURE.md) | Single-node must work without coordination overhead. Multi-node should not require a redesign — but designing for multi-node *now*, before any node is overloaded, is premature. |
| **No timeline pressure; optimise for correctness** | project posture | Permits choosing slightly more careful foundations over minimal-viable. Does not justify designing for hypothetical scale or future tenants. |
| **Local-dev simplicity** | [`QUICKSTART.md`](../../QUICKSTART.md) promises `just up` works | Adding required infrastructure (a Postgres, a Redis, an object store) degrades the "clone and run" property. Trade-off must be argued explicitly. |
| **Operator backup-and-recovery story** | implicit in self-hosted posture | Each new store adds a backup path. Operators reasonably expect a small number of clearly-named things to back up. |
| **Audit trail is sacred** | [`event-schema.md`](./event-schema.md), the projection's "rebuildable from events" invariant | Anything that is a projection of events must remain rebuildable. Anything that is *not* a projection of events is by definition new source-of-truth and must be backed up. |
| **NATS is already load-bearing** | implementation | Adding a second event-bus-shaped store is a hard sell. NATS already has the durability story for the audit log. |
| **SQLite is already load-bearing for queries** | implementation | Adding a second relational store is a hard sell. Single SQLite db as the operator-queryable surface is already accepted. |
| **Tooling is in-process today** | implementation | All persistence is currently process-local except NATS. Multi-process coordination is a step change in operational complexity. |

What this rules out without further discussion: managed cloud-only stores, anything that requires running multiple databases by default, anything that breaks the "events are the source of truth" invariant for things that are projections.

What it permits: choosing a *different* store family for a *different* shape, as long as the operator-experience cost is justified.

---

## 4. Cross-cutting questions

These are not store-choice questions. They are *contract* questions
that determine what any store is being asked to do. None of them
can be answered by surveying tools or comparing databases. All of
them constrain the solution space in load-bearing ways.

### 4.1 Tool idempotency contract

What does the runtime promise about how many times a tool is
dispatched? Three plausible answers, each cascading into
different persistence shapes:

- **At-most-once**: the runtime persists the dispatch decision
  *before* invoking the tool. On crash recovery, the persisted
  decision is honoured rather than re-dispatched. Forces durable
  writes on the hot path.
- **At-least-once**: tools must be idempotent or coordinate
  externally. Crash recovery may re-dispatch a tool that already
  ran. Persistence shape is much smaller, but the contract on
  tool authors is heavier.
- **Per-tool declaration**: each tool declares its idempotency
  posture. Runtime gates the persistence cost on tools that need
  it.

This single answer changes whether durable suspension persists
`(state, last_result)` or `(state, pending_action)`. It also
governs how approval gates and crash recovery interact with
side-effecting tools.

#### What "at-most-once" actually buys

Strict at-most-once delivery to an external system is provably
impossible without cooperation from that system. A WAL alone
cannot disambiguate two failure modes:

| Crash point | WAL state | Re-dispatch is | Don't re-dispatch is |
|---|---|---|---|
| After persisting intent, before dispatch | `intent`, no `completed` | correct (tool runs once) | wrong (work lost) |
| After dispatch, before persisting completion | `intent`, no `completed` | wrong (tool runs twice) | wrong (downstream effect we can't see) |

Same WAL state, opposite correct actions. So a WAL by itself
gives **at-most-once-or-flagged-as-ambiguous**, not at-most-once.
On recovery the runtime can act mechanically on unambiguous
cases (intent without dispatch attempt → safe to retry; full
completion record → safe to skip) and surface the genuinely
ambiguous middle case to the operator. That's a defensible
contract for a single-tenant self-hosted system, but it has
to be named explicitly — "at-most-once" without qualification
overpromises.

#### Three-state record, not two

To make the WAL useful, the runtime probably needs three
durable states per dispatch, not two:

| State | Recorded when | Recovery behaviour |
|---|---|---|
| `intent` | Before any side effect (network call, syscall) | Safe to re-dispatch — nothing happened. |
| `dispatched` | After the tool returns control to us, before we trust the result | Ambiguous — flag to operator. |
| `completed(result)` | After the result is durably written | Safe to skip — already done. |

Today's events have only `tool.call` (≈ `intent`) and
`tool.result` (≈ `completed`). The middle state isn't recorded.
Adding it is a precondition for any honest at-most-once-or-flagged
contract.

#### Is the existing audit stream the WAL?

`tool.call` is already published with synchronous JetStream
ack before `tool.execute()` runs. So intent is already durable.
If we added a `tool.dispatched` event between `tool.call` and
`tool.result`, the existing event stream would carry the WAL
semantics natively — no second store. Attractive, but it
conflates two purposes:

- The audit log is operator-facing, retention-bounded,
  optimised for replay.
- The WAL is recovery-state, hot-path-critical, must survive
  at single-event granularity even after older audit data
  rotates out.

These can probably share a stream with care, but it's a real
decision, not a free choice.

#### The boundary isn't sharp

"Internal tools we control are idempotent; external tools
aren't" is tempting but wrong. Among our own built-ins:

| Tool | Idempotent? |
|---|---|
| `file_read` | Yes (read-only). |
| `self_inspect` | Yes (read-only synthesis). |
| `file_write` | No (overwrites). |
| `shell` | No (arbitrary side effects). |

So a contract designed around "no idempotency assumed" applies
to almost every tool we already ship, not just hypothetical
third-party MCP tools.

#### LLM calls have the same shape

LLM calls also touch external systems and cost money. Crashing
mid-LLM-call leaves the same ambiguity as crashing mid-tool
(call may or may not have been billed; response may or may not
exist). Whatever WAL semantics we pick for tools should also
apply to LLM calls — otherwise the runtime has a clean recovery
story for tools and an opaque one for LLM calls, which is the
wrong asymmetry.

#### Idempotency keys are additive, not redundant

Once the WAL exists, tools or external systems that *do*
support idempotency keys (Stripe-style) layer on top: the
runtime generates a stable key per `(invocation_id,
tool_call_id)`, re-issues with the same key on recovery, and
the cooperating external system deduplicates. The runtime can
ship without this and add it later without redesigning the
WAL — but only if the WAL already records a stable
`tool_call_id` per dispatch (it does).

### 4.2 Secret handling in persisted state

What ends up in invocation state — conversation history, tool
outputs — is currently arbitrary text. If a tool returns a
credential, or a user pastes one in, it lives in the state blob.
Once we persist state durably:

- Encryption at rest? At what layer (store, factor-q, tool)?
- Retention horizon? Forever, with the audit trail? Bounded?
- Right-to-delete? Operator-driven scrub of an invocation's full
  trail (state + audit) on request?
- Access control? Today, operator owns everything. Future
  multi-operator deployments?

These decisions have store implications (some stores support
encryption-at-rest natively, others don't) and contract
implications (what tool authors can rely on).

### 4.3 Cross-store consistency boundaries

When two stores hold related data and a crash happens between
their writes, the invariant has to be operator-comprehensible.
For each pair of stores in any candidate architecture:

- What's the "winner" if they disagree?
- What's the operator-visible consequence of disagreement?
- Are there compensating actions the runtime can take on
  startup to detect and reconcile?

Today there are exactly two stores (NATS, SQLite) and the
contract is "SQLite is rebuildable from NATS, so NATS wins by
construction." Any new store breaks this property unless
explicitly designed not to.

### 4.4 Retention and deletion

When can persisted state be deleted? Plausible policies:

- On invocation `Completed` or `Failed` — state is redundant
  with the audit trail.
- After a grace period — operators can replay or debug recent
  invocations from state directly.
- Never — state is part of the permanent record.
- Operator-driven — operators run a `gc` command.

Each policy implies different store sizing, different backup
loads, and different debug surfaces.

### 4.5 Backup unit

What does "back up factor-q" mean? Plausible answers:

- The audit-trail store only; everything else is rebuildable or
  ephemeral.
- The audit-trail store plus one other (config? state?).
- Every store, coordinated, as a single point-in-time snapshot.

Self-hosted operators ask this question early and reasonably
expect a clean answer. Adding stores without naming the answer
adds operator burden.

### 4.6 Schema evolution

When the runtime version updates and a persisted shape changes,
what happens to in-flight data?

- Can a v1.0 daemon resume an invocation suspended by a v0.9
  daemon? If yes, by what migration path? If no, what's the
  operator's recovery story?
- Are persisted shapes versioned alongside the binary, or
  separately?
- Is "in-flight data is invalidated by version mismatch" an
  acceptable answer?

This is an unsexy concern but it is the difference between a
runtime that operators trust to upgrade and one they don't.

### 4.7 Persistence latency vs durability mode

A blanket "every write is durable before we proceed" rule
makes the contract simple but pays a synchronous-write cost
on every step boundary and every tool dispatch. With local
storage that's milliseconds; with remote storage stacked across
a tool-heavy invocation it can be hundreds of milliseconds of
added latency. Most database systems address this by offering
**configurable durability modes** rather than a single default,
on the principle that some workloads accept bounded data loss
in exchange for lower latency.

Plausible modes for factor-q:

| Mode | Semantics | Crash-loss bound | Use case |
|---|---|---|---|
| **Sync** (default candidate) | Each write is durable before the runtime proceeds to the next step. | Zero unflushed records on crash; ambiguity bounded to the in-flight middle state described in §4.1. | Production runs where re-execution risk must be minimised. |
| **Group-commit** | Writes batch up to a threshold (N records or T milliseconds, whichever comes first). | Up to one batch window of records lost on crash. | Tool-heavy invocations where individual-write latency matters but operators accept a bounded loss. |
| **Async per-invocation** | Records buffer in-memory for an entire invocation; flush on terminal status. | Up to one invocation's full record on crash. | Low-stakes invocations (development, throwaway agents) where per-step durability isn't worth the latency. |
| **Per-tool differentiated** | Idempotent / read-only tools use async; side-effecting tools use sync. | Tool-class-dependent. | Future enhancement once tools declare their idempotency posture. |

What a proposal needs to commit to:

- **Default mode.** "Most systems opt for async" is true at the surface but misleading: most systems opt for *configurable* async with a quantified loss bound, and default to sync. The default factor-q ships needs an explicit answer.
- **Loss bound, named.** "Degraded resiliency" is too vague to be a contract. The bound has to be expressible to an operator: "in mode X, you can lose up to N seconds of records on crash" or "in mode X, you can lose up to one full invocation."
- **Per-agent override?** An agent definition might want to declare its preferred mode (a long-running approval-gated agent wants sync; a hot one-shot agent might prefer group-commit). Or modes might be runtime-global. Trade-off in operator surface area.
- **Interaction with §4.1.** Async durability with a non-idempotent tool means the WAL entry might land *after* the tool side-effect did. On crash recovery, the WAL could be missing intent records for tools that already ran — meaning the recovery code can't even flag the ambiguity. This composes badly with the at-most-once-or-flagged contract from §4.1 unless the modes are specified together.

### 4.8 Multi-process / multi-node ownership

If we eventually run multiple host processes, who owns the
write-lock for an invocation? Lease-based? Coordinator-based?
Single-writer-per-key by construction (e.g. NATS subject
ownership)?

This is in the "design for the shape, not the future" bucket —
an answer is needed before multi-node ships, but designing for
it now means specifying a coordination protocol nobody is going
to use for months. The relevant decision today is:
*don't accidentally choose a store that makes multi-node
impossible.*

---

## 5. What's already decided

These are not up for re-litigation in this document. They
are constraints, not choices.

| Decision | Source |
|---|---|
| NATS+JetStream is the source of truth for the agent audit log and the trigger work queue. | [`ADR-0011`](../adrs/accepted/0011-event-bus-and-persistence.md) |
| SQLite holds the queryable projection over the audit log. | [`event-schema.md`](./event-schema.md), implementation in `fq-runtime/src/projection/`. |
| The projection is **rebuildable from events**. | implementation invariant; the projection consumer is idempotent on `event_id`. |
| Memory (long-term, collective) is delivered as MCP services, not built into the runtime's persistence. | [`ADR-0013`](../adrs/accepted/0013-memory-as-mcp-services.md) |
| Static configuration (`fq.toml`), agent definitions, and skills are filesystem files. | [`ADR-0005`](../adrs/accepted/0005-agent-definition-format.md), implementation. |
| Pricing data is fetched from a remote source and cached locally; the cache is rebuildable. | implementation. |
| Audit-log retention is operator-set; events default to 30 days, triggers to 24 hours. | [`bus.rs`](../../services/fq-runtime/crates/fq-runtime/src/bus.rs) defaults. |
| Secrets (API keys) are read from environment variables; factor-q itself does not write them anywhere. | [`fq.toml`](../../services/fq-runtime/crates/fq-cli/src/templates/fq.toml) provider section. |

---

## 6. What's not a force

Naming what we are *not* designing for, so that no candidate
solution accidentally pays for it.

| Non-force | Reason |
|---|---|
| Multi-tenant isolation | factor-q is single-tenant per [`VISION.md`](../../VISION.md). No data partitioning by tenant; no tenant-aware permission model in stores. |
| Cloud-managed deployment | Self-hosted is the deployment shape. Stores must be operable by a single operator, not require a managed-service provider. |
| Internet-scale throughput | Realistic scale: one operator, dozens to hundreds of invocations per day, peak bursts during agent fan-out. Anything that optimises for >10k writes/sec is over-engineered. |
| Real-time analytics | Operator-facing queries (`fq events query`, `fq costs`) are bounded and infrequent. No need for OLAP-shaped stores. |
| Geo-replication | Not on the roadmap. Multi-node, when it lands, is single-DC. |
| Public read API | Stores are accessed by the runtime and its operators, not external consumers. |
| Sub-millisecond persistence latency | Persistence sits next to LLM calls (hundreds of ms minimum) and tool calls (tens of ms minimum). Single-digit-ms write latency is sufficient; sub-ms doesn't pay for itself. |

---

## 7. Open questions, ranked

The questions whose answers most narrow the solution space.
Ranked by *how much one answer determines the rest of the
architecture*, not by how urgent the answer is.

1. **Tool idempotency contract.** ([§4.1](#41-tool-idempotency-contract))
   Answer determines whether persistence happens before or
   after action dispatch. Cascades into approval-gate design,
   crash recovery, and the state-shape decision.

2. **Backup unit.** ([§4.5](#45-backup-unit))
   Answer determines whether all persistence shares one store,
   splits along clear lines, or proliferates. Operator-facing.

3. **State retention policy.** ([§4.4](#44-retention-and-deletion))
   Answer determines whether state can be ephemeral
   (deduplicated against the audit trail) or must be a
   long-lived store with its own retention semantics.

4. **Default durability mode and named loss bound.** ([§4.7](#47-persistence-latency-vs-durability-mode))
   Answer determines whether the runtime ships sync-by-default,
   group-commit-by-default, or async-by-default, and what the
   operator-visible loss bound is. Composes with §4.1 — async
   modes weaken the WAL guarantees there.

5. **Multi-node ambition timeline.** (interaction of §4.8 with
   the "multi-node aspiration" force)
   Answer determines whether we accept stores that are
   single-node-only today.

6. **Workspace size profile.** ([§1, workspaces row](#1-data-shapes))
   Answer determines whether workspace storage is a different
   problem (large blobs, separate store) or the same problem
   (small enough to live alongside invocation state).

7. **Approval-gate timeline expectations.**
   Answer determines whether long-lived waits are
   weeks-and-months durable (real backup concern) or
   minutes-and-hours (operational concern only).

These are deliberately *not* answered in this document. The
expectation is that answering them produces enough constraint
that the next document — a data architecture proposal — has a
narrow solution space to choose from, with the trade-offs already
named.

---

## What this document is not

- **Not a solution.** No store is recommended, no schema is
  proposed, no API is sketched.
- **Not exhaustive.** The shapes and forces listed here are the
  ones visible today. New ones will surface as features land.
- **Not stable.** This is a working document; revising it as
  questions get answered or new requirements emerge is expected.
- **Not a promise about ordering.** Listing a problem in
  [§2](#2-categories-of-persistence-problem) does not mean it
  will be addressed before others.

The next document, when written, should:
- Reference this one for the requirements baseline.
- State decisions for the open questions in [§7](#7-open-questions-ranked).
- Map shapes from [§1](#1-data-shapes) to specific stores.
- Address each cross-cutting question in [§4](#4-cross-cutting-questions) with a concrete contract.
- Justify any deviation from the constraints in [§3](#3-forces-and-constraints).
