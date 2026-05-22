# Plan: Operator CLI for triage and recovery

**Date**: 2026-05-22
**Status**: Active
**Parent plan**:
[`2026-04-28-data-architecture-v1.md`](./2026-04-28-data-architecture-v1.md) — step 9.
**Design references**:
- [`docs/design/data-architecture.md`](../../design/data-architecture.md) §3.4 (ambiguous cases), §4.4 (operator surface), §7 (recovery).
- [`docs/design/event-schema.md`](../../design/event-schema.md) — adds `invocation.operator_recovered`.

## Goal

Operator-facing CLI for triaging ambiguous invocations and
inspecting worker liveness. After step 7 we have heartbeat
data; after step 8 we have archive data; this step closes the
loop so a human operator can act on what's in the
control-plane without dropping into raw SQLite or `nats sub`.

After this lands, an operator can:

- See what's wrong (`fq status` highlights ambiguous
  invocations and stale workers, recommends next commands).
- Inspect detail (`fq invocation list/show`, `fq workers
  list/show`).
- Take action (`fq invocation drop`) and have the action
  show up in the audit log as a distinct event type.

## Context

What exists after step 8:

- `coordination_invocation_owner` rows with status
  `InFlight | Ambiguous | Completed | Failed` (updated by
  the coordination consumer from `invocation.ambiguous` and
  `invocation.archived` events).
- `coordination_worker` rows with `last_heartbeat` and a
  stale-worker sweep that flips status to `Stale`.
- `invocation_archive` rows for cleanly terminated
  invocations.
- A `fq status` command that prints a NATS/streams/consumers
  health overview but says nothing about coordination state
  or what needs operator attention.

What's missing:

- No command to list ambiguous invocations or workers.
- No way to mark an ambiguous invocation as resolved.
- `fq status` doesn't direct the operator anywhere when
  things are off.

## Decisions taken on 2026-05-22

- **Per-invocation recovery lives under `fq invocation`.**
  Operator actions on a single invocation share a namespace
  with `list / show / drop`. Less scattered than a top-level
  `fq recover` would be.
- **`fq recover` is reserved for node-level scope** — worker
  or control-plane (manager) recovery. Not implemented in
  this step; left as a sketch in the closing notes for a
  follow-up plan. `fq status` will point operators at the
  right per-invocation commands when only invocations need
  attention.
- **`fq invocation drop` is the only mutating verb in v1.**
  Marks the invocation `Failed` and emits a new
  `invocation.operator_recovered` event. Works on any
  current state — operator can drop an in-flight invocation
  if needed (kill switch). Audit record makes the action
  attributable.
- **New event variant `invocation.operator_recovered`.**
  Distinct from `invocation.archived` so audit can filter
  operator-triggered terminal transitions from worker-
  triggered ones. Coordination consumer's handler writes an
  archive row and updates the owner row to `Failed`. Subject
  is `fq.agent.<agent_id>.invocation.operator_recovered` —
  picked up by the coordination consumer's existing filter.
- **`resume` action is deferred to v2.** The control-plane
  doesn't have the worker's `state_blob` for an ambiguous
  invocation (the `invocation.ambiguous` event doesn't
  carry it), and the worker may be dead. Honest semantics
  would require either enriching the ambiguous event with
  the state blob (extra wire traffic on a path that's
  already operator-triage-only) or adding an operator-RPC
  to the worker (new surface for a marginal v1 use case).
  Step 9 ships `drop` + `skip` (skip is implicit — just
  don't run anything); `resume` lands when we have a
  concrete use case demanding it.
- **`fq workers` is read-only in v1.** No `fq workers drop`
  or similar; if a worker needs to die, the operator stops
  the process. The stale sweep handles the bookkeeping.
- **Output format: JSON with `--json`, human-readable by
  default.** Single canonical JSON schema per command;
  emitted via `serde_json::to_string_pretty` over a typed
  output struct so the schema is type-checked.

## Approach: TDD per step

Same shape as previous plans:

1. Acceptance test (red).
2. Integration tests (red).
3. Unit tests (red).
4. Implement until all three tiers pass.
5. Refactor with all tests green.

## Implementation Steps

### Step 1 — `invocation.operator_recovered` event variant

**Goal.** Add the new event type with payload, subject,
schema id, and handler skeleton. No CLI yet.

#### Payload sketch

```rust
pub struct InvocationOperatorRecoveredPayload {
    /// Action the operator took. v1 is always "drop"; the
    /// field exists so future actions (resume, requeue) can
    /// be distinguished without a new variant.
    pub action: String,
    /// Reason supplied by the operator (`--reason` flag).
    /// Free-form. Audit-only; consumers should not parse it.
    pub reason: Option<String>,
    /// Phase the invocation ended at. v1 is always "failed";
    /// future resume action would set this to "completed".
    pub final_phase: String,
}
```

Subject: `fq.agent.<agent_id>.invocation.operator_recovered`.
Schema id: `factor-q/invocation_operator_recovered@1`.

#### Integration tests

- **`invocation_operator_recovered_subject_is_agent_scoped`** —
  build an event; verify subject and schema id.

#### Unit tests

- **`schema_id_for_invocation_operator_recovered`** — pure.

#### Done when

- [ ] `EventPayload::InvocationOperatorRecovered` variant exists.
- [ ] Subject helper in `events::subjects` covers it.
- [ ] Schema-id mapping covers it.
- [ ] All listed tests green.

---

### Step 2 — Control-plane handler

**Goal.** The coordination consumer gains a handler arm for
`InvocationOperatorRecovered`. On receipt:

1. Write an `invocation_archive` row (idempotent on
   invocation_id). State blob is empty if the operator
   action didn't supply one; otherwise carries through from
   the payload. v1 always-empty.
2. Update `coordination_invocation_owner.status` to `Failed`
   (or `Completed` if `final_phase = "completed"`, but v1
   only emits `failed`).
3. **No ack** is emitted — unlike `invocation.archived`,
   there's no worker waiting to clean up. The original
   worker may be dead or oblivious; its local row, if any,
   is left to the worker's own recovery on restart (it'll
   see a terminal coordination state and clean up
   accordingly, or the operator can restart the worker).

