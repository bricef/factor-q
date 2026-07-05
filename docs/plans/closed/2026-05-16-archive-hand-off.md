# Plan: Archive Hand-off (data-architecture-v1 step 8)

**Date**: 2026-05-16
**Status**: Closed 2026-05-18. Implementation landed 2026-05-17
on `worktree-inherited-dancing-reef` (`22087a9..5c4f90d`) and
fast-forwarded onto `main` as `421ab11..e241860`. The doc-only
follow-ups landed 2026-05-18 (event-schema and
data-architecture updates, `[worker]` keys in the fq.toml
template). See the parent plan's
[Step 8](./2026-04-28-data-architecture-v1.md#step-8--worker--control-plane-archive-hand-off)
status block for the canonical record.
**Parent plan**:
[`2026-04-28-data-architecture-v1.md`](./2026-04-28-data-architecture-v1.md) — step 8.
**Design references**:
- [`docs/design/committed/data-architecture.md`](../../design/committed/data-architecture.md) §5.3 (state retention), §5.5 (archive hand-off write order), and §9.3 (new event types).
- Schema: §10 `invocation_archive` table is already implemented in
  `services/fq-runtime/crates/fq-runtime/src/control_plane/store.rs`.

## Status (2026-05-17)

The work landed on `worktree-inherited-dancing-reef` as the
eight commits `22087a9..5c4f90d`. The design that shipped
diverges from this plan in three load-bearing places — the
plan was reconciled to match what actually shipped, so the
sections below describe reality, with each step's
"What shipped" subsection naming the commit.

Headline divergences from the original 2026-05-16 design:

- **Worker pending-state lives on `invocation_state`**, not on
  a new `pending_archive` table. Two nullable columns
  (`archive_status`, `archive_published_at`) and an index
  cover the retry sweeper's scan; migration `v4` is additive.
- **Ack subject is worker-scoped**
  (`fq.worker.<worker_id>.invocation.archive_acked`) so the
  worker subscribes with a single filter, mirroring the
  heartbeat. The coordination consumer does not double-consume.
- **`InvocationArchivedPayload` carries `worker_id`** so the
  control-plane knows how to address the ack back.
  `InvocationArchiveAckedPayload` carries `worker_id` too, as
  defense-in-depth on the worker side.

Other shipped-but-different details: ack consumer is core
NATS (not durable JetStream); cleanup on ack is just the
`invocation_state` row (no WAL purge, no separate
`pending_archive` row); the timeout knob is a *warn-after*
threshold (the sweeper keeps republishing past it — never
deletes); config lives under `[worker]`, not `[state]`.

Deferred:

- Live acceptance test against NATS + Anthropic — cannot run
  from the dev sandbox.
- `docs/design/committed/event-schema.md` and
  `docs/design/committed/data-architecture.md` updates for the two new
  event types and the §5.5 write-order detail.

## Goal

Move terminal invocations off the worker and into the
control-plane archive via NATS:

1. Worker emits `invocation.archived` on terminal.
2. Control-plane consumer writes the `invocation_archive` row,
   flips ownership to `Completed`, and emits
   `invocation.archive_acked`.
3. Worker consumes the ack, deletes its local
   `invocation_state` row.
4. If no ack arrives, a worker-side retry sweeper republishes
   on a fixed cadence and **holds the row past the warn
   threshold** — correctness over cleanup.

After this step lands, the worker no longer accumulates rows
for completed invocations indefinitely, and step 10's
retention sweep has a single canonical home for terminal
state.

## Context

Step 7 (control-plane recovery) shipped on 2026-05-16,
including the heartbeat producer/consumer and the periodic
stale-worker sweep. The coordination consumer already filters
`fq.agent.*.invocation.*` and has an explicit `_ => Ok(())`
arm awaiting the `invocation.archived` variant (see
`coordination_consumer.rs:177`). The architecture decision is
settled; this plan executes it.

**Scaffolding already in place** (no work to redo):

- `invocation_archive` table + indexes on `ControlPlaneStore`.
- `ControlPlaneStore::insert_archive` is idempotent via
  `ON CONFLICT(invocation_id) DO NOTHING`.
- `ControlPlaneStore::{get_archive, sweep_archive, list_archive_for_agent}`
  exist for the consumer and step 10's sweep.
- `WorkerStore::delete_invocation_state` is ready for
  post-ack cleanup — reused as-is.
- The reducer runner already stamps `terminal_at` on the
  `invocation_state` row when the reducer reaches Complete
  or Failed (`runner.rs:397` via
  `phase_and_terminal_from`). The `emit_failed` path needed a
  new `ensure_terminal` helper to close a pre-existing gap
  (LLM-error and budget-exceeded mid-step sites flag failure
  before the run-loop's per-step upsert can fire).

## Decisions taken on 2026-05-16

These were agreed upfront and are not relitigated unless code
exposes a flaw. Each is annotated with what actually shipped.

- **Pending-ack tracking**: ~~a new `pending_archive` table on
  the worker store~~ → **shipped as two columns on
  `invocation_state`** (`archive_status`,
  `archive_published_at`) plus an index. Same durability
  guarantee, fewer moving parts (no second table to migrate
  or join), and the existing run-loop already owns the
  `invocation_state` row's lifecycle. `upsert_invocation_state`
  deliberately leaves the archive columns untouched so
  per-step writes don't clobber `"pending"` back to `NULL`.
- **`InvocationArchivedPayload` fields**: `final_phase`
  (`"completed"` | `"failed"`), `final_state_blob: Vec<u8>`,
  `started_at_ms: i64`, `terminal_at_ms: i64`, **plus
  `worker_id: WorkerId`** so the control-plane can address
  the ack back. The plan's "1:1 onto `InvocationArchiveRow`"
  holds for the four schema-row fields; `worker_id` is
  carried for routing.
- **`InvocationArchiveAckedPayload` fields**: ~~empty (unit
  struct)~~ → **`{ worker_id: WorkerId }`**. Defense-in-depth:
  even though the subject token routes by `worker_id`, the
  receiving worker still refuses to delete someone else's row
  if a producer bug ever publishes a misaddressed ack.
- **Subjects**:
  `fq.agent.<id>.invocation.archived` (worker → CP — falls
  under the coordination consumer's existing filter) and
  `fq.worker.<worker_id>.invocation.archive_acked`
  (CP → worker — **worker-scoped**, not agent-scoped as the
  original plan said). The worker subscribes with a single
  filter on its own id, mirroring the heartbeat. The
  coordination consumer does not consume the ack.
- **Schema ids**: `factor-q/invocation_archived@1` and
  `factor-q/invocation_archive_acked@1`.
- **Republish cadence**: every 10s while pending.
  **Warn threshold**: default 60s. After the threshold the
  sweeper *keeps republishing* and logs a single warning per
  row (de-duplicated by `invocation_id` in a `HashSet` on the
  sweeper). The local row is never deleted automatically —
  operator action required.
- **fq.toml**: new `[worker]` section (not `[state]`) with
  `archive_retry_interval_ms` (default 10_000) and
  `archive_warn_after_ms` (default 60_000). Both knobs are
  exposed; the heartbeat cadence is still a const because
  changing it independently of the control-plane's stale
  threshold would change semantics.
- **Control-plane emits `archive_acked` unconditionally** on
  every successful `insert_archive` call, including the
  idempotent-conflict no-op case. Otherwise a redelivered
  `archived` event would never re-trigger the ack and a
  worker that missed the first ack would never clean up.
- **`Completed` / `Failed` events are unchanged**.
  `invocation.archived` is *after* the terminal event, per
  the §9.3 canonical sequence diagram.

## Approach: TDD per step

Same shape as the parent plan. Per step:

1. Acceptance test (red).
2. Integration tests (red).
3. Unit tests (red).
4. Implement until all three tiers pass.
5. Refactor with all tests green.

### Test tiers

Identical to the parent plan's table. Acceptance tests gated
on `FQ_NATS_URL`; no LLM key is needed for any acceptance
test in this plan (scripted reducers suffice). The single
end-to-end acceptance test on the parent plan additionally
needs `ANTHROPIC_API_KEY` and is the only deferred item.

## Implementation Steps

### Step 1 — Event types, payloads, subjects

**Goal.** Add the two new event variants, their payloads,
schema ids, subject helpers, and serde tests. No behavioural
change.

**What shipped (`22087a9`).** `EventPayload::InvocationArchived`
and `::InvocationArchiveAcked` variants, payloads as in
"Decisions" above, subject helpers
`agent_invocation_archived(agent_id)` and
`worker_invocation_archive_acked(worker_id)` in
`events::subjects`, `schema_id_for` arms for both variants,
and `Event::subject` routing the ack to the worker-scoped
form via the payload's `worker_id`.

#### Integration tests

- **`invocation_archived_subject_is_agent_scoped`** —
  construct an event with `InvocationArchived`, verify
  subject is `fq.agent.<id>.invocation.archived` and
  `schema_id` is `factor-q/invocation_archived@1`.
- **`invocation_archive_acked_subject_is_worker_scoped`** —
  construct an event with `InvocationArchiveAcked`, verify
  subject is `fq.worker.<worker_id>.invocation.archive_acked`
  and `schema_id` is `factor-q/invocation_archive_acked@1`.

#### Done when

- [x] `EventPayload::InvocationArchived` and
      `EventPayload::InvocationArchiveAcked` variants exist.
- [x] Subject helpers in `events::subjects` cover both.
- [x] Schema-id mapping in `events.rs` covers both.
- [x] `Event::subject` routes the ack via payload `worker_id`.
- [x] All listed tests green.
- [x] No regression: full `cargo test -p fq-runtime` green.

---

### Step 2 — Worker-store archive columns

**Goal.** Track pending hand-off durably on the worker so the
retry sweeper can republish across worker restarts. Migration
runs automatically on `WorkerStore::open` against an existing
worker DB.

**What shipped (`a1c0758`).** Worker-store migration `v4`
adds two nullable columns on `invocation_state` plus a
covering index. Two new methods (`set_archive_pending`,
`list_archive_pending`); `upsert_invocation_state`
deliberately leaves the archive columns out of its column
list. `delete_invocation_state` is reused on ack receipt.

#### Schema

```sql
-- Worker migration v4 (additive)
ALTER TABLE invocation_state ADD COLUMN archive_status TEXT;
ALTER TABLE invocation_state ADD COLUMN archive_published_at INTEGER;
CREATE INDEX IF NOT EXISTS idx_invocation_state_archive
    ON invocation_state(archive_status, archive_published_at);
```

`archive_status` is `NULL` for in-flight rows and rows that
have reached terminal but not yet been published (a
transient sliver the sweeper picks up via NULL-first
ordering). `archive_status = 'pending'` means the worker has
published `invocation.archived` and is awaiting the ack. There
is no on-disk `acked` state — receipt of the ack deletes the
row outright.

#### Methods

- `set_archive_pending(invocation_id, published_at_ms) → u64` —
  idempotent `UPDATE` guarded by `terminal_at IS NOT NULL`.
  Re-calling on an already-pending row bumps
  `archive_published_at`. Returns rows affected.
- `list_archive_pending() → Vec<InvocationStateRow>` — selects
  terminal rows where `archive_status` is `NULL` or
  `'pending'`, ordered `archive_published_at IS NULL DESC,
  archive_published_at ASC`. NULL-first so a crashed-between-
  terminal-and-publish row is picked up ahead of legitimately
  pending rows.

#### Integration tests

- **`worker_store_open_creates_v4_columns_and_index`** — open
  against a fresh tempdir; verify both columns and the index
  exist on `invocation_state`.
- **`worker_store_v4_migrates_existing_db`** — open against a
  pre-v4 DB; verify migration adds the columns and preserves
  existing rows (which read both columns as `NULL`).
- **`set_archive_pending_marks_terminal_row_pending`** — seed
  a terminal row; call `set_archive_pending`; verify
  `archive_status = 'pending'` and `archive_published_at` is
  the provided value.
- **`set_archive_pending_no_op_on_non_terminal_row`** — seed
  a non-terminal row (no `terminal_at`); call the method;
  verify zero rows affected and columns still `NULL`.
- **`list_archive_pending_returns_null_first_then_oldest`** —
  seed three terminal rows (one with `archive_published_at =
  NULL`, two pending with different times); verify ordering.
- **`upsert_invocation_state_preserves_archive_columns`** —
  set pending; re-upsert a step; verify the archive columns
  retain their values.

#### Done when

- [x] `WorkerStore::{set_archive_pending, list_archive_pending}`
      exist.
- [x] v4 migration on a pre-v4 DB succeeds and preserves data.
- [x] `upsert_invocation_state` does not clobber archive
      columns.
- [x] All listed tests green.

---

### Step 3 — Worker emits `invocation.archived` on terminal

**Goal.** When the reducer runner reaches a terminal phase,
it publishes `invocation.archived` immediately after the
existing `Completed` / `Failed` event and flips the row's
`archive_status` to `'pending'`.

**What shipped (`d91c482` + `b0670eb`).** `d91c482` threaded
`WorkerId` into `ReducerRunner` and updated the `fq-cli`
event-log summary to include the new variants. `b0670eb`
added the emission helpers and wired them into the Complete
and `emit_failed` arms.

Order on the Complete path: terminal upsert → publish
`Completed` → publish `InvocationArchived` → call
`set_archive_pending(invocation_id, now_ms)`. The `now_ms`
intentionally differs from `terminal_at` — the retry sweeper
measures from the most recent publish.

The `emit_failed` path calls a new `ensure_terminal` helper
*before* the same publish helper. This closes a pre-existing
gap: the LLM-error and budget-exceeded mid-step sites flag
failure before the loop's per-step terminal upsert can fire,
so pre-step-8 those rows could be left non-terminal until
recovery on restart. The helper is idempotent and a no-op if
`terminal_at` is already set.

A crash between the terminal upsert and the publish is a safe
replay: `list_archive_pending`'s NULL-first ordering means
the retry sweeper picks the row up and emits on the next
tick.

#### Acceptance test

```text
TEST: complete_emits_invocation_archived_and_marks_row_pending

Setup:    Live NATS. `fq run` (single-node).
Action:   Trigger a scripted reducer that reaches Complete on
          its first step.
Assert:   Captured events include `invocation.archived` after
          `completed`. The payload's `final_state_blob`
          deserialises to a State::Complete{..}. The
          invocation_state row's archive_status is 'pending'.
```

Gated on `FQ_NATS_URL`.

#### Integration tests

- **`complete_emits_invocation_archived_and_marks_row_pending`** —
  the acceptance test above, NATS-gated.
- **`emit_failed_calls_ensure_terminal_before_publish`** —
  drive an LLM-error path that flags failure mid-step;
  verify `terminal_at` is set, the archived event is
  published, and the row is pending.

#### Unit tests

- **`build_archive_payload_from_terminal_state`** — pure:
  given a terminal `InvocationStateRow`, produce the matching
  `InvocationArchivedPayload`, stamping `worker_id`.

#### Done when

- [x] Emission integrated into Complete and `emit_failed`.
- [x] `ensure_terminal` helper covers the mid-step failure
      paths.
- [x] All listed tests green (NATS-gated tests skip when
      `FQ_NATS_URL` is unset).
- [x] Legacy executor path unaffected (it doesn't archive in
      v1; the parent plan's cross-cutting invariant holds).

---

### Step 4 — Control-plane consumer handles `invocation.archived`

**Goal.** Replace the `_ => Ok(())` arm in
`coordination_consumer.rs` with a real handler that:

1. Calls `ControlPlaneStore::insert_archive` (idempotent).
2. Flips coordination ownership to `Completed`.
3. Emits `invocation.archive_acked` on
   `fq.worker.<worker_id>.invocation.archive_acked`, **even
   when insert was a no-op** — see decision above.

If `insert_archive` fails (real store error), the message is
NAK'd and JetStream redelivers — existing pattern.

**What shipped (`dc4e512`).** The handler is
`handle_invocation_archived` (`pub(crate)` so the NATS-gated
integration test can drive it directly). The dispatch loop's
error type was unified to `CoordinationConsumerError` so the
ambiguous and archived arms can coexist; the ambiguous arm's
previous `ControlPlaneStoreError` now flows through via the
existing `#[from]` impl. If insert succeeds but ack publish
fails, the message NAKs and is redelivered — insert is
idempotent, the second ack will go out, and the worker's
retry sweeper would have republished anyway.

#### Acceptance test

```text
TEST: control_plane_writes_archive_and_acks

Setup:    Live NATS, fresh control-plane DB.
Action:   Publish an `invocation.archived` event directly.
Assert:   - `invocation_archive` row appears within 2 seconds.
          - `invocation.archive_acked` is published on the
            worker-scoped subject with the same invocation_id
            and the worker_id from the archived payload.
          - Republishing the same `invocation.archived` event
            results in a second ack (idempotent).
```

Gated on `FQ_NATS_URL`.

#### Integration tests

- **`handler_archives_invocation_and_publishes_ack`** —
  unit-style: pass the handler a `(store, bus, event)`;
  verify the archive row is written, coordination is flipped
  to Completed, and the ack is published on the worker-scoped
  subject. Also covers the idempotent-redelivery case.
- **`handle_invocation_archived_store_error_returns_err`** —
  inject a store failure; verify the handler returns `Err`
  (NAK path) without publishing an ack.

#### Unit tests

- **`archive_row_from_payload`** — pure: payload + envelope
  → `InvocationArchiveRow` mapping.

#### Done when

- [x] `coordination_consumer.rs` arm for
      `InvocationArchived` is implemented.
- [x] Coordination ownership flips to Completed on archive.
- [x] Ack is published on the worker-scoped subject derived
      from the payload's `worker_id`.
- [x] All listed tests green.
- [x] The earlier coordination-consumer end-to-end test
      (`coordination_consumer_handles_invocation_ambiguous_end_to_end`)
      still passes.

---

### Step 5 — Worker consumes `invocation.archive_acked`, cleans up

**Goal.** Add a worker-side consumer subscribed to
`fq.worker.<worker_id>.invocation.archive_acked`. On ack:
delete the local `invocation_state` row.

**What shipped (`a543edf`).** `ArchiveAckConsumer` is a
long-lived tokio task per worker. Subscription is **core
NATS** (`bus.subscribe`), not durable JetStream — acks missed
while the consumer is offline are recovered by the retry
sweeper (step 6) republishing `invocation.archived` until a
fresh ack arrives. A JetStream consumer per worker would not
change the correctness story.

On ack the handler does a defense-in-depth `worker_id` check
on the payload (subject routing is the primary protection,
this is belt-and-braces) and then calls
`WorkerStore::delete_invocation_state`. Cleanup scope is
deliberately narrow: just the `invocation_state` row. There
is no separate `pending_archive` row to remove (step 2 went
with columns), and `tool_dispatch` / `llm_dispatch` rows are
left to the existing per-invocation cleanup paths. Failure
policy is log-and-continue: a transient SQLite-busy delete
leaves the row terminal+pending and the next republish-and-ack
cycle retries the delete. Recovery treats already-terminal
rows as done.

Wired into the `fq run` lifecycle alongside `HeartbeatProducer`
(select-on-handle-or-shutdown, 5s graceful shutdown,
`system.task_failed` published on premature exit).

#### Acceptance test

```text
TEST: ack_deletes_matching_invocation_state_row

Setup:    Live NATS, `fq run` with both roles.
Action:   Trigger a scripted reducer to Complete.
Assert:   Within 5 seconds of completion:
          - invocation_state row removed.
          - Archive row exists on the control-plane.
```

Gated on `FQ_NATS_URL`.

#### Integration tests

- **`ack_deletes_matching_invocation_state_row`** — the
  acceptance test above, NATS-gated.
- **`ack_handler_idempotent_on_redelivery`** — call twice;
  second call is a no-op (row already gone).
- **`ack_for_unknown_invocation_is_noop`** — call with an
  id that has no row; no panic, no error.
- **`ack_with_mismatched_worker_id_is_ignored`** — payload
  `worker_id` differs from consumer's; verify the row is
  *not* deleted and a warn is logged.

#### Unit tests

- **`parse_ack_event_extracts_invocation_id`** — pure: build
  an event, extract id from envelope.

#### Done when

- [x] `ArchiveAckConsumer` exists alongside the existing
      heartbeat producer in `fq-runtime::worker`.
- [x] Defense-in-depth `worker_id` check in place.
- [x] Failure policy is log-and-continue.
- [x] All listed tests green.

---

### Step 6 — Retry sweeper, warn threshold, fq.toml

**Goal.** A periodic worker task republishes
`invocation.archived` for any row in archive flow
(`terminal_at IS NOT NULL AND archive_status IN (NULL,
'pending')`). Past the configured *warn-after* threshold the
sweeper still republishes, but logs a single warn per row.

**What shipped (`1653ab5`).** `ArchiveRetrySweeper`. Two
config knobs on a new `[worker]` section:

```toml
[worker]
archive_retry_interval_ms = 10000   # republish cadence
archive_warn_after_ms     = 60000   # log once at this age
```

The sweeper:

1. Sleeps `archive_retry_interval_ms`.
2. Calls `list_archive_pending`; for each row, republishes
   `invocation.archived` (using the row's stored state) and
   calls `set_archive_pending(invocation_id, now_ms)` to bump
   `archive_published_at`.
3. If `(now_ms - terminal_at) > archive_warn_after_ms`, calls
   `maybe_warn_once(&row, now_ms, &mut warned)` — a
   `HashSet<String>` of invocation ids ensures one warn per
   row across the sweeper's lifetime.

The control-plane's coordination consumer is idempotent on
`invocation_id`; duplicate republishes are safe. The sweeper
is also the recovery path for: control-plane temporarily
offline; worker crashed between marking pending and the
original publish (terminal but `archive_status` NULL —
picked up via list ordering); ack lost in transit.

Past the warn threshold the sweeper *never* deletes the row —
the "correctness over cleanup" rule. Operator action is
required to clear an unrecoverable row.

Wired into the `fq run` lifecycle alongside
`ArchiveAckConsumer` (same select-on-handle-or-shutdown
pattern, 5s graceful shutdown).

#### Acceptance test

```text
TEST: sweep_republishes_pending_terminal_rows

Setup:    Live NATS. Worker started. Control-plane consumer
          NOT started: use a custom consumer name and don't
          run the real CoordinationConsumer.
Action:   Trigger a scripted reducer to Complete.
Assert:   Within ~25 seconds, the same invocation_id appears
          on `fq.agent.<id>.invocation.archived` at least
          two times (republish observed).
          Start the CP consumer; within 5s the worker rows
          are cleaned up.
```

Gated on `FQ_NATS_URL`.

#### Integration tests

- **`sweep_republishes_pending_terminal_rows`** — the
  acceptance test above, NATS-gated.
- **`sweep_warns_once_after_threshold`** — drive a row past
  the warn threshold; capture logs; verify a single
  warn-level log per row across multiple sweep ticks.

#### Unit tests

- **`maybe_warn_once_fires_only_once_per_invocation`** —
  pure: call the helper repeatedly with the same id past
  the threshold; verify the `HashSet` gate.

#### Configuration

`fq.toml` (defaults shown):

```toml
[worker]
archive_retry_interval_ms = 10000
archive_warn_after_ms     = 60000
```

`fn default_archive_retry_interval_ms` /
`fn default_archive_warn_after_ms` in `config.rs` return the
constants `DEFAULT_RETRY_INTERVAL_MS` /
`DEFAULT_WARN_AFTER_MS` re-exported from
`worker::archive_retry`.

#### Done when

- [x] Worker spawns `ArchiveRetrySweeper` alongside the
      archive-ack consumer.
- [x] `[worker]` section honoured;
      `archive_retry_interval_ms` and `archive_warn_after_ms`
      both default and parse correctly.
- [x] Sweeper never deletes a row past the warn threshold;
      operator-facing warn fires once per row.
- [x] Integration tests green.
- [ ] Acceptance test green against live NATS — deferred,
      see parent plan.

---

### Step 7 — Documentation and closing

**Goal.** Make the new behaviour discoverable and close the
plan.

**What shipped.** Parent plan got a Step 8 status block (same
shape as Step 7's) in `5c4f90d`, listing the eight commits,
the renamed integration tests, and the (then-)deferred live
acceptance test. `control_plane/mod.rs`'s topology comment
was extended to list the coordination consumer (now covering
both `invocation.ambiguous` and `invocation.archived`) and
the heartbeat consumer. The design-doc updates listed below
landed on 2026-05-18 once the work was verified end-to-end
against live NATS (which also surfaced and fixed the
`fq.worker.>` stream-binding bug in `cffe16b`). No separate
closing report under `docs/plans/closed/` — the parent plan's
status block is the canonical record.

#### Done when

- [x] Update `docs/design/committed/event-schema.md` to document the
      two new event types and their canonical position
      (`completed → invocation.archived → invocation.archive_acked`).
- [x] Update `docs/design/committed/data-architecture.md` §5.5 with
      the worker's archive emission write order.
- [x] Update `fq.toml` template / operator docs with the
      `[worker]` keys.
- [x] Parent plan's step-8 status block reflects what
      shipped.
- [x] `control_plane/mod.rs` topology comment updated.
- [x] Move this plan to `docs/plans/closed/`.

## Cross-cutting concerns

- **No regression on existing tests.** Full
  `cargo test -p fq-runtime` was green at each commit (245
  lib tests pass without NATS available by skipping the
  gated ones).
- **Reducer-equivalence tests survive.** Adding
  `invocation.archived` is non-breaking; the equivalence
  fixtures tolerate the new tail event on the reducer path.
- **Documentation lands with the code.** The parent plan got
  the status block in the closing commit; the design-doc
  updates listed in Step 7 landed in a follow-up commit on
  2026-05-18 once the work was verified end-to-end.

## Risks and what we'll learn

| Risk | What would tell us | Outcome / mitigation |
|---|---|---|
| Republish cadence interacts badly with JetStream redelivery | The sweep test sees the same invocation_id archived 3+ times within 25s, or duplicate acks confuse the worker | Worker's `invocation_state.archive_status` is the source of truth; once the row is deleted, further acks are no-ops. Covered by `ack_handler_idempotent_on_redelivery` and `ack_for_unknown_invocation_is_noop`. |
| Cleanup scope on ack is too narrow (no WAL purge) | Long-lived `tool_dispatch` / `llm_dispatch` rows pile up after archival | Accepted for v1: row counts are bounded by terminal-invocation lifecycle. Step 10's retention sweep is the planned home for the broader cleanup story. |
| `archive_acked` arrives at a worker that no longer owns the invocation (post-restart on a different worker_id) | Held row on the original worker forever | v1 is single-worker; subject is worker-scoped so the ack reaches the original worker on restart. Multi-worker ownership transfer is a v2 concern noted on the parent plan. |
| `ensure_terminal` covers all failure paths | A future failure-emission site forgets to call it and leaves a non-terminal row | Recovery on restart re-categorises non-terminal rows; the gap is detectable, not silent. |

## Closing condition

This plan is closed when:

- [x] Acceptance tests for steps 3, 4, 5, and 6 are green
      against live NATS (covered by their NATS-gated
      integration tests, verified 2026-05-18 after the
      `fq.worker.>` stream-binding fix in `cffe16b`). The
      parent plan's end-to-end live-NATS+Anthropic acceptance
      test remains deferred and is tracked separately.
- [x] `event-schema.md` and `data-architecture.md` updates
      land (2026-05-18).
- [x] This plan moves to `docs/plans/closed/`.
- [x] Parent plan's step-8 checklist updated to match (2026-05-17,
      commit `5c4f90d`).
