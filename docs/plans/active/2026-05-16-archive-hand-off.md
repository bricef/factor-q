# Plan: Archive Hand-off (data-architecture-v1 step 8)

**Date**: 2026-05-16
**Status**: Active
**Parent plan**:
[`2026-04-28-data-architecture-v1.md`](./2026-04-28-data-architecture-v1.md) — step 8.
**Design references**:
- [`docs/design/data-architecture.md`](../../design/data-architecture.md) §5.3 (state retention) and §9.3 (new event types).
- Schema: §10 `invocation_archive` table is already implemented in
  `services/fq-runtime/crates/fq-runtime/src/control_plane/store.rs`.

## Goal

Move terminal invocations off the worker and into the
control-plane archive via NATS:

1. Worker emits `invocation.archived` on terminal.
2. Control-plane consumer writes the `invocation_archive` row
   and emits `invocation.archive_acked`.
3. Worker consumes the ack, deletes its local
   `invocation_state` row (and the matching WAL rows).
4. If no ack arrives within the configured hand-off window,
   the worker republishes on a fixed cadence and **holds the
   row past the window** — correctness over cleanup.

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
  post-ack cleanup.
- The reducer runner already stamps `terminal_at` on the
  `invocation_state` row when the reducer reaches Complete
  or Failed (`runner.rs:397` via
  `phase_and_terminal_from`).

## Decisions taken on 2026-05-16

These were agreed upfront and are not relitigated unless code
exposes a flaw.

- **Pending-ack tracking**: a new `pending_archive` table on
  the worker store, mirroring how `pending_wait` is modelled
  on the control-plane. One row per invocation awaiting
  ack. Reasons: durable across worker restarts (in-memory
  loses state); cleaner inspection in sqlite than mixing
  extra columns onto `invocation_state`; matches existing
  precedent.
- **`InvocationArchivedPayload` fields**: `final_phase`
  (`"completed"` | `"failed"`), `final_state_blob: Vec<u8>`,
  `started_at: i64`, `terminal_at: i64`. Maps 1:1 onto
  `InvocationArchiveRow`. No cost or error summary in v1 —
  the state blob carries it.
- **`InvocationArchiveAckedPayload` fields**: empty (unit
  struct). The `invocation_id` is on the envelope.
- **Subjects**: `fq.agent.<id>.invocation.archived` and
  `fq.agent.<id>.invocation.archive_acked`. Both fall under
  the coordination consumer's existing filter; only the
  former is consumed by it. The ack is consumed by a new
  worker-side consumer.
- **Schema ids**: `factor-q/invocation_archived@1` and
  `factor-q/invocation_archive_acked@1`.
- **Republish cadence**: every 10s while pending.
  **Hand-off window**: default 60s; after expiry, log an
  error per republish but **never delete the local row**
  automatically. Operator intervention required for held
  rows.
- **fq.toml**: new key `state.handoff_timeout_seconds`,
  default 60. Republish cadence is a constant (not exposed)
  unless we find a reason to.
- **Control-plane emits `archive_acked` unconditionally** on
  every successful `insert_archive` call, including the
  idempotent-conflict no-op case. Otherwise a redelivered
  `archived` event would never re-trigger the ack and a
  worker that missed the first ack would never clean up.
- **`Completed` / `Failed` events are unchanged**.
  `invocation.archived` is *after* the terminal event, per
  the §9.3 canonical sequence diagram. The reducer runner's
  existing emission stays put.

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
test in this plan (scripted reducers suffice).

## Implementation Steps

### Step 1 — Event types, payloads, subjects

**Goal.** Add the two new event variants, their payloads,
schema ids, subject helpers, and serde tests. No behavioural
change.

#### Acceptance test

None. This is structural. Step 3's acceptance test
transitively proves the payloads serialise correctly when
the worker emits.

#### Integration tests

- **`invocation_archived_round_trips_through_serde`** —
  construct an `InvocationArchivedPayload`, build a full
  `Event`, serialise to JSON, deserialise, assert equality.
- **`invocation_archive_acked_round_trips_through_serde`** —
  same for the ack payload.
- **`subject_for_invocation_archived_is_per_agent`** —
  build an event with
  `EventPayload::InvocationArchived(...)`; verify subject is
  `fq.agent.<id>.invocation.archived`.

#### Unit tests

- **`schema_id_for_invocation_archived`** — pure: maps the
  variant to `factor-q/invocation_archived@1`.
