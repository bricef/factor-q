# Plan: Data Architecture v1 (single-node, role-aware)

**Date**: 2026-04-28
**Status**: Active
**Design references**:
- [`docs/design/data-architecture.md`](../../design/data-architecture.md) — the architectural commitments this plan implements.
- [`docs/design/data-architecture-requirements.md`](../../design/data-architecture-requirements.md) — the requirements baseline.

## Goal

Land the v1 (single-node, role-aware) slice of the data
architecture. Tool idempotency cannot be assumed; the runtime
must honour at-most-once-or-flagged dispatch with a three-state
WAL, persist reducer state at every step boundary, and surface
ambiguous middle-state cases to the operator on recovery. v1
collapses control-plane and worker into one `fq run` process
but enforces the role boundary internally so v2 (multi-node)
is a deployment change, not a redesign.

## Approach: TDD per step

For every step in [§Implementation Steps](#implementation-steps),
the order is:

1. **Write the acceptance test first.** The end-to-end
   behaviour the step must produce. Initially red.
2. **Drop to integration tests** for cross-component behaviour
   exercised by the acceptance test. Initially red.
3. **Drop to unit tests** for the smaller components. Initially
   red.
4. **Implement** until all three tiers pass.
5. **Refactor** with all tests green.

The per-step "Done when" checklist is the green-bar contract for
that step. A step doesn't close until its acceptance test runs
green against live infrastructure where applicable.

### Test tiers

| Tier | Where it runs | Speed | What it covers |
|---|---|---|---|
| **Unit** | `cargo test` (no env vars) | <1s/test | Pure functions, single types. No I/O. |
| **Integration** | `cargo test` (no env vars) | seconds | SQLite (in-memory or tempdir), no network. Multi-component within the runtime. |
| **Acceptance** | `cargo test` gated on `FQ_NATS_URL` (and `ANTHROPIC_API_KEY` where indicated) | seconds to tens of seconds | Full daemon, real NATS, sometimes real LLM. End-to-end behaviour. |

Acceptance tests skip silently when their gating env vars are
unset, mirroring the existing `executor::tests` and
`reducer::runner::tests` pattern. Pre-merge requires all unit
and integration tests pass; acceptance tests run in a separate
phase.

### Test harness preliminaries

These exist or need light extensions before step 1 starts. Not
their own step — they're the foundation each step's tests build
on.

| Asset | Status | What's needed |
|---|---|---|
| SQLite tempdir fixture | exists in `projection::store::tests` | Generalise to a shared helper used by control-plane and worker stores. |
| NATS test infra | exists (`FQ_NATS_URL` gated) | None. |
| Mock `LlmClient` | exists in `reducer::harness::tests` | None. |
| Event capture helper | partial (subscribers in equivalence tests) | Generalise to a "subscribe and collect for N seconds" helper. |
| **Crash-simulation helper** | NEW | A way to drop a host loop mid-step deterministically, so recovery tests can target a specific WAL state. |
| **Event-sequence assertion helper** | NEW | Given a captured event list, assert presence of `tool.dispatched`, `llm.dispatched` in the right order; reused across steps 4–8. |

## Implementation Steps

### Step 1 — Internal role split inside `fq-runtime`

**Goal.** Introduce `control_plane` and `worker` modules with a
trait-defined boundary. v1 ships as one process; the boundary
is a Rust trait, not a network boundary. No behavioural change.

#### Acceptance test

```text
TEST: existing_e2e_behaviour_unchanged_after_role_split

Setup:    A trivial agent (sample-responder) runs through
          `fq trigger sample-agent --reducer` against live NATS.
Pre:      Capture the event sequence on `events.>` for the run.
Action:   Re-run the same agent after the role split.
Assert:   Event sequence is identical (modulo timestamps and
          UUIDs). Same outcome text. Same cost.
```

Gated on `FQ_NATS_URL`; runs as part of `cargo test` with the
env var set.

#### Integration tests

- **`control_plane_and_worker_communicate_only_through_defined_interface`** — module visibility test: control-plane code that imports a worker private item fails to compile. Enforced via `pub(crate)` and Rust module structure; verified by a deliberate broken-import test under `#[cfg(compile_fail)]` (or by code review and a linting check).
- **`fq_run_starts_both_roles`** — `fq run` daemon startup constructs a `ControlPlane` and a `Worker` instance and connects them via the in-process trait. Test boots a daemon to the "ready" state and verifies both components reported initialisation.
- **`existing_reducer_equivalence_tests_pass`** — `equivalent_event_sequence_for_simple_completion` and `equivalent_event_sequence_for_tool_call_loop` from the reducer crate continue to pass after the split.

#### Unit tests

- **`control_plane_trait_round_trip`** — instantiate a control-plane stub; pass an invocation claim; verify ownership recorded. Stubs the worker side.
- **`worker_trait_round_trip`** — instantiate a worker stub; receive an invocation claim from a control-plane stub; verify the worker accepts and starts running it.
- **`role_construction_in_isolation`** — each role is independently constructable for testing without the other. Smoke test on each constructor.

#### Done when

- [ ] `cargo test -p fq-runtime` is green (all existing tests pass)
- [ ] `fq trigger sample-agent --reducer` produces identical events as before the split
- [ ] No worker-private types are reachable from control-plane code (verified by structure)
- [ ] Module documentation in `control_plane/mod.rs` and `worker/mod.rs` describes the role boundary
- [ ] `cargo doc -p fq-runtime` builds clean

---

### Step 2 — Worker schema migration

**Goal.** Create `invocation_state`, `tool_dispatch`,
`llm_dispatch` tables. Add `WorkerStore` (or extend
`ProjectionStore`) to manage them.

#### Acceptance test

```text
TEST: fresh_fq_run_creates_worker_tables

Setup:    Empty cache dir.
Action:   Start `fq run` (single-node, both roles).
Assert:   `events.db` contains the new worker tables with
          their indexes. Daemon startup logs note the schema
          version. Daemon reaches "ready" state.
```

Gated on `FQ_NATS_URL`.

#### Integration tests

- **`worker_store_open_creates_tables_on_fresh_db`** — open against an empty SQLite file; verify tables and indexes exist.
- **`worker_store_open_against_existing_v0_db_applies_migration`** — open against a pre-migration SQLite (the projection-only schema); verify migration runs, schema version advances, projection tables are unchanged.
- **`worker_store_refuses_incompatible_schema`** — open against a SQLite at schema version higher than the binary supports; verify it refuses to start with a clear error.
- **`wal_intent_dispatched_completed_round_trip`** — write each WAL state for a tool dispatch; query each state; verify timestamps and JSON round-trip cleanly.
- **`find_in_flight_invocations_excludes_terminal`** — insert mixed terminal/non-terminal rows; verify the in-flight query returns only non-terminal.
- **`find_ambiguous_dispatches_returns_dispatched_without_completed`** — insert WAL rows in each of the three states; verify only the `dispatched` rows surface.

#### Unit tests

- **`write_intent_sets_status_and_intent_at`** — pure-ish: open in-memory DB, call `write_intent`, query.
- **`write_dispatched_transitions_intent_row`** — write intent, then dispatched; verify row updated in place.
- **`write_completed_transitions_with_result`** — write all three; verify `result`, `is_error`, `completed_at` populated.
- **`schema_version_lookup`** — pure function; assert it reads the version row correctly.
- **`schema_version_compatibility_check`** — pure function over a (binary_version, db_version) pair; cases for compatible / incompatible / fresh.

#### Done when

- [ ] All listed integration tests green
- [ ] All listed unit tests green
- [ ] Acceptance test green against live NATS
- [ ] `fq run` startup logs show the schema version
- [ ] No regression in projection tests (`cargo test projection::`)

---

### Step 3 — Control-plane schema migration

**Goal.** Create `coordination_worker`,
`coordination_invocation_owner`, `schedule_entry`,
`pending_wait`, `invocation_archive` tables. Add
`ControlPlaneStore` to manage them.

#### Acceptance test

```text
TEST: fresh_fq_run_creates_control_plane_tables

Setup:    Empty cache dir.
Action:   Start `fq run` (single-node, both roles).
Assert:   `events.db` contains the new control-plane tables
          alongside the worker tables and the projection
          tables. Self-registration row exists in
          `coordination_worker` for the local worker.
```

Gated on `FQ_NATS_URL`.

#### Integration tests

- **`worker_registration_round_trip`** — register a worker; query; verify row.
- **`worker_heartbeat_updates_last_heartbeat`** — register, then heartbeat twice; verify timestamp advances.
- **`worker_marked_stale_after_heartbeat_lapse`** — register, advance time past staleness threshold; query stale workers; verify presence.
- **`invocation_ownership_round_trip`** — assign an invocation to a worker; query by `invocation_id`; query by `worker_id`.
- **`pending_wait_insert_and_signal`** — insert a wait; signal it; verify row removed.
- **`schedule_entry_due_query`** — insert entries at various `fire_at`; query "due before now"; verify only past-due returned.
- **`archive_insert_and_retention_query`** — insert archived invocations with varying `archived_at`; verify retention sweep query returns only old ones.

#### Unit tests

- **`heartbeat_staleness_logic`** — pure function over `(last_heartbeat_ms, now_ms, threshold_ms)`.
- **`schedule_due_predicate`** — pure function over `(fire_at, now_ms)`.
- **`retention_sweep_predicate`** — pure function over `(archived_at, now_ms, retention_days)`.

#### Done when

- [ ] All listed integration tests green
- [ ] All listed unit tests green
- [ ] Acceptance test green against live NATS
- [ ] Local worker self-registration happens automatically on `fq run` start
- [ ] No regression in earlier steps

---

### Step 4 — Three-state WAL writes in `ReducerRunner`

**Goal.** Persist `intent` / `dispatched` / `completed` around
every tool dispatch and LLM call. Emit `tool.dispatched` and
`llm.dispatched` events alongside.

#### Acceptance test

```text
TEST: live_invocation_produces_wal_and_dispatched_events

Setup:    `file-reader` agent (uses tool calls).
          Live NATS, live Anthropic.
Action:   `fq trigger file-reader "read README.md" --reducer`.
Assert:   Captured event stream includes `tool.dispatched`
          between every `tool.call` and `tool.result`, and
          `llm.dispatched` between every `llm.request` and
          `llm.response`. After completion, every WAL row is
          `status = 'completed'`. No stuck `intent` or
          `dispatched` rows remain. Final outcome and cost
          match the legacy executor for the same scripted
          input.
```

Gated on `FQ_NATS_URL` and `ANTHROPIC_API_KEY`.

#### Integration tests

- **`tool_dispatch_writes_intent_then_dispatched_then_completed_in_order`** — drive a single scripted tool dispatch through the runner with a stub LLM and a stub tool; query the WAL after each phase; assert state transitions in order.
- **`tool_dispatched_event_emitted_between_call_and_result`** — same scenario; capture events; assert order.
- **`llm_dispatch_has_same_three_state_shape`** — same shape for LLM calls.
- **`wal_intent_committed_before_nats_publish`** — instrument the runner with a tap on each operation; verify SQLite write completes before NATS publish initiates (per §5.5 write order).
- **`wal_completed_committed_before_tool_result_published`** — symmetric on the back side.
- **`error_path_writes_completed_with_is_error_true`** — tool returns an error; verify WAL row marked completed with `is_error = true`, not stuck in `dispatched`.

#### Unit tests

- **`dispatch_with_wal_emits_correct_state_sequence`** — wraps a fake dispatch operation; mock the store; verify the call sequence is intent → dispatched → completed.
- **`dispatch_with_wal_propagates_tool_error`** — fake dispatch errors; verify completed-with-error written.
- **`event_payload_for_dispatched_carries_tool_call_id`** — pure function; verify shape.

#### Done when

- [ ] All listed integration tests green (with stubs)
- [ ] All listed unit tests green
- [ ] Acceptance test green against live NATS + Anthropic
- [ ] Reducer-equivalence tests still pass (legacy path doesn't get the new events; new equivalence tolerates the addition)
- [ ] Latency overhead measured and reported in the closing notes (target: ≤100ms added per typical invocation, per §5.2)

---

### Step 5 — Persist reducer state on every step boundary

**Goal.** Update `state_blob` synchronously alongside WAL
transitions. After every reducer step, the SQLite row reflects
the post-step state.

#### Acceptance test

```text
TEST: kill_mid_invocation_resumes_cleanly

Setup:    `file-reader` agent. Live NATS, mock LLM (scripted).
Action:   Start the invocation. After the first tool result,
          forcibly drop the `ReducerRunner` (simulating crash).
          Restart the runner against the same invocation_id.
Assert:   Invocation continues from the persisted state. Final
          outcome is identical to running uninterrupted.
          Captured event sequence matches the uninterrupted
          baseline (modulo any duplicate-but-idempotent
          republishes on resume).
```

Gated on `FQ_NATS_URL`.

#### Integration tests

- **`state_blob_updated_after_each_step`** — drive a multi-step invocation; after each step, query `invocation_state.state_blob`; verify it deserialises to the expected `HarnessState` for that step.
- **`state_blob_serialisation_round_trip`** — serialise an `HarnessState`, write to SQLite, read back, deserialise; verify equality.
- **`drop_after_intent_resumes_cleanly`** — using the crash-simulation helper, drop after intent; restart; verify recovery proceeds (same state, no duplicate effect).
- **`drop_after_completed_resumes_at_next_step`** — drop after completed but before the next reducer step; restart; verify the runner consumes the persisted result and proceeds.

#### Unit tests

- **`harness_state_serde_round_trip`** — pure: serialise + deserialise produces an equal value.
- **`persist_state_writes_blob_and_phase`** — fake store; verify both fields written together (single transaction).

#### Done when

- [ ] All listed integration tests green
- [ ] All listed unit tests green
- [ ] Acceptance test green against live NATS
- [ ] Crash-simulation helper documented and reusable

---

### Step 6 — Worker recovery path

**Goal.** On worker startup, categorise in-flight invocations
into safe-resume / safe-replay / ambiguous, and act
accordingly.

#### Acceptance test

```text
TEST: worker_recovery_handles_three_categories

Setup:    Pre-populated worker SQLite with three invocations:
          - inv_safe_resume: `intent` row, no NATS event
          - inv_safe_replay: `completed` row, no next-step
          - inv_ambiguous: `dispatched` row, no `completed`
Action:   Start `fq run` (worker role).
Assert:   - inv_safe_resume re-emits intent event and
            re-dispatches (visible on NATS)
          - inv_safe_replay continues: next reducer step
            emits `llm.intent` (or terminates with the
            persisted result)
          - inv_ambiguous emits `invocation.ambiguous` to
            NATS, is held; no auto-recovery
          Daemon startup output includes the categorisation
          counts.
```

Gated on `FQ_NATS_URL`.

#### Integration tests

- **`categorise_in_flight_groups_correctly`** — populate WAL rows in each state; call `categorise_in_flight`; assert the right partition.
- **`safe_resume_re_emits_intent_event`** — populate a safe-resume row; trigger recovery; capture events; verify intent event is re-emitted (idempotent on `event_id`).
- **`safe_replay_consumes_completed_result`** — populate a safe-replay row with a `completed` payload; trigger recovery; verify the runner picks up `last_result` and emits the next reducer step's events.
- **`ambiguous_emits_invocation_ambiguous_and_holds`** — populate ambiguous; trigger recovery; verify event emitted; verify invocation isn't re-dispatched; verify it stays in `coordination_invocation_owner` with status `ambiguous`.
- **`recovery_runs_concurrent_with_new_triggers`** — a new trigger arrives while recovery categorises; new trigger isn't blocked.

#### Unit tests

- **`categorise_in_flight_safe_resume`** — pure: WAL rows → category.
- **`categorise_in_flight_safe_replay`** — same.
- **`categorise_in_flight_ambiguous`** — same.
- **`categorise_handles_no_dispatch_rows`** — a state with no dispatch yet (just `invocation_state` row) → safe-resume from the persisted reducer phase.

#### Done when

- [ ] All listed integration tests green
- [ ] All listed unit tests green
- [ ] Acceptance test green against live NATS
- [ ] Worker startup logs include the categorisation summary

---

### Step 7 — Control-plane recovery path

**Status (2026-05-16): substantively complete.** The
coordination consumer for `invocation.ambiguous` shipped in
commit `a63ef8c`, including the periodic stale-worker sweep
wired into `fq run` startup. Worker heartbeat emission and
the receiving heartbeat consumer landed in a 6-commit series
on 2026-05-16 ending at the wiring commit; the sweep now
has fresh `last_heartbeat` data and no longer mass-marks
workers stale.

Two items remain, both legitimately deferred:

1. ~~Worker heartbeat emission~~ — **done.** Event-based
   design: workers publish `fq.worker.{worker_id}.heartbeat`
   on a 10s cadence; the `HeartbeatConsumer` updates
   `coordination_worker.last_heartbeat`. Shared
   `events::subjects::validate_token` introduced alongside,
   and `WorkerId` newtype mirroring `AgentId`.
2. **`invocation.archived` / `invocation.archive_acked`
   handling.** Deferred to step 8 (where the events are
   first published). The coordination consumer's
   `_ => Ok(())` arm catches unknown variants today.
3. ~~**Acceptance test surface.**~~ **Shipped 2026-05-22**
   via the acceptance harness plan. End-to-end stale-worker
   detection covered by
   `test_support::runtime::tests::stale_worker_marked_stale_within_threshold`.

The natural next move is **step 8 (archive hand-off)**. It
will pull item 2 along with it as a natural consequence,
since the consumer arm for `invocation.archived` is the
worker-side write that step 8 is about.

**Goal.** Control-plane on restart subscribes to
`invocation.ambiguous`, `invocation.archived`,
`invocation.archive_acked`, and reconciles coordination state
with live worker membership.

#### Acceptance test

```text
TEST: control_plane_recovery_aggregates_cross_worker_state

Setup:    Pre-populated control-plane SQLite with one
          ambiguous invocation owned by w-001.
          NATS has a backlogged `invocation.ambiguous` event
          for inv_other.
Action:   Start `fq run` (control-plane role); wait for
          recovery.
Assert:   - Both ambiguous invocations are now visible via
            `fq invocation list --status=ambiguous`
          - Worker w-001 is shown as stale (no heartbeat)
            via `fq workers stale`
          - New triggers can be dispatched to other workers
            without delay
```

Gated on `FQ_NATS_URL`.

#### Integration tests

- **`control_plane_subscribes_to_invocation_events`** — start the CP; emit `invocation.ambiguous`; verify ownership status updated.
- **`control_plane_handles_backlogged_events_on_startup`** — pre-publish an event before CP starts; CP starts; consumes; updates state.
- **`control_plane_reconciles_stale_workers`** — pre-populate a worker with old heartbeat; CP starts; verifies status is `stale`.
- **`control_plane_idempotent_on_event_redelivery`** — emit the same `invocation.ambiguous` twice; second is no-op.

#### Unit tests

- **`invocation_ambiguous_handler_updates_ownership`** — fake store; verify update call.
- **`invocation_archived_handler_writes_archive_row`** — fake store; verify insert call.
- **`worker_stale_predicate`** — already covered in step 3 unit tests; spot-check.

#### Done when

- [ ] All listed integration tests green
- [ ] All listed unit tests green
- [ ] Acceptance test green against live NATS
- [ ] Control-plane subscription topology documented in `control_plane/mod.rs`

---

### Step 8 — Worker → control-plane archive hand-off

**Status (2026-05-17): substantively complete.** Shipped as
8 commits on `worktree-inherited-dancing-reef`:

1. `InvocationArchived` / `InvocationArchiveAcked` event
   variants + subjects (`fq.agent.{id}.invocation.archived`
   from worker, `fq.worker.{id}.invocation.archive_acked`
   from CP — worker-scoped ack so a single subscription
   filters cleanly).
2. Worker store v4 migration adds `archive_status` and
   `archive_published_at` to `invocation_state`; methods
   `set_archive_pending` / `list_archive_pending`. The
   existing `delete_invocation_state` is reused on ack
   receipt.
3. `WorkerId` threaded into `ReducerRunner` so the archive
   payload can be stamped.
4. `ReducerRunner` emits `InvocationArchived` on Complete
   and on `emit_failed` (the latter via a new
   `ensure_terminal` helper that closes a pre-existing gap
   where LLM-error / budget-exceeded mid-step paths left
   `invocation_state` non-terminal).
5. Coordination consumer handles `InvocationArchived`:
   inserts the archive row (idempotent on `invocation_id`),
   flips ownership to `Completed`, publishes
   `InvocationArchiveAcked`.
6. `ArchiveAckConsumer` (worker side) subscribes via core
   NATS, deletes the local row on receipt.
7. `ArchiveRetrySweeper` (worker side) republishes pending
   rows on a configurable cadence; warns once per row past
   the configured threshold; never deletes.
8. This commit: topology comment + plan close.

Remaining work, intentionally deferred:

1. ~~**Live acceptance test against NATS + Anthropic.**~~
   **Shipped as a mock-based acceptance test on 2026-05-18**
   (mock-llm-test-harness plan). `coordination_consumer::tests::completed_invocation_archives_and_worker_cleans_up_against_mock`
   exercises the full ReducerRunner → CoordinationConsumer →
   ArchiveAckConsumer pipeline against `MockAnthropicServer`,
   gated only on `FQ_NATS_URL`. The real-Anthropic drift
   detector lives at
   `llm::genai::tests::anthropic_real_api_basic_response_parses`,
   marked `#[ignore]` and runnable via `just acceptance-drift`.
2. **Some planned tests not written under the original
   names.** The plan listed
   `worker_emits_archived_on_terminal`,
   `control_plane_consumes_archived_writes_row_acks`,
   `worker_deletes_local_row_on_ack`,
   `hand_off_idempotent_on_archived_redelivery`,
   `hand_off_window_timeout_logs_and_holds`. The same
   behaviours are covered (each NATS-gated) by:
   - `runner::tests::complete_emits_invocation_archived_and_marks_row_pending`
   - `coordination_consumer::tests::handler_archives_invocation_and_publishes_ack`
     (also covers idempotency)
   - `archive_ack::tests::ack_deletes_matching_invocation_state_row`
   - `archive_retry::tests::sweep_republishes_pending_terminal_rows`
   - `archive_retry::tests::sweep_warns_once_after_threshold`
3. **Retry-from-CP-outage end-to-end acceptance test.**
   **Shipped 2026-05-22** via the acceptance harness.
   `test_support::runtime::tests::retry_sweeper_recovers_from_cp_outage`
   drives a real invocation with no CP, then starts CP and
   verifies cleanup lands — proving the sweeper republished
   under live conditions.

**Goal.** Worker emits `invocation.archived` on terminal;
control-plane writes archive row and emits
`invocation.archive_acked`; worker deletes its local row.

#### Acceptance test

```text
TEST: completed_invocation_archives_and_worker_cleans_up

Setup:    Live NATS. Single-node `fq run`.
Action:   Trigger sample-agent; wait for completion; wait an
          additional 5 seconds (hand-off window).
Assert:   - Worker's `invocation_state` no longer has the row
          - Control-plane's `invocation_archive` has the row
            with the final state
          - NATS shows `invocation.archived` and
            `invocation.archive_acked` events in order
```

Gated on `FQ_NATS_URL` and `ANTHROPIC_API_KEY` (or scripted
LLM equivalent).

#### Integration tests

- **`worker_emits_archived_on_terminal`** — drive an invocation to terminal; capture events; verify `invocation.archived` includes the final state blob.
- **`control_plane_consumes_archived_writes_row_acks`** — emit `invocation.archived`; verify archive row written and `invocation.archive_acked` emitted.
- **`worker_deletes_local_row_on_ack`** — after ack, verify worker's `invocation_state` row removed.
- **`worker_holds_row_when_control_plane_down`** — control-plane offline; worker emits archived; ack never arrives; verify worker retains the row past the configured window (with backoff retry on republishing the event).
- **`hand_off_idempotent_on_archived_redelivery`** — emit `invocation.archived` twice; second is no-op (control-plane keys on `invocation_id`).
- **`hand_off_window_timeout_logs_and_holds`** — control-plane never acks; configurable timeout (default 60s) elapses; worker logs an error and *does not delete* the local row (correctness over cleanup).

#### Unit tests

- **`archive_payload_construction`** — pure: build `invocation.archived` event from final `HarnessState`.
- **`hand_off_state_machine_transitions`** — pure: state transitions on archived → ack-pending → ack-received → cleaned-up; and on archived → ack-pending → timeout → held.

#### Done when

- [x] All listed integration tests green (under the renamed
      test names above; NATS-gated, 258 lib tests pass against
      live NATS as of 2026-05-18)
- [x] All listed unit tests green
- [x] Acceptance test green via mock — `coordination_consumer::tests::completed_invocation_archives_and_worker_cleans_up_against_mock`
      runs every CI build. Real-API drift detector at
      `anthropic_real_api_basic_response_parses` (`#[ignore]`'d)
      runs via `just acceptance-drift` when an `ANTHROPIC_API_KEY`
      is available.
- [x] Hand-off timeout configurable via `fq.toml` — new
      `[worker]` section with `archive_retry_interval_ms` and
      `archive_warn_after_ms`
- [x] Worker logs are clear when an invocation is held due to
      ack timeout — `ArchiveRetrySweeper::maybe_warn_once`
      fires one `warn!` per row past the threshold

---

### Step 9 — `fq recover` and `fq workers` commands

**Status (2026-05-22): substantively complete.** Shipped as
six commits on `main` (`5cf41ea..8ee44b2`) following the
[step-9 operator-CLI plan](../closed/2026-05-22-operator-cli.md).
The surface that landed is slightly reshaped from the
original sketch:

- `fq invocation list/show/drop` — per-invocation triage
- `fq workers list/show` — worker liveness inspection
- `fq status` — gained a "Recovery state" section with
  counts and command hints

`fq recover` (top-level) is **deferred** as a follow-up: the
sub-plan reframed it as node-level scope (worker / control-
plane), distinct from per-invocation triage which now lives
under `fq invocation`. `resume` action is also deferred —
the CP doesn't carry the worker's `state_blob` for
ambiguous invocations, so honest resume needs further work
(see the closed sub-plan's Decisions section).

New event variant introduced: `invocation.operator_recovered`
(see `docs/design/event-schema.md`). Audit can filter
operator-triggered terminal transitions from worker-
triggered ones. The `invocation.archived` handler gained a
no-downgrade guard so a late `archived` from a still-alive
worker doesn't override the operator's `Failed`.

**Goal.** Operator-facing CLI for triaging ambiguous
invocations and inspecting workers.

#### Acceptance test

```text
TEST: fq_recover_round_trip_against_real_ambiguous_invocation

Setup:    Provoke an ambiguous case (drop a runner mid-tool;
          restart; verify it's marked ambiguous).
Action 1: Run `fq recover` non-interactively with `--id
          inv_xxx --action drop`.
Assert 1: - Coordination row updated to terminal-failed
          - `invocation.failed` event emitted
          - `fq invocation list --status=ambiguous` no longer
            shows it
Action 2: Run `fq workers list`.
Assert 2: Output shows the local worker, its heartbeat
          freshness, and its current invocation count.
```

Gated on `FQ_NATS_URL`.

#### Integration tests

- **`fq_recover_lists_no_ambiguous`** — no ambiguous; command prints "nothing to recover" and exits 0.
- **`fq_recover_drop_action`** — one ambiguous; `--action drop` marks failed; emits `invocation.failed`; coordination updated.
- **`fq_recover_resume_action`** — `--action resume` treats as completed; emits a synthetic `invocation.archived` with the persisted state and `is_error=false`.
- **`fq_recover_skip_action`** — leaves the invocation in ambiguous state; coordination unchanged.
- **`fq_workers_list_shows_alive`** — fresh heartbeat; status shown as alive.
- **`fq_workers_stale_filters_correctly`** — stale worker present; alive worker absent.
- **`fq_invocation_drop_marks_failed`** — drop a non-ambiguous invocation; verify failed status and event.

#### Unit tests

- **`recover_action_parser`** — pure: `--action drop|resume|skip` parsed; invalid values rejected.
- **`workers_list_formatting`** — pure: format function over a list of `(worker_id, status, last_heartbeat)`.
- **`invocation_drop_command_validation`** — pure: invocation_id format checked.

#### Done when

- [x] Integration coverage for the new surface lives in
      `fq-cli`'s `invocation_tests` and `workers_tests`
      modules (NATS-gated where relevant); pre-existing
      step-7/8 NATS-gated tests still pass against live NATS
      (262 lib tests pass as of 2026-05-22).
- [x] Pure unit tests cover the parser (`parse_invocation_status_filter`),
      the recovery-guidance renderer (`render_recovery_guidance`),
      the heartbeat-age formatter (`format_heartbeat_age_human`),
      and the JSON shape of `InvocationListItem`/`WorkerListItem`.
- [x] Live-NATS end-to-end acceptance test — **shipped 2026-05-22**
      via the acceptance harness plan:
      `test_support::runtime::tests::drop_ambiguous_terminates_invocation_end_to_end`
      drives a real ambiguous case and a real operator drop
      through the live CP dispatch loop.
- [x] `fq invocation --help`, `fq workers --help` give clear
      usage (clap auto-generated; subcommand docstrings in
      `fq-cli/src/main.rs`).
- [x] CLI output is parseable (`--json` on every command)
      and human-readable (default).
- [ ] `fq recover` (top-level, node-scope) — **deferred** to
      a follow-up plan.
- [ ] `resume` action — **deferred** pending the state-blob
      retrieval design.

---

### Step 10 — Retention sweep

**Goal.** Background task on the control-plane that deletes
archive rows past `state.retention_days`.

#### Acceptance test

```text
TEST: retention_sweep_deletes_old_archives

Setup:    `fq.toml` with `state.retention_days = 1` and
          `state.sweep_interval_seconds = 5`. Pre-populate
          `invocation_archive` with one row aged 2 days and
          one row aged 12 hours.
Action:   Start `fq run`; wait two sweep cycles (~10s).
Assert:   - The 2-day-old row is gone
          - The 12-hour-old row remains
          - Sweep emitted a log line per cycle showing rows
            deleted
          - `fq invocation list --include-archived` confirms
            the state
```

Gated on `FQ_NATS_URL`.

#### Integration tests

- **`sweep_runs_on_schedule`** — start the sweep with a 1s interval; observe two runs in 3s.
- **`sweep_deletes_only_aged_rows`** — populate mixed-age rows; one sweep; verify only old rows deleted.
- **`sweep_idempotent_across_runs`** — populate; sweep twice; second run is a no-op.
- **`sweep_handles_empty_archive`** — no rows; sweep runs without error.
- **`sweep_disabled_when_retention_days_negative`** — `retention_days = -1` disables sweep; verify no deletions.

#### Unit tests

- **`sweep_query_predicate`** — pure: cutoff calculation given `retention_days` and `now`.
- **`config_sweep_interval_parser`** — pure: invalid values rejected; defaults applied.

#### Done when

- [ ] All listed integration tests green
- [ ] All listed unit tests green
- [ ] Acceptance test green against live NATS
- [ ] Sweep configuration documented in the `fq.toml` template
- [ ] Sweep emits a log line per cycle (including no-op)

## Cross-cutting concerns

These do not get their own step; they are honoured at every step.

- **No regression on existing tests.** Each step's "Done when" includes a full `cargo test -p fq-runtime` pass.
- **The reducer-equivalence tests survive every step.** Both the legacy and reducer paths must continue to emit identical canonical event sequences (modulo the new `tool.dispatched` / `llm.dispatched` events on the reducer path; the legacy path doesn't get them in v1).
- **Documentation lands with the code.** Each step updates the relevant guide / design doc as part of the same commit.
- **Latency budgets are checked, not assumed.** Steps 4 and 5 measure WAL write latency on a representative invocation and verify it sits within §5.2's loss bound.

## Risks and what we'll learn

This plan tests the architectural commitments under load for the first time. Three places where the design might survive contact with code differently than expected:

| Risk | What would tell us |
|---|---|
| Sync write latency dominates invocation cost | Step 4 acceptance test latency overhead far exceeds 100ms target. Mitigation: Step 4 closing notes name a follow-up to revisit `state.durability_mode`. |
| The role boundary is hard to enforce in Rust | Step 1 reveals that the `pub(crate)` discipline leaks. Mitigation: introduce a stricter crate split (control-plane and worker as separate crates within the workspace). |
| The hand-off protocol is more complex than three events | Step 8 reveals an edge case (e.g. ack delivery to a different control-plane instance after restart). Mitigation: add the case to the doc's open-questions list and design a fix. |

## Closing condition

This plan closes when:

- All 10 steps have their "Done when" checklists complete.
- The end-to-end acceptance test for step 10 (or a combined end-to-end test) demonstrates a full invocation cycle, an induced crash, recovery, archive, and retention sweep.
- A short closing report is written (in the same shape as `docs/plans/closed/2026-04-25-native-reducer-prototype.md`) capturing what landed, what didn't, and what to do next.

## Successor plans

After close, the natural follow-ups (each its own active plan):

1. **v2 multi-node deployment**: lift the role split into separate processes; design HA control-plane.
2. **Approval-gate UI/flow**: the `pending_wait` table is now committed; the user-facing approval surface is a separate design.
3. **Per-agent durability mode opt-in**: revisit if Step 4 measurements suggest async modes would meaningfully help.