#### Integration tests

- **`handler_operator_recovered_writes_archive_and_updates_owner`** —
  pass the handler an event; verify the archive row, owner
  row status, and that no ack is published.
- **`handler_operator_recovered_idempotent_on_redelivery`** —
  redeliver the same event; verify the archive row is
  unchanged (ON CONFLICT DO NOTHING) and owner stays Failed.

#### Done when

- [ ] `coordination_consumer.rs` arm for
      `InvocationOperatorRecovered` is implemented.
- [ ] All listed tests green.
- [ ] Existing step-8 handler tests still pass.

---

### Step 3 — `fq invocation` subcommand

**Goal.** Add the read-side commands first, then `drop`:

- `fq invocation list [--status=<status>] [--agent=<id>] [--include-archived] [--limit=N] [--json]`
- `fq invocation show <id> [--json]`
- `fq invocation drop <id> [--reason "..."] [--json]`

`list` queries `coordination_invocation_owner` (joined with
`invocation_archive` when `--include-archived`). `show`
prints owner row + archive row (if present) + last few
events from the projection. `drop` looks up the agent_id,
publishes `invocation.operator_recovered` with
`action="drop", final_phase="failed", reason=<flag>`, waits
for the archive row to appear (with a timeout), and prints
the new state.

#### Acceptance test

```text
TEST: fq_invocation_drop_terminates_ambiguous

Setup:    Live NATS. Single-node `fq run`. Provoke an
          ambiguous case (drop a worker mid-tool; restart;
          ambiguous event surfaces).
Action:   `fq invocation drop <id> --reason "stuck on
          flaky network call"`.
Assert:   - Archive row appears on CP within 2 seconds.
          - Owner row status is `Failed`.
          - `fq invocation list --status=ambiguous` no longer
            shows the id.
          - `fq invocation show <id>` shows the reason from
            the payload.
```

Gated on `FQ_NATS_URL`.

#### Integration tests

- **`fq_invocation_list_filters_by_status`** — populate
  owner rows of multiple statuses; verify `--status` filter
  returns only matching rows.
- **`fq_invocation_list_no_ambiguous_prints_zero`** — empty
  state; output is "0 invocations" or equivalent, exit 0.
- **`fq_invocation_show_unknown_id_exits_nonzero`** — show
  a fabricated id; expect exit 1 and a clear "not found"
  message.
- **`fq_invocation_drop_emits_operator_recovered`** —
  drive `drop` via the CLI helper; capture the NATS event;
  verify shape and payload.
- **`fq_invocation_drop_in_flight_works`** — drop a
  non-ambiguous in-flight invocation; verify it terminates
  the same way. (The kill-switch path.)

#### Unit tests

- **`parse_invocation_status_filter`** — pure: `--status`
  argument validation. Accepts `in_flight|ambiguous|completed|failed`;
  rejects others.
- **`format_invocation_list_row_human`** — pure: one row →
  one line of human output (agent, status, worker, age).
- **`format_invocation_list_json`** — pure: same with the
  `--json` codepath.

#### Done when

- [ ] `fq invocation list/show/drop` exist and pass tests.
- [ ] `--json` output validates against a fixed
      `InvocationListItem` / `InvocationDetail` schema.
- [ ] `fq invocation --help` is clear about which fields
      each subcommand prints.

---

### Step 4 — `fq workers` subcommand

**Goal.** Read-only worker inspection:

- `fq workers list [--stale-only] [--alive-only] [--json]`
- `fq workers show <id> [--json]`

`list` reads `coordination_worker`, applies filters, prints
`(worker_id, status, last_heartbeat_age, in_flight_count)`.
`show` prints the same plus the worker's last few
ambiguous-event surfacings (if any) from the projection.

#### Integration tests

- **`fq_workers_list_shows_alive_and_stale`** — populate
  with one fresh and one stale heartbeat; `list` shows
  both, `--stale-only` shows the stale, `--alive-only` shows
  the alive.
- **`fq_workers_show_unknown_exits_nonzero`** — clear error
  for unknown worker.

#### Unit tests

- **`format_heartbeat_age_human`** — pure: `now_ms - hb_ms`
  → "12s", "3m", "1h", "stale".
- **`workers_list_json_schema`** — pure: one row → JSON
  matches the documented schema.

#### Done when

- [ ] `fq workers list/show` exist and pass tests.
- [ ] `--stale-only` / `--alive-only` filters work.
- [ ] Heartbeat-age display is consistent across human and
      JSON output (JSON keeps the raw timestamp; human
      computes the age string).

---

### Step 5 — Enhanced `fq status` with recovery guidance

**Goal.** Extend the existing `fq status` to surface
operator-relevant state alongside the runtime health
overview. Output gains two sections:

```
Recovery state:
  Ambiguous invocations: 2
    -> `fq invocation list --status=ambiguous` to inspect
    -> `fq invocation drop <id>` to triage individually
  Stale workers: 1
    -> `fq workers list --stale-only` to inspect