- **`schema_id_for_invocation_archive_acked`** — pure: maps
  to `factor-q/invocation_archive_acked@1`.

#### Done when

- [ ] `EventPayload::InvocationArchived` and
      `EventPayload::InvocationArchiveAcked` variants exist.
- [ ] Subject helpers in `events::subjects` cover both.
- [ ] Schema-id mapping in `events.rs` covers both.
- [ ] All listed tests green.
- [ ] No regression: full `cargo test -p fq-runtime` green.

---

### Step 2 — `pending_archive` table on worker store

**Goal.** Add a new SQLite table and CRUD methods on
`WorkerStore` for pending hand-off tracking. Migration runs
automatically on `WorkerStore::open` against an existing
worker DB.

#### Schema

```sql
-- Invocations the worker has emitted invocation.archived for
-- and is awaiting invocation.archive_acked. Created in step 8
-- of data-architecture-v1.
CREATE TABLE pending_archive (
    invocation_id    TEXT PRIMARY KEY,
    agent_id         TEXT NOT NULL,
    first_emitted_at INTEGER NOT NULL,   -- ms; for timeout
    last_emitted_at  INTEGER NOT NULL,   -- ms; for cadence
    final_phase      TEXT NOT NULL,      -- 'completed' | 'failed'
    final_state_blob BLOB NOT NULL,
    started_at       INTEGER NOT NULL,
    terminal_at      INTEGER NOT NULL
);
CREATE INDEX idx_pending_archive_last_emitted
    ON pending_archive(last_emitted_at);
```

#### Integration tests

- **`worker_store_open_creates_pending_archive_table`** —
  open against a fresh tempdir; verify table and index
  exist.
- **`worker_store_open_against_pre_step_8_db_migrates`** —
  open against a DB built before this step (no
  `pending_archive` table); verify migration creates it and
  preserves existing rows.
- **`pending_archive_insert_and_get_round_trip`** — insert
  via `enqueue_pending_archive`; fetch via
  `get_pending_archive`; assert equality.
- **`pending_archive_delete_removes_row`** — insert,
  delete, re-fetch returns None.
- **`pending_archive_list_due_for_resend`** — insert two
  rows with different `last_emitted_at`; query "due before
  cutoff"; assert only the older row returns.

#### Unit tests

- **`pending_archive_row_serde_round_trip`** — pure: build
  a row struct, serialise via internal encoding, decode,
  assert equality.

#### Done when

- [ ] `WorkerStore::{enqueue_pending_archive,
      get_pending_archive, mark_pending_archive_resent,
      list_pending_archive_due, delete_pending_archive}`
      exist.
- [ ] Migration on a pre-step-8 DB succeeds and preserves
      data.
- [ ] All listed tests green.

---

### Step 3 — Worker emits `invocation.archived` on terminal

**Goal.** When the reducer runner reaches a terminal phase,
it inserts a `pending_archive` row **and** publishes
`invocation.archived` in the same boundary as the terminal
event (`Completed` / `Failed`).

Order: SQLite write first (`pending_archive` row), then
NATS publish, mirroring the §5.5 write order used elsewhere
on the worker. A crash between the two is a safe replay:
recovery (and the resend loop in step 6) re-emits on
restart from the persisted row.

#### Acceptance test

```text
TEST: worker_emits_archived_after_terminal

Setup:    Live NATS. `fq run` (single-node).
Action:   Trigger a scripted reducer that reaches Complete on
          its first step.
Assert:   Captured events include `invocation.archived` after
          `completed`. The payload's `final_state_blob`
          deserialises to a State::Complete{..}. A
          `pending_archive` row exists for the invocation.
```

Gated on `FQ_NATS_URL`.

#### Integration tests

- **`terminal_complete_writes_pending_archive_then_emits`** —
  drive a scripted reducer to Complete via the existing
  test harness; assert (a) the pending row exists, (b) the
  event is on the bus, (c) the order: row write completes
  before publish.
- **`terminal_failed_writes_pending_archive_then_emits`** —
  same for Failed.
- **`archive_emission_is_per_invocation_idempotent`** —
  call the emission helper twice for the same terminal
  state; assert only one `pending_archive` row exists.

#### Unit tests

- **`build_archive_payload_from_terminal_state`** — pure:
  given a `HarnessState::Complete{..}` or `::Failed{..}`,
  produce the matching `InvocationArchivedPayload`.

#### Done when

- [ ] Emission integrated into the reducer runner's
      terminal handling.
