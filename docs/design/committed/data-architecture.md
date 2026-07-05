# Data Architecture

## Status

Draft proposal. Builds on
[`data-architecture-requirements.md`](./data-architecture-requirements.md);
references that document for the requirements baseline rather than
re-deriving it. Where the requirements doc surveys the open
solution space, this doc *commits* — to a contract, to a role
model, to a set of stores, to a backup unit, and to a named
recovery semantics.

The document lands once the open questions in
[§12](#12-what-this-doc-leaves-open) are resolved or explicitly
deferred, and once a first slice (durable suspension on a
single-node deployment) is built and operated against it.

## 1. What this document is

A single, committed answer to the questions raised in the
requirements doc, in the order they cascade:

1. The committed constraint that drove every other decision
   ([§2](#2-the-committed-constraint-tool-idempotency)).
2. The role boundary: control-plane and worker
   ([§3](#3-the-role-boundary-control-plane-and-worker)).
3. The runtime's contract with tool authors and operators
   ([§4](#4-the-runtime-contract)).
4. Decisions on the cross-cutting questions from
   [requirements §4](./data-architecture-requirements.md#4-cross-cutting-questions)
   ([§5](#5-decisions-on-cross-cutting-questions)).
5. The mapping from data shape to physical store
   ([§6](#6-shape-to-store-mapping)).
6. The recovery semantics on host restart
   ([§7](#7-recovery-semantics)).
7. The operator's backup story
   ([§8](#8-backup-unit)).
8. Schema additions: tables, events, contracts
   ([§9](#9-schema-additions)).
9. Implementation order for the v1 slice
   ([§10](#10-implementation-order)).
10. The v1-to-v2 evolution path
    ([§11](#11-v1-to-v2-evolution-path)).
11. What the doc leaves open for follow-ups
    ([§12](#12-what-this-doc-leaves-open)).

The doc is opinionated. Where alternatives were considered and
dismissed, the dismissal is named.

## 2. The committed constraint: tool idempotency

> The factor-q runtime cannot assume any tool — built-in or
> third-party — is safe to re-dispatch on recovery.

This is the load-bearing constraint. Tools span built-ins, MCP
servers, and arbitrary external integrations; idempotency cannot
be guaranteed across that surface. The guarantee must come from
the runtime's behaviour, not from a tool-author contract.

What this dismisses:

| Dismissed | Why |
|---|---|
| At-least-once tool dispatch | Re-dispatch on recovery is correctness-breaking under the constraint. |
| Async-per-invocation durability mode as default | Loses WAL records on crash; recovery cannot even flag the ambiguous case. |
| Group-commit durability as default | Same shape, smaller window. Same problem. |
| Auto-retry on tool failure (runtime layer) | Auto-retry assumes safe re-dispatch. The runtime cannot make that assumption. |
| Pure event-stream-as-WAL with async ack | Same loss-of-records problem. |
| Any persistence design that requires tools to be idempotent | Dismissed by construction. |

What this forces:

| Forced | Consequence |
|---|---|
| At-most-once-or-flagged-as-ambiguous dispatch contract | Per the requirements doc §4.1. |
| Three-state WAL record per dispatch | `intent` / `dispatched` / `completed`. |
| Sync durability as the runtime default | No async modes in v1. |
| Operator-visible surface for ambiguous middle-state cases | A control-plane responsibility. |
| Same WAL contract for LLM calls | Same crash shape; same accounting. |

Idempotency keys remain useful as an *additive opt-in*: tools
that support them let the runtime turn ambiguous cases into
safe-retry. Not required, not load-bearing for v1.

## 3. The role boundary: control-plane and worker

factor-q's runtime separates into two roles. v1 collapses them
into a single `fq run` process for deployment simplicity; the
internal interface is real and the v2 split is a deployment
change rather than a redesign.

### 3.1 Roles defined

**Control-plane**: the global view. Responsible for:

- Trigger ingestion from NATS and routing to a worker.
- Projection over the audit log; operator queries hit here.
- Schedule / cron entries (pricing refresh, scheduled triggers).
- Coordination state: which workers exist, which invocations
  are claimed by which worker, which invocations are stuck or
  ambiguous globally.
- Pending waits: approval gates, time-based wakeups, webhook
  receivers. Control-plane fires them when the signal arrives.
- Completed-invocation archive (after migration from the
  worker that ran them).
- Operator-facing CLI/HTTP endpoints.

**Worker**: where work happens. Responsible for:

- Claiming invocations the control-plane routes to it.
- Running the reducer-runner host loop for claimed invocations.
- Owning local in-flight reducer state and the WAL for those
  invocations (intent / dispatched / completed records).
- Owning the per-invocation workspace dir on local disk.
- Executing tool calls (in-process built-ins, MCP, shell).
- Issuing LLM calls and recording them through the WAL.
- Reading pricing / agent / config from a control-plane-
  published source (or a local cache thereof).
- Publishing lifecycle events to NATS as it goes.

### 3.2 Why this separation

Five reasons it pays its weight:

1. **Operator mental model.** "Where do I query things?" →
   control-plane. "Where does work happen?" → worker. The audit
   log is still source of truth; the projection of it lives in
   one well-named place.

2. **Scaling characteristics differ.** Control-plane is
   read-heavy, coordination-heavy, low-throughput. Workers are
   I/O-heavy, write-heavy, scaling with concurrent invocations.
   Different sizing, different replication needs.

3. **Failure modes decouple.** A worker crash loses its in-flight
   invocations — recoverable per the WAL contract. Operator
   queries and trigger routing keep working via the
   control-plane. A control-plane crash means no new triggers
   dispatch, but in-flight invocations on workers continue.
   Either failure has bounded consequences.

4. **Backup splits cleanly.** Control-plane backup = projection
   DB + coordination state + schedule. Worker backup = in-flight
   state + workspaces. Each artefact serves a clear recovery
   scenario.

5. **Composes with v1.** In v1 the two roles share a process,
   but the boundary is real. v2 lifts the worker out as a
   separate `fq run --role=worker` deployment without
   redesigning contracts.

### 3.3 Invocation locality (not project pinning)

An invocation is **claimed by exactly one worker for its
lifetime** and runs on that worker until it terminates. This is
*invocation-pinning*, not project-pinning. The reason is
workspace locality during execution: tool calls within an
invocation read and write the same workspace dir, and that dir
lives on the worker that's running the invocation.

What this is **not**:

- Not a per-project deployment boundary. Any worker can run any
  agent's invocations; the operator does not have to set up
  separate workers per project.
- Not a long-term affinity. Once an invocation completes, the
  worker forgets it. The next invocation of the same agent goes
  wherever the scheduler routes it.
- Not a hard constraint on agent placement. The scheduler can
  use agent declarations as preferences (e.g., "this agent
  prefers high-CPU nodes") but pinning is invocation-scoped.

What this **does** mean:

- The control-plane assigns each new invocation to a worker at
  trigger time. Once assigned, the assignment is immutable for
  that invocation's lifetime.
- A worker crash drops all in-flight invocations on that
  worker. Each is surfaced via the WAL contract: safe-cases
  resume on recovery; ambiguous cases require operator decision.
  Invocations cannot be migrated to a different worker mid-flight.
- Workspace storage is **local to the worker** that ran the
  invocation. Workspaces don't move. A worker that's
  permanently lost takes its workspaces with it; the operator
  decides what to do with the audit-trail-only record.

### 3.4 v2-only: the deployment shape

In v2, an operator runs:

- One control-plane process (HA pair optional).
- N worker processes, sized to the load.
- Shared NATS cluster.

Each worker registers with the control-plane on startup
(via NATS subject) and starts heartbeating. The control-plane
maintains the live worker membership view. Trigger routing
picks among healthy workers; ownership of an invocation is
recorded in coordination state.

In v1, a single `fq run` process plays both roles; the worker
membership view contains exactly one entry; trigger routing is
trivial. The contracts are exercised — they just don't do
much.

## 4. The runtime contract

What an operator can assume after a crash. Stated in the
declarative form an `fq` man page would use.

### 4.1 Tool dispatch

For every tool dispatch the runtime initiates, exactly one of the
following holds after recovery:

- **`intent` only** — the tool was *not* started. Safe to
  re-issue. Recovery proceeds automatically on the same worker.
- **`completed` recorded** — the tool ran and its result is
  durably stored. Recovery proceeds with that result. The tool
  is not re-issued.
- **`dispatched` without `completed`** — *ambiguous*. The tool
  may have run partially, fully, or not at all. The control-plane
  surfaces this case to the operator. **No auto-recovery.**

### 4.2 LLM calls

Same shape. LLM calls touch external systems and cost money;
crashing mid-call leaves the same ambiguity. The same
three-state record applies.

### 4.3 Reducer state

Between any two `step` calls, the reducer's state blob is
durable on the worker that owns the invocation. On worker
recovery (process restart, same host), the host loop resumes
from the last durable state with the appropriate `last_result`
value drawn from the WAL.

If the previous dispatch was ambiguous, the runtime stops and
surfaces the case rather than guessing a `last_result`.

If the worker is permanently lost (host failure, disk loss),
the in-flight invocation is unrecoverable. Its audit trail in
NATS is intact; the operator decides whether to mark it failed
or attempt manual reconstruction.

### 4.4 Operator surface for ambiguous cases

Ambiguous cases are surfaced by the **control-plane**, which
aggregates them across workers. The v1 surface is a set of
non-interactive subcommands rooted at `fq invocation` (per-
invocation triage) and `fq workers` (worker liveness), with
`fq status` summarising what needs attention:

```
$ fq status
...
Recovery state
  Ambiguous invocations: 2
    -> `fq invocation list --status=ambiguous` to inspect
    -> `fq invocation drop <id>` to triage individually
  Stale workers: 1
    -> `fq workers list --stale-only` to inspect

$ fq invocation list --status=ambiguous
invocation  status     agent                    worker     arch
inv_abc12   ambiguous  code-reviewer            w-001      no
inv_def34   ambiguous  doc-bot                  w-001      no

$ fq invocation drop inv_abc12 --reason "stuck on flaky network"
```

`fq invocation drop` publishes
`invocation.operator_recovered` (see event-schema.md). The
coordination consumer's handler writes an `invocation_archive`
row and updates the owner status to `Failed`. A late
`invocation.archived` from a still-alive worker is detected by
a no-downgrade guard and does not override the operator's
decision.

`resume` semantics are deferred from v1: the control-plane
doesn't currently carry the worker's `state_blob` for
ambiguous invocations, so an honest resume would either
require enriching `invocation.ambiguous` to carry the blob or
adding an operator-RPC to the worker. The contract — *that
ambiguity is operator-visible across the cluster and not
silently resolved* — is preserved by drop. Node-level
recovery (`fq recover`) for stuck workers / control-plane is
a follow-up; see the step-9 plan
(`docs/plans/closed/2026-05-22-operator-cli.md`) for the
scope decision.

### 4.5 What the contract does NOT promise

- No promise that any particular tool is safe to re-run. Tools
  declare their own semantics; the runtime treats them all as
  potentially side-effecting.
- No promise of zero ambiguous cases. The two-failure-modes-
  one-WAL-state proof in requirements §4.1 stands.
- No promise that an invocation can migrate between workers
  mid-flight. Workspace locality forbids it in v1/v2.
- No promise that the reducer's state survives a version
  upgrade across incompatible schema changes (see §5.6).
- No promise of secret-handling beyond what the OS filesystem
  provides (see §5.4).

## 5. Decisions on cross-cutting questions

In the order they cascade.

### 5.1 Tool idempotency contract — DECIDED

**At-most-once-or-flagged-as-ambiguous.** Three-state record
(`intent` / `dispatched` / `completed`) per dispatch, applied to
both tool calls and LLM calls.

The "dispatched" state is recorded *after the tool returns
control to the runtime, before the result is durably written*.
This is the genuine source of recovery ambiguity. The window is
small but cannot be eliminated.

### 5.2 Default durability mode — DECIDED

**Sync.** Each WAL transition is durable before the runtime
proceeds.

Named loss bound: in sync mode, on crash, you lose at most one
in-flight dispatch (the `dispatched`-without-`completed` row)
per affected worker. Recovery surfaces it as ambiguous; no
record is silently lost.

Async modes are deferred. They are not ruled out — a per-agent
opt-in is a defensible v3 addition. v1 ships sync-only.

Latency cost (named, not hand-waved): a sync write to local
SQLite is single-digit milliseconds on commodity SSDs. An
invocation with 5 LLM calls and 5 tool dispatches incurs
roughly 20 sync writes (intent + dispatched + completed × 2
per pair). ~100ms of added latency per invocation, sitting
next to LLM calls that take seconds. Acceptable.

### 5.3 State retention — DECIDED

**Delete on terminal status + 7-day grace period. Operator
configurable.**

Concrete:

- Invocation reaches `Completed` or `Failed` on a worker → state
  row marked `terminal_at = now()`.
- Worker emits `invocation.archived` to NATS. Control-plane
  consumer copies the final state into its archive table.
  Worker deletes its local state row after a configurable
  hand-off-confirmation window (default 60 seconds).
- Background sweep on the control-plane deletes archive rows
  where `terminal_at < now() - retention_days`.
- Operator override: `fq.toml` key `state.retention_days`
  (default 7).
- Operator-driven force: `fq invocation drop <id>`.

Why 7 days: long enough for the "I'll look at this Monday"
debug pattern, short enough that the archive stays small. No
correctness implication of the number.

### 5.4 Secret handling — DEFERRED, NAMED

State rows can contain conversation history with arbitrary text.
A tool that returns a credential, a user who pastes one in, an
LLM that echoes one back: all end up in the state blob.

**v1 stance:** state files are at-rest in plain SQLite.
Encryption is the operator's concern, handled by disk-level
encryption (LUKS, FileVault, BitLocker) on the host. factor-q
does not implement application-level encryption.

This is a deliberate non-decision rather than an oversight.
v1 addresses retention (§5.3) and operator-driven deletion.
Encryption-at-rest, right-to-delete tooling, and access control
are deferred to a future ADR: `secret-handling-in-persisted-state`.

For v1 operators: don't store factor-q state on shared
filesystems; use disk encryption if the host warrants it.

### 5.5 Cross-store consistency — DECIDED

**Each worker's SQLite is the source of truth for its in-flight
invocations. The control-plane's SQLite is the source of truth
for projections, coordination, schedules, and the completed
archive. NATS is the source of truth for the audit log.**

Today's invariant — "SQLite (projection) is rebuildable from
NATS" — held because SQLite carried only projections. After this
proposal, SQLite (in two forms) carries new source-of-truth
data. The invariant changes:

> | Store | Tables | Source of truth? | Rebuildable from NATS? |
> |---|---|---|---|
> | Control-plane SQLite | `events_*` | No | **Yes** |
> | Control-plane SQLite | `coordination_*`, `schedule_*`, `pending_wait`, `invocation_archive` | **Yes** | No |
> | Per-worker SQLite | `invocation_state`, `tool_dispatch`, `llm_dispatch` | **Yes** | No |

Write order at every dispatch boundary on a worker:

1. **Persist `intent` to worker SQLite** (sync). Durable commit
   point. Nothing has happened externally yet.
2. **Publish `tool.call` to NATS** (sync ack). The audit event.
3. **Execute the tool.**
4. **Persist `dispatched` to worker SQLite** (sync).
5. **Persist `completed` to worker SQLite** (sync) with the
   result.
6. **Publish `tool.result` to NATS** (sync ack).

Crash semantics:

- (1)→(2): SQLite ahead of NATS by one intent row. Recovery
  republishes; NATS is idempotent on `event_id`.
- (2)→(3): safe. Intent recorded, nothing happened externally.
  Re-dispatch is correct.
- (3)→(4): ambiguous. Intent recorded, dispatched not, but tool
  may have started. **Operator surface.**
- (4)→(5): ambiguous. Dispatched recorded, completed not.
  **Operator surface.**
- (5)→(6): safe-direction. Result is durable; replay the audit.

Each worker's WAL is consulted independently on its own restart;
the control-plane's view of "what's stuck globally" comes from
each worker reporting its ambiguous cases via NATS on recovery.

**Archive hand-off** (terminal invocation → control-plane,
covered in §9.3) uses the same SQLite-first write order. On
reaching terminal:

1. **Persist `terminal_at` to worker SQLite** (sync). Already
   stamped by the reducer runner's per-step terminal upsert; the
   `emit_failed` path calls a small `ensure_terminal` helper to
   cover mid-step failure sites that bypass the loop's upsert.
2. **Publish `completed` or `failed` to NATS** (sync ack). The
   lifecycle event the rest of the system reads.
3. **Publish `invocation.archived` to NATS** (sync ack). Carries
   the final state blob for the control-plane to archive.
4. **Mark the worker SQLite row `archive_status = 'pending'`**
   (sync). Stamps `archive_published_at` so the retry sweeper
   can measure age.

Crash semantics:

- After (1), before (2)/(3): recovery on restart sees a terminal
  row with `archive_status IS NULL` and the retry sweeper picks
  it up via `list_archive_pending`'s NULL-first ordering, then
  publishes `invocation.archived`.
- Between (3) and (4): the event went out but the row is still
  NULL. The next sweeper tick treats it the same way and
  republishes; the control-plane's `insert_archive` is
  idempotent on `invocation_id` so the second publish is a no-op
  on the archive table, and the ack still flows.
- After (4): the row is `'pending'`; the sweeper republishes on
  its cadence until `invocation.archive_acked` arrives on the
  worker-scoped subject and the local row is deleted.

The retry sweeper's "correctness over cleanup" rule (never
delete a held row automatically — see `archive_retry.rs`) means
a control-plane outage holds the row indefinitely on the worker
rather than risking silent data loss.

### 5.6 Schema evolution — DECIDED, MINIMAL

**Refuse-and-flag across incompatible schema changes.** Each
state blob carries `schema_version: u32`. On resume, mismatched
versions fail loudly. Operator decides whether to drop and
restart the invocation or roll the runtime back.

Why this answer:

- Single-operator, self-hosted, no timeline pressure.
- Building a migration framework before the schema has settled
  is over-engineering.
- "In-flight invocations don't survive incompatible upgrades"
  is an acceptable contract for this stage of the project,
  *if it's named*. Silently failing on resume is not.

In a control-plane / worker split, schema versions advance
independently per role: a control-plane can be upgraded ahead of
its workers (or vice versa) only across compatible versions.
Incompatibility surfaces at startup with a clear message
identifying which role and which schema is mismatched.

Concrete behaviour on a worker:

```
$ fq run --role=worker
ERROR: 2 invocations have incompatible state schema (v0.3 -> v0.4):
  inv_abc123: tool_dispatch table format changed
  inv_def456: same

Use `fq invocation drop --schema-mismatch` to abandon them, or
roll back to fq v0.3.x to resume.
```

### 5.7 Multi-process / multi-node ownership — DECIDED

**Invocation ownership is recorded explicitly in the
control-plane. One worker owns an invocation for its lifetime;
ownership is immutable; ownership transfer on worker failure is
not supported in v1/v2.**

This is the load-bearing decision the requirements doc deferred.
The shape:

- Worker registration: each `fq run --role=worker` registers
  with the control-plane on startup (NATS subject; heartbeat).
- Invocation claim: when a trigger fires, the control-plane
  picks a healthy worker, records the assignment in
  `coordination_invocation_owner` (control-plane SQLite), and
  delivers the invocation to the worker via NATS.
- Lifetime: the worker owns the invocation until terminal. The
  control-plane does not reassign.
- Worker failure: in-flight invocations on a failed worker stay
  marked as owned. On worker restart, the worker reclaims its
  in-flight rows from its local SQLite and resumes per the WAL
  contract. If the worker host is permanently lost, the
  operator manually marks affected invocations failed.

A future v3 with multi-host workspace storage and
ownership-transfer-on-failure is possible but not designed
here. The v2 contract — invocations don't migrate — is
operator-comprehensible and matches the workspace-locality
constraint.

### 5.8 What was considered and dismissed

| Alternative | Why dismissed |
|---|---|
| Project-pinned workers | Rate limits accrue to API keys, not nodes; cost accounting is invocation-tree-based; failure blast radius is identical with or without pinning. The only argument that survived (workspace locality) is invocation-scoped, not project-scoped. |
| Symmetric / peer-to-peer worker model (no control-plane) | Operator surface gets fuzzy: where do queries live? Where does coordination state live? Splitting the role is cheap and clarifies. |
| NATS JetStream KV for state | Memory: NATS is for events, not general persistence. Adding KV blurs the role. |
| A separate KV store (sled, redb, fjall) | Adds an infra item without solving a problem SQLite can't. Hurts `just up` simplicity. |
| Postgres | Violates local-dev simplicity. No requirement justifies it at this scale. With per-worker SQLite, write throughput on any single file is bounded by that worker's invocations, well within SQLite's WAL-mode capacity. |
| Filesystem JSON files for state | Workable for v1 but doesn't compose with SQL queries ("what's stuck?", "what's ambiguous?"). No transactions across multiple files. |
| Direct worker→control-plane RPC for completed-invocation handoff | Adds a network protocol where NATS already does the job. Latency of NATS dwarfed by invocation duration anyway. |

## 6. Shape-to-store mapping

For each shape from
[requirements §1](./data-architecture-requirements.md#1-data-shapes),
the role and store.

| Shape | Role | Store | Reason |
|---|---|---|---|
| Append-only audit log | shared | NATS JetStream | Already load-bearing per ADR-0011. |
| Work queue (triggers) | shared | NATS JetStream | Same. |
| Queryable projection | control-plane | Control-plane SQLite | Operator-queryable. Rebuildable from NATS. |
| Coordination: worker membership, invocation ownership | control-plane | Control-plane SQLite | Single source of truth for "what's running where." |
| Schedules / wakeups | control-plane | Control-plane SQLite | Time-driven; control-plane fires them. |
| Pending approvals / waits | control-plane | Control-plane SQLite | Control-plane wakes them on signal arrival. |
| Completed-invocation archive | control-plane | Control-plane SQLite | Migrated from worker on terminal status. |
| **Latest state per in-flight invocation** | worker | Per-worker SQLite | Local, fast, single-writer. Belongs with the WAL it underpins. |
| **Tool dispatch WAL** | worker | Per-worker SQLite | Three-state record per dispatch. Co-located with state for transactional consistency. |
| **LLM dispatch WAL** | worker | Per-worker SQLite | Same shape as tool WAL. |
| **Per-invocation workspaces** | worker | Worker filesystem | Critical state. Local to the worker that runs the invocation. Lifecycle bound to invocation. |
| Static configuration | operator | Filesystem (`fq.toml`) | Operator-managed. Unchanged. |
| Agent definitions | operator | Filesystem (`agents/`) | Operator-managed. Unchanged. |
| Pricing cache | both roles | Filesystem (`<cache_dir>/pricing.json`) | Rebuildable from network. Both roles may cache. |
| Cost/budget windows | derived | (control-plane projection) | Rebuildable from audit log via the projection. |

**Three source-of-truth physical stores per worker (local
SQLite + workspace dir + their handles into NATS). Two on the
control-plane (control-plane SQLite + its handles into NATS).**

## 7. Recovery semantics

Recovery is per-role.

### 7.1 Worker startup sequence

1. Open the worker's local SQLite. Acquire exclusive lock. On
   lock failure: refuse to start with a clear message about
   another worker process holding the file.
2. Verify schema version. On mismatch: refuse-and-flag per §5.6.
3. Connect to NATS. On unreachable: retry with backoff.
4. Register with the control-plane (NATS subject, heartbeat).
5. Query local `invocation_state` for rows with non-terminal
   `phase`.
6. For each non-terminal invocation, query local `tool_dispatch`
   and `llm_dispatch` for rows where the latest entry is
   `intent` or `dispatched` without a matching `completed`.
7. **Categorise:**
   - **Safe-resume**: latest dispatch row is `intent` only.
     Re-dispatch automatically.
   - **Safe-replay**: latest dispatch row is `completed`.
     Continue with the persisted result as `last_result`.
   - **Ambiguous**: latest dispatch row is `dispatched` without
     `completed`. Mark locally as held; publish
     `invocation.ambiguous` to NATS; do not auto-recover.
8. Resume safe-resume and safe-replay categories automatically.
   Held ambiguous invocations don't block new triggers.

### 7.2 Control-plane startup sequence

1. Open the control-plane SQLite. Acquire exclusive lock.
2. Verify schema version.
3. Connect to NATS.
4. Subscribe to:
   - The audit stream for projection updates.
   - Worker heartbeats and `invocation.ambiguous` events.
   - Trigger queue.
5. Reconcile coordination state with live worker membership:
   - Workers seen heartbeating: marked alive.
   - Workers in `coordination_invocation_owner` but not
     heartbeating after a grace period: marked stale.
   - Invocations owned by stale workers: surface to operator
     via `fq workers stale`.
6. Begin trigger dispatch.

### 7.3 NATS consistency reconciliation

On worker startup, after the local categorisation, the worker
republishes any audit events that may have been lost between
SQLite commit and NATS publish (intent rows without a matching
NATS event, completed rows without a matching `tool.result`).
Idempotent on `event_id`.

This handles the (1)→(2) and (5)→(6) crash windows in §5.5.
Both are safe-direction; reconciliation makes them invisible to
downstream consumers.

### 7.4 Operator workflow

Single-node (v1), `fq run --role=both`:

```
$ fq run
[fq] role: both (control-plane + worker)
[fq] opening control-plane store          ✓
[fq] opening worker store                  ✓
[fq] verifying schemas (cp v1, w v1)       ✓
[fq] connecting to NATS                    ✓
[fq] checking for in-flight invocations
       3 found
       1 safe-resume → resuming inv_abc123
       1 safe-replay → resuming inv_def456
       1 ambiguous   → holding (run `fq recover` to triage)
[fq] daemon ready
```

Multi-node (v2):

```
$ fq run --role=control-plane
[fq] role: control-plane
[fq] opening control-plane store          ✓
[fq] connecting to NATS                    ✓
[fq] waiting for workers
       w-001 registered (host: prod-1)
       w-002 registered (host: prod-2)
[fq] reconciling coordination state
       2 stale-worker invocations (use `fq workers stale`)
       1 ambiguous (use `fq recover`)
[fq] dispatcher ready
```

```
$ fq run --role=worker --worker-id=w-001
[fq] role: worker, id: w-001
[fq] opening worker store                  ✓
[fq] connecting to NATS                    ✓
[fq] registering with control-plane        ✓
[fq] checking for in-flight invocations
       2 found
       1 safe-resume → resuming inv_abc123
       1 safe-replay → resuming inv_def456
[fq] worker ready
```

`fq recover` and `fq workers stale` are control-plane commands.
They aggregate across the cluster.

## 8. Backup unit

Per role.

### 8.1 Critical artefacts

| Artefact | Role | Critical? | Backup story |
|---|---|---|---|
| `<nats_data_dir>/` | shared | **Yes** | Standard JetStream backup; snapshot the data dir or `nats stream backup`. |
| `<cache_dir>/control-plane.db` | control-plane | **Yes** | SQLite file holding projection, coordination, schedules, archive. `sqlite3 control-plane.db ".backup target.db"`. |
| `<cache_dir>/worker-<id>.db` | per worker | **Yes** | One file per worker. Contains in-flight state and WAL. Backed up while the worker is running via SQLite online backup, or while stopped via copy. |
| `<workspaces_dir>/` (per worker) | per worker | **Yes** | Critical: workspaces are core agent state. Backed up *together* with the worker SQLite (same point-in-time) so resume sees consistent state. |
| `fq.toml`, `agents/` | operator | n/a | Operator-managed (typically git). |

### 8.2 What this means in practice

Single-node (v1):

> Back up `<nats_data_dir>/`, `<cache_dir>/store.db` (single
> file, both roles), and `<workspaces_dir>/`. Three artefacts.

Multi-node (v2):

> Control-plane backup: `<nats_data_dir>/` plus
> `<cache_dir>/control-plane.db`. Two artefacts.
>
> Per-worker backup: that worker's `<cache_dir>/worker-<id>.db`
> plus its `<workspaces_dir>/`. Two artefacts per worker. Each
> worker is independent; worker backups don't have to be
> coordinated with each other or with the control-plane.

### 8.3 Restore semantics

Worker SQLite + workspace dir restored to the same point-in-time
is the **strict requirement** for an in-flight invocation to
resume cleanly. Mismatched timestamps mean the WAL might
reference workspace bytes that don't exist (or vice versa),
which surfaces as ambiguous on resume.

Control-plane restore is independent of worker restores. The
control-plane projection is rebuildable from NATS; coordination
state is reconstructed from worker registration on startup;
schedules and pending waits must be backed up because they are
source-of-truth.

NATS restore: the audit log is intact whatever else fails. A
fresh control-plane against a restored NATS rebuilds its
projection automatically. A fresh worker against a restored
NATS has no in-flight state — it starts empty, picks up new
triggers, and the audit log shows the gap.

### 8.4 Cross-store divergence

If NATS, control-plane SQLite, and workers are all restored to
*different* points in time, the runtime detects divergence:

- State references invocations whose audit events don't exist:
  ambiguous; surface to operator.
- Audit events reference invocations not in any worker's state:
  treat as long-completed (matches the safe-replay category).
- Coordination claims an invocation owned by a worker, but the
  worker has no record of it: surface as orphaned.

The detection is straightforward; the resolution is operator
judgement. Cross-store-divergence is named in operator runbook
(future) as the failure mode that requires human decision.

## 9. Schema additions

### 9.1 Worker SQLite schema

```sql
-- In-flight invocation state. One row per active invocation
-- on this worker.
CREATE TABLE invocation_state (
    invocation_id   TEXT PRIMARY KEY,
    agent_id        TEXT NOT NULL,
    schema_version  INTEGER NOT NULL,
    phase           TEXT NOT NULL,              -- reducer phase
    state_blob      BLOB NOT NULL,              -- opaque to runtime
    iteration       INTEGER NOT NULL DEFAULT 0,
    started_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    terminal_at     INTEGER,                    -- set on terminal
    INDEX(agent_id),
    INDEX(terminal_at)
);

-- Tool dispatch WAL.
CREATE TABLE tool_dispatch (
    invocation_id   TEXT NOT NULL,
    tool_call_id    TEXT NOT NULL,
    tool_name       TEXT NOT NULL,
    status          TEXT NOT NULL,              -- 'intent' | 'dispatched' | 'completed'
    parameters      TEXT NOT NULL,              -- JSON
    result          TEXT,                       -- JSON; null until completed
    is_error        INTEGER,                    -- null until completed
    intent_at       INTEGER NOT NULL,
    dispatched_at   INTEGER,
    completed_at    INTEGER,
    PRIMARY KEY (invocation_id, tool_call_id),
    INDEX(status, dispatched_at)                -- for ambiguous-case query
);

-- LLM dispatch WAL.
CREATE TABLE llm_dispatch (
    invocation_id   TEXT NOT NULL,
    request_id      TEXT NOT NULL,
    model           TEXT NOT NULL,
    status          TEXT NOT NULL,
    request_payload TEXT NOT NULL,
    response        TEXT,
    cost_usd        REAL,
    intent_at       INTEGER NOT NULL,
    dispatched_at   INTEGER,
    completed_at    INTEGER,
    PRIMARY KEY (invocation_id, request_id),
    INDEX(status, dispatched_at)
);
```

### 9.2 Control-plane SQLite schema

Existing projection tables (`events_*`) are unchanged.

```sql
-- Live worker membership.
CREATE TABLE coordination_worker (
    worker_id        TEXT PRIMARY KEY,
    host             TEXT NOT NULL,
    registered_at    INTEGER NOT NULL,
    last_heartbeat   INTEGER NOT NULL,
    status           TEXT NOT NULL              -- 'alive' | 'stale' | 'shutdown'
);

-- Invocation ownership: which worker owns which invocation.
-- Source of truth for "where is X running."
CREATE TABLE coordination_invocation_owner (
    invocation_id    TEXT PRIMARY KEY,
    worker_id        TEXT NOT NULL,
    assigned_at      INTEGER NOT NULL,
    status           TEXT NOT NULL,             -- 'in_flight' | 'completed' | 'ambiguous'
    INDEX(worker_id, status)
);

-- Pending external waits: approvals, time waits, webhook receivers.
CREATE TABLE pending_wait (
    invocation_id    TEXT PRIMARY KEY,
    kind             TEXT NOT NULL,             -- 'approval' | 'time' | 'webhook' | ...
    descriptor       TEXT NOT NULL,             -- JSON; kind-specific
    expires_at       INTEGER,                   -- null = no expiry
    created_at       INTEGER NOT NULL
);

-- Future-firing entries: scheduled triggers, pricing refreshes.
CREATE TABLE schedule_entry (
    id               TEXT PRIMARY KEY,
    kind             TEXT NOT NULL,
    fire_at          INTEGER NOT NULL,
    payload          TEXT NOT NULL,
    INDEX(fire_at)
);

-- Completed-invocation archive: migrated from workers on
-- `invocation.archived` event consumption.
CREATE TABLE invocation_archive (
    invocation_id    TEXT PRIMARY KEY,
    agent_id         TEXT NOT NULL,
    final_phase      TEXT NOT NULL,             -- 'completed' | 'failed'
    final_state_blob BLOB NOT NULL,
    started_at       INTEGER NOT NULL,
    terminal_at      INTEGER NOT NULL,
    archived_at      INTEGER NOT NULL,
    INDEX(agent_id, terminal_at),
    INDEX(archived_at)                          -- for retention sweep
);
```

Schemas are sketches. Final shapes during implementation may
add columns; the *contract* (PK choice, indexes for recovery
and retention sweep) is committed.

### 9.3 New event types

`tool.dispatched` joins the canonical event sequence between
`tool.call` and `tool.result`:

```
triggered → llm.request → llm.dispatched → llm.response (cost on envelope)
          → tool.call  → tool.dispatched → tool.result
          → ... → completed
          → invocation.archived  (after worker hand-off)
```

Today's schema uses `tool.call` (≈ intent) and `tool.result` (≈
completed). Adding `tool.dispatched` is non-breaking; existing
consumers ignore unknown events. Same for `llm.dispatched`
between `llm.request` and `llm.response`.

After the envelope refactor (see
`docs/plans/active/2026-05-15-event-envelope-refactor.md`),
cost rides on the `llm.response` envelope rather than as a
separate event; the canonical sequence shortens by one event
per LLM turn.

`invocation.archived` is a new event published by a worker once
it has determined an invocation is terminal and is ready to hand
off the final state. The control-plane archive consumer reads
this event and writes the archive row, then publishes
`invocation.archive_acked` so the worker can delete its local
state.

### 9.4 New CLI commands

| Command | Role | Purpose |
|---|---|---|
| `fq recover` | control-plane | Triage ambiguous invocations interactively, across the cluster. |
| `fq invocation list` | control-plane | Enumerate non-terminal invocations cluster-wide. |
| `fq invocation drop <id>` | control-plane | Force-terminate; retains audit trail. |
| `fq invocation show <id>` | control-plane | Show full state for one invocation. |
| `fq workers list` | control-plane | List registered workers and status. |
| `fq workers stale` | control-plane | List workers whose heartbeat has lapsed. |

`fq run` startup gains the in-flight summary block from §7.4,
keyed off the role.

## 10. Implementation order

The first slice — a build sequence that exercises the contract
end-to-end on single-node before touching the multi-node split.

1. **Internal role split inside `fq-runtime`.** Introduce
   `control_plane` and `worker` modules. v1 ships as one
   process; the boundary is a Rust trait, not a network
   boundary. Same SQLite file, schema split between the two
   table classes.
2. **Worker schema migration.** Create `invocation_state`,
   `tool_dispatch`, `llm_dispatch`. Update `ProjectionStore` (or
   a new `WorkerStore`) to manage them.
3. **Control-plane schema migration.** Create
   `coordination_*`, `schedule_entry`, `pending_wait`,
   `invocation_archive`. Existing projection tables unchanged.
4. **Three-state WAL writes in `ReducerRunner`.** Persist
   `intent` / `dispatched` / `completed` around tool dispatch.
   Same for LLM calls. New events emitted alongside.
5. **Persist reducer state on every step boundary.** `state_blob`
   updated synchronously alongside WAL transitions.
6. **Worker recovery path.** Categorise in-flight invocations on
   restart; auto-resume safe categories.
7. **Control-plane recovery path.** Reconcile coordination state
   on restart; subscribe to `invocation.ambiguous`,
   `invocation.archived`.
8. **Worker → control-plane archive hand-off.** Worker emits
   `invocation.archived`; control-plane consumes and writes
   archive row; control-plane publishes
   `invocation.archive_acked`; worker deletes its local row.
9. **`fq recover` and `fq workers` commands.** Interactive
   triage on the control-plane.
10. **Retention sweep.** Background task on the control-plane
    that deletes archive rows past the configured age.

Each step is independently demonstrable. Steps 1–6 deliver the
correctness contract for single-node. Steps 7–9 deliver the
operator surface. Step 10 makes it bounded.

## 11. v1 to v2 evolution path

What changes when an operator splits the deployment.

### 11.1 What stays the same

- All contracts (§4).
- All schemas (§9). Both SQLite stores' schemas already exist
  in v1 — they just happen to live in the same file.
- The reducer harness, the event bus shape, the audit log
  semantics.
- All event types.

### 11.2 What changes

- **Process boundary.** v1's `fq run` becomes
  `fq run --role=control-plane` or `fq run --role=worker`
  (or `--role=both` for compatibility).
- **SQLite split.** v1 has one file with both schema classes.
  v2 splits into `control-plane.db` and `worker-<id>.db`. A
  migration tool ports v1 → v2 by copying the relevant tables.
- **NATS coordination becomes load-bearing.** v1 has trivial
  coordination (one node claims itself). v2 has real worker
  registration, invocation ownership, ambiguous-case
  propagation.
- **Workspace dir per worker.** v1 has one. v2 has N, each
  worker's own.

### 11.3 What v1 must commit to to not block v2

- Invocation IDs are globally unique (already true).
- Workers and control-plane communicate strictly through NATS
  in v1 too (no in-process shortcuts that wouldn't work across
  processes).
- The internal role boundary is enforced at build time (Rust
  module visibility), so v1 cannot accidentally couple the two
  roles in a way v2 has to undo.
- The trigger dispatch path goes through the control-plane
  module even on a single-node deployment.
- Configuration permits a per-role `state_db` path. v1 default
  collapses both to one file; v2 separates them.

## 12. What this doc leaves open

| Open | Why deferred |
|---|---|
| Per-agent durability mode opt-in | v1 is sync-only; opt-in async is a v3 addition. |
| Encryption-at-rest for state | Operator-handled via disk encryption in v1; named for future ADR. |
| Right-to-delete tooling beyond `fq invocation drop` | No regulatory requirement at this stage; named for future. |
| Schema migration framework | Refuse-and-flag in v1; build a migration path once the schema stabilises. |
| Ownership-transfer-on-failure (invocation migration between workers) | Workspace-locality forbids it without shared workspace storage. v3 concern. |
| Shared workspace storage (NFS / object-store-backed) | v1/v2 use local workspaces. v3 if working set caps single-node disk. |
| HA control-plane | Single control-plane in v2; HA pair is a follow-up. |
| Workload-class-based scheduling preferences | Scheduler picks any healthy worker in v2; agent-declared preferences (CPU-heavy, GPU, etc.) are a follow-up. |
| Idempotency-key opt-in for cooperating tools | Additive, post-v1. The WAL contract works without it. |
| Approval-gate UI / signal mechanics | The shape (`pending_wait` table) is committed; the user-facing approval flow is a separate design. |

## 13. What this is not

- **Not a complete recovery story for all crash modes.** The
  contract handles process crash and worker host loss. Disk
  corruption, partial NATS data loss, simultaneous-store
  divergence on restore are operator-recovery concerns named
  in §8.4 but not solved here.
- **Not a horizontal-scale-of-state design.** State stays
  per-worker; multi-node scales by adding workers, not by
  distributing any single invocation's state.
- **Not a permission or access-control story.** Single-tenant,
  single-operator assumption; multi-operator deployments need
  a separate design.
- **Not a replacement for the requirements doc.** Read both;
  this one commits, that one surveys.

## Cross-references

- Requirements: [`data-architecture-requirements.md`](./data-architecture-requirements.md)
- Tool idempotency constraint: requirements §4.1
- Event bus accepted decision: [`ADR-0011`](../../adrs/accepted/0011-event-bus-and-persistence.md)
- Memory-as-MCP-services scope boundary: [`ADR-0013`](../../adrs/accepted/0013-memory-as-mcp-service.md)
- Reducer harness (load-bearing consumer of state persistence): [`reducer-harness.md`](../../guide/reducer-harness.md)
- Boundary design (why the reducer is a clean fit for this WAL): [`wasm-boundary-design.md`](./wasm-boundary-design.md)
- Vision and Q200 north star: [`VISION.md`](../../../VISION.md)