```

When everything is green, the section prints "All clear."

#### Integration tests

- **`fq_status_reports_clean_when_nothing_pending`** —
  populate fresh-heartbeat workers and no ambiguous rows;
  status section says "All clear" and exits 0.
- **`fq_status_reports_counts_and_commands`** — populate
  one ambiguous + one stale worker; status section shows
  both counts and the suggested commands.

#### Unit tests

- **`render_recovery_guidance_for_ambiguous_only`** — pure:
  `(amb=N, stale=0)` → expected text.
- **`render_recovery_guidance_for_stale_only`** — pure.
- **`render_recovery_guidance_all_clear`** — pure.

#### Done when

- [ ] `fq status` shows the new section.
- [ ] Recovery guidance text is unit-tested for the three
      shapes (clear / ambiguous-only / stale-only /
      both).
- [ ] `--json` output gains the corresponding fields.

---

### Step 6 — Documentation and closing

**Goal.** Make the new surface discoverable.

- [ ] Update `docs/design/event-schema.md` to add
      `invocation.operator_recovered` (event type +
      subject row + invariant).
- [ ] Update `docs/design/data-architecture.md` §4.4
      (operator surface) to point at the new commands.
- [ ] Update `services/fq-runtime/README.md`'s testing
      table if any new commands need a recipe.
- [ ] Update parent plan's step-9 status block.
- [ ] Move this plan to `docs/plans/closed/`.

## Cross-cutting concerns

- **No regression on existing tests.** Each step's
  "Done when" includes a full `cargo test -p fq-runtime`
  pass.
- **JSON output is contract.** Once published, the shape
  shouldn't change without bumping a version (operators
  will pipe these into scripts). Document each command's
  schema in `--help` or alongside the events doc.
- **CLI text errors go to stderr, exit codes non-zero.**
  Stdout stays parseable when `--json` is set.

## Risks and what we'll learn

| Risk | What would tell us | Mitigation |
|---|---|---|
| Operators expect `resume` and the v1 deferral hurts | Real-world ambiguous case where drop loses irreplaceable progress | If/when it surfaces, the cheapest fix is to enrich `invocation.ambiguous` with the state blob and add a `resume` action. Doable as a follow-up; the operator_recovered variant already accommodates `final_phase=completed`. |
| Empty `state_blob` on operator_recovered confuses the retention sweep | Step 10 deletes archive rows with an unexpected shape | Step 10 should treat operator-recovered rows the same as any archive row. We'll note this in the step 10 plan. |
| `fq invocation drop` of an in-flight invocation races a live worker | Worker is mid-step on the invocation when operator drops it; worker continues, then publishes `completed` after the operator's drop | The owner row's `Failed` state should win on conflict — the worker's `invocation.archived` arrives later but the coordination consumer's existing idempotency handles the second archive insert. Owner status update needs a "don't downgrade Failed → Completed" guard. Test this. |

## Closing condition

This plan closes when:

- All 6 steps' "Done when" boxes are ticked.
- Parent plan's step-9 status block reflects what shipped.
- Event-schema and data-architecture docs are updated.
- This plan moves to `docs/plans/closed/`.
- A follow-up plan or backlog note exists for `fq recover`
  (node-level), capturing the design questions deferred
  from here (manager recovery, bulk worker recovery,
  `resume` action if/when wanted).