- [ ] All listed tests green.
- [ ] Legacy executor path unaffected (it doesn't archive
      in v1; the parent plan's cross-cutting invariant
      holds).

---

### Step 4 — Control-plane consumer handles `invocation.archived`

**Goal.** Replace the `_ => Ok(())` arm in
`coordination_consumer.rs` with a real handler that:

1. Calls `ControlPlaneStore::insert_archive` (idempotent).
2. Emits `invocation.archive_acked` on
   `fq.agent.<id>.invocation.archive_acked` **even when
   insert was a no-op** — see decision above.

If `insert_archive` fails (real store error), the message is
NAK'd and JetStream redelivers — existing pattern.

#### Acceptance test

```text
TEST: control_plane_writes_archive_and_acks

Setup:    Live NATS, fresh control-plane DB.
Action:   Publish an `invocation.archived` event directly.
Assert:   - `invocation_archive` row appears within 2 seconds.
          - `invocation.archive_acked` is published with the
            same `invocation_id` and `agent_id`.
          - Republishing the same `invocation.archived` event
            results in a second ack (idempotent).
```

Gated on `FQ_NATS_URL`.

#### Integration tests

- **`handle_invocation_archived_writes_row_and_publishes_ack`** —
  unit-style: pass the handler a `(store, bus, event)`;
  verify both side effects.
- **`handle_invocation_archived_acks_on_idempotent_no_op`** —
  pre-populate the archive row; deliver the same event;
  verify an ack is still emitted.
- **`handle_invocation_archived_store_error_returns_err`** —
  inject a store failure; verify the handler returns Err
  (NAK path) without publishing an ack.

#### Unit tests

- **`archive_row_from_payload`** — pure: payload + envelope
  → `InvocationArchiveRow` mapping.

#### Done when

- [ ] `coordination_consumer.rs` arm for
      `InvocationArchived` is implemented.
- [ ] All listed tests green.
- [ ] The earlier coordination-consumer end-to-end test
      (`coordination_consumer_handles_invocation_ambiguous_end_to_end`)
      still passes.

---

### Step 5 — Worker consumes `invocation.archive_acked`, cleans up

**Goal.** Add a worker-side durable consumer
(`fq-archive-ack`) filtered on
`fq.agent.*.invocation.archive_acked`. On ack:

1. Delete the `pending_archive` row.
2. Delete the `invocation_state` row
   (`WorkerStore::delete_invocation_state` already exists).
3. Delete all `tool_dispatch` and `llm_dispatch` rows for
   the invocation.

WAL rows are co-located with the invocation; deleting
`invocation_state` does not currently cascade-delete them.
This step adds explicit deletion of the WAL rows. (We do
not rely on `ON DELETE CASCADE` because the WAL tables
don't have an FK to `invocation_state` today.) The three
deletes happen in a single SQLite transaction
(`delete_invocation_artifacts`).

#### Acceptance test

```text
TEST: worker_cleans_up_on_ack

Setup:    Live NATS, `fq run` with both roles.
Action:   Trigger a scripted reducer to Complete.
Assert:   Within 5 seconds of completion:
          - `pending_archive` row removed.
          - `invocation_state` row removed.
          - `tool_dispatch` rows for the invocation removed.
          - `llm_dispatch` rows for the invocation removed.
          - Archive row exists on the control-plane.
```

Gated on `FQ_NATS_URL`.

#### Integration tests

- **`ack_handler_deletes_pending_and_state_and_wal`** —
  populate worker state for an invocation; call the ack
  handler directly; verify all four tables cleared.
- **`ack_handler_idempotent_on_redelivery`** — call twice;
  second call is a no-op (rows already gone).
- **`ack_for_unknown_invocation_is_noop`** — call with an
  id that has no pending row; no panic, no error.

#### Unit tests

- **`parse_ack_event_extracts_invocation_id`** — pure:
  build an event, extract id from envelope.

#### Done when

- [ ] Worker-side `ArchiveAckConsumer` exists alongside the
      existing trigger/heartbeat consumers.
- [ ] `WorkerStore::delete_invocation_artifacts(invocation_id)`
      helper does the three-table cleanup in a single
      transaction.
- [ ] All listed tests green.

---

### Step 6 — Retry, timeout, fq.toml

**Goal.** A periodic worker task republishes
`invocation.archived` for any pending row that hasn't seen
`last_emitted_at` advance recently; once past the
configured timeout, logs an error per republish and stops
counting (the row stays held until an ack or operator
action).

Cadence: every 10s, scan `pending_archive` for rows with
`last_emitted_at < now - 10s`. For each, republish and
update `last_emitted_at`. If
`now - first_emitted_at > handoff_timeout`, log an error
(not just debug) and continue — we still republish, because
the control-plane may yet come back.

#### Configuration

`fq.toml`:

```toml
[state]
handoff_timeout_seconds = 60   # default
```

Republish cadence (10s) is a constant in code for v1.

#### Acceptance test

```text
TEST: pending_archive_republished_until_acked

Setup:    Live NATS. Worker started. Control-plane consumer
          NOT started (simulate CP down): use a custom
          consumer name and don't run the real
          `CoordinationConsumer`.
Action:   Trigger a scripted reducer to Complete.
Assert:   Within 25 seconds, the same invocation_id appears
          on `fq.agent.<id>.invocation.archived` at least
          two times (republish observed).
          Start the CP consumer; within 5s the worker rows
          are cleaned up.
```

Gated on `FQ_NATS_URL`.

#### Integration tests

- **`resend_loop_republishes_due_rows`** — populate a
  pending row with `last_emitted_at = now - 11s`; run one
  iteration of the resend loop; assert publish happened
  and `last_emitted_at` advanced.
- **`resend_loop_skips_recent_rows`** — populate with
  `last_emitted_at = now - 1s`; one iteration; assert no
  publish.
- **`resend_loop_logs_error_past_timeout`** — populate
  with `first_emitted_at = now - 65s`; one iteration;
  capture logs; assert error-level log emitted; assert the
  row is still present (not deleted).
- **`fq_toml_handoff_timeout_seconds_parses`** — load a
  config with the key; verify field populated; verify
  default if absent.

#### Unit tests

- **`pending_due_predicate`** — pure: given
  `(last_emitted_at, now, cadence_ms)` → bool.
- **`pending_past_timeout_predicate`** — pure: given
  `(first_emitted_at, now, timeout_ms)` → bool.

#### Done when

- [ ] Worker spawns an `ArchiveResendTask` alongside the
      heartbeat producer.
- [ ] `state.handoff_timeout_seconds` honoured.
- [ ] Acceptance test green.
- [ ] Documentation in `worker/mod.rs` notes the held-row
      condition and that operator action is required.

---

### Step 7 — Documentation and closing

**Goal.** Make the new behaviour discoverable and close
the plan.

- [ ] Update `docs/design/event-schema.md` to document the
      two new event types and their canonical position
      (`completed → invocation.archived → invocation.archive_acked`).
- [ ] Update `docs/design/data-architecture.md` §5.5 if any
      write-order detail in the worker emission deserves a
      mention.
- [ ] Update `fq.toml` template / docs with
      `state.handoff_timeout_seconds`.
- [ ] Write the closing report
      (`docs/plans/closed/2026-05-16-archive-hand-off.md`)
      summarising what landed, latency measured on a sample
      invocation, and any deferred items.
- [ ] Update the parent plan's step-8 "Done when"
      checklist.

## Cross-cutting concerns

- **No regression on existing tests.** Each step's "Done
  when" includes a full `cargo test -p fq-runtime` pass.
- **Reducer-equivalence tests survive.** Adding
  `invocation.archived` is non-breaking; the equivalence
  fixtures need updating to tolerate the new tail event on
  the reducer path. Document the change in the test file.
- **Documentation lands with the code.** Each step that
  changes the schema or wire format updates the relevant
  design doc in the same commit.

## Risks and what we'll learn

| Risk | What would tell us | Mitigation |
|---|---|---|
| Republish cadence interacts badly with JetStream redelivery | Step 6 acceptance test sees the same invocation_id archived 3+ times within 25s, or duplicate acks confuse the worker | Worker's pending-archive row is the source of truth; once deleted, further acks are no-ops. Verified by `ack_for_unknown_invocation_is_noop`. |
| WAL cleanup alongside `invocation_state` reveals a missing constraint | Orphan WAL row after ack | Step 5 helper is transactional; add a partial-failure test if surfaces. |
| `archive_acked` arrives at a worker that no longer owns the invocation (post-restart on a different worker_id) | Held row on the original worker forever | v1 is single-worker; v2 will need to address ownership in the ack subject. Note as a v2 follow-up. |

## Closing condition

This plan closes when:

- All 7 steps' "Done when" boxes are ticked.
- Acceptance tests for steps 3, 4, 5, and 6 are green
  against live NATS.
- Parent plan's step-8 checklist is updated to match.
- A closing report is written and committed.
