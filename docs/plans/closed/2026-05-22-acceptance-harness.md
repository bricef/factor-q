# Plan: Live-NATS + real-runtime acceptance harness

**Date**: 2026-05-22
**Status**: Closed 2026-05-22. Seven commits on `main`:
`c5b7a43` (TestRuntime harness), `ad53960` (drop-ambiguous
scenario + extracted operator::drop_invocation), `3577e41`
(stale-worker scenario), `42a9682` (retry-sweeper scenario
+ cross-test contamination fix + projection-consumer flake
fix), `9926882` (drop-vs-late-archived race), `7f6b13b`
(binary smoke test), and the doc-only close (this commit).
Parent plan's step-7/8/9 status blocks updated to mark the
deferred acceptance tests as shipped.

Two real bugs found and fixed inline (per the plan's
"no #[ignore] escape hatch" discipline):

1. **Cross-test contamination via the ack subject.** Parallel
   tests' CoordinationConsumers were processing each other's
   archived events and publishing acks on each other's worker
   subjects, racing the sweepers. Fix: `with_test_filter_subject`
   builder method on CoordinationConsumer; TestRuntime narrows
   the filter to `fq.agent.<test_agent_id>.invocation.*`.
2. **Projection-consumer test flake under accumulated stream
   history.** `consumer_projects_events_into_store` timed out
   replaying days of stream history. Fix: narrow the test
   consumer's filter to its own agent.
**Design references**:
- [`docs/design/committed/data-architecture.md`](../../design/committed/data-architecture.md) §3.4 (ambiguous), §5.5 (archive write order), §7 (recovery).
- [`docs/design/committed/event-schema.md`](../../design/committed/event-schema.md) — every event the harness asserts on.

## Goal

Build a reusable in-process test harness that boots the full
`fq run` runtime against live NATS and `MockAnthropicServer`,
then use it to ship the four end-to-end scenarios that have
been deferred across steps 7, 8, and 9 of data-architecture-v1.

After this lands, future plans (step 10 retention sweep,
phase 2 work, etc.) have a stable scaffold for acceptance
tests — and we close the long-standing gap between the
many component-level NATS-gated tests and a true full-stack
acceptance test.

## Context

What the codebase already has:

- Many NATS-gated component-level tests (250+ pass against
  live NATS as of `2e4b592`).
- `completed_invocation_archives_and_worker_cleans_up_against_mock`
  — one full-pipeline test that constructs the worker side
  (ReducerRunner + ArchiveAckConsumer), a test variant of
  the CoordinationConsumer, MockAnthropicServer, and asserts
  the happy-path archive flow. ~150 lines, all inline.
- `MockAnthropicServer` in `test_support::mock_anthropic`
  — already production-grade.
- `run_test_consumer` in `coordination_consumer.rs` tests
  — dispatches `InvocationAmbiguous`,
  `InvocationArchived`, and `InvocationOperatorRecovered`
  under a custom durable name.

What's missing:

- No reusable harness — every full-stack test re-builds the
  same setup inline. New scenarios pay full setup tax.
- No coverage of failure paths end-to-end: worker crash
  mid-tool, stale worker, CP outage, race conditions.
- No binary-level smoke (`cargo run --bin fq -- --help`
  could break without any test catching it).

## Decisions taken on 2026-05-22

- **In-process harness, with one subprocess smoke test.**
  The runtime's logic lives in `fq-runtime`; binary-level
  bugs are usually CLI-arg parsing or startup logic. An
  in-process harness gives us deterministic, fast scenarios;
  a single subprocess test exercises `fq --help` /
  `fq status` to catch the rare binary-only regression.
- **Harness lives in `fq-runtime::test_support::runtime`.**
  Already-`cfg(test)`-gated module; sibling to
  `mock_anthropic`. fq-runtime's own tests use it directly;
  fq-cli tests can build a stripped-down variant if they
  need to (separate concern, not in scope here).
- **Test isolation by uniqueness, not by reset.** Each
  `TestRuntime::start()` invocation gets a fresh
  `WorkerId`, `AgentId`-prefix, durable consumer name, and
  tempdir for each SQLite store. NATS state is shared across
  tests (we don't `docker compose down -v` between cases);
  uniqueness keeps them from stepping on each other. The
  one exception is the long-running JetStream stream itself
  — its message volume grows over time, occasionally
  causing the projection-consumer test to time out. That's a
  pre-existing flake; we'll note a fix idea in the closing
  notes but not solve it in this plan.
- **Mock LLM only — no live-Anthropic in this harness.**
  The drift detector at
  `llm::genai::tests::anthropic_real_api_basic_response_parses`
  already covers protocol-drift detection (manual,
  `just acceptance-drift`). The harness uses
  `MockAnthropicServer` exclusively so scenarios are
  deterministic.
- **Scenarios match what failure modes are actually
  reachable in v1.** The four selected:
  - Drop ambiguous end-to-end (step 9's deferred acceptance).
  - Stale worker detection (step 7's stale-sweep, full path).
  - Retry sweeper recovers from CP outage (step 8's retry
    sweeper, but exercised through real NATS publish/
    redeliver rather than direct method calls).
  - Race: drop vs late `invocation.archived` (the
    no-downgrade guard, exercised live).
  Other failure modes (workspace corruption, partial WAL,
  etc.) are out of scope for this harness — they belong
  with the steps that introduce them.

## Approach: TDD per step

Same shape as previous plans:

1. Acceptance test (red).
2. Integration tests (red).
3. Unit tests (red).
4. Implement until all three tiers pass.
5. Refactor with all tests green.

For this plan the "acceptance" / "integration" distinction
collapses — every scenario IS an acceptance test, and the
harness itself doesn't have meaningful unit-test surface
(it's wiring). Steps 2-5 are each one acceptance test.

## Implementation Steps

### Step 1 — `TestRuntime` harness in `test_support::runtime`

**Goal.** A struct that boots all the daemon components in
one call and tears them down cleanly. Provides assertion
hooks (store accessors) and action hooks (trigger,
publish-arbitrary-event).

#### API sketch

```rust
let rt = TestRuntime::start().await;

// rt exposes:
//   rt.bus()          -> &EventBus
//   rt.cp_store()     -> &Arc<ControlPlaneStore>
//   rt.worker_store() -> &Arc<WorkerStore>
//   rt.proj_store()   -> &Arc<ProjectionStore>
//   rt.mock()         -> &MockAnthropicServer
//   rt.agent_id()     -> &AgentId    (unique per test)
//   rt.worker_id()    -> &WorkerId   (unique per test)

rt.push_llm_response(MockResponse::text("done", 10, 5));

let inv_id = rt.run_invocation(json!({"input": "go"})).await.expect("complete");

// Wait for the full hand-off:
rt.wait_for_archive(inv_id, Duration::from_secs(10)).await.expect("archived");
rt.wait_for_local_cleanup(inv_id, Duration::from_secs(10)).await.expect("cleaned");

rt.shutdown().await;
```

Components spun up:

- `EventBus::connect(FQ_NATS_URL)`.
- `WorkerStore`, `ControlPlaneStore`, `ProjectionStore` in
  three separate tempdirs (so the WAL files don't collide).
- `MockAnthropicServer`.
- `ProjectionConsumer` (custom durable name).
- A test-variant CoordinationConsumer (the existing
  `run_test_consumer` helper, or a similar function lifted
  out so it can be called from this module).
- `HeartbeatProducer` + `HeartbeatConsumer` (separate test
  durables).
- `ArchiveAckConsumer`.
- `ArchiveRetrySweeper` (with short retry interval for tests).
- A real `GenAiClient::with_base_url(mock.base_url())`.
- `ReducerRunner` wired with the above.

Each long-running task gets a `oneshot::Sender<()>` shutdown
handle; `TestRuntime::shutdown()` fires all of them and
awaits the handles.

#### Done when

- [x] `TestRuntime::start()` and `::shutdown()` work; one
      smoke test that just starts, runs no invocation,
      shuts down cleanly passes.
- [x] All store and mock accessors compile and return the
      expected types.
- [x] The existing
      `completed_invocation_archives_and_worker_cleans_up_against_mock`
      test is rewritten to use `TestRuntime` and stays
      green — proves the harness is a faithful replacement
      for the inline setup.

---

### Step 2 — Scenario: drop ambiguous end-to-end

**Goal.** Realise the step-9 deferred acceptance test
end-to-end through live NATS, the full CP pipeline, and a
real `fq invocation drop` (via `publish_invocation_drop`).

#### Acceptance test

```text
TEST: drop_ambiguous_terminates_invocation_end_to_end

Setup:    TestRuntime started. Seed a triggered event and an
          invocation_state row simulating an in-flight
          invocation. Publish invocation.ambiguous for it.
Action:   Call publish_invocation_drop(invocation_id,
          Some("test drop")).
Assert:   - Within 5s: coordination_invocation_owner.status
            is Failed.
          - invocation_archive row exists for invocation_id.
          - The captured event chain on
            fq.agent.<id>.invocation.operator_recovered
            shows the drop event with reason="test drop".
          - `fq invocation list --status=ambiguous` (via the
            CLI helper) no longer returns it.
```

#### Done when

- [x] Test green against live NATS.
- [x] Step 9 parent plan's status block updated to mark
      this acceptance test as shipped.

---

### Step 3 — Scenario: stale worker detection

**Goal.** A worker that stops emitting heartbeats gets
flipped to `Stale` by the coordination consumer's sweep
within the threshold window.

#### Acceptance test

```text
TEST: stale_worker_marked_stale_within_threshold

Setup:    TestRuntime started with a short stale-threshold
          (e.g. 500ms) and a short sweep interval (e.g.
          100ms). HeartbeatProducer running.
Action:   Register the worker. Stop the heartbeat task.
          Wait 1 second.
Assert:   - coordination_worker row for the worker_id has
            status = Stale.
          - `fq workers list --stale-only` (via the CLI
            helper) returns it.
```

#### Done when

- [x] Test green against live NATS.
- [x] TestRuntime exposes a way to start with overridden
      stale/sweep thresholds (test-only constructor or a
      builder method).

---

### Step 4 — Scenario: retry sweeper recovers from CP outage

**Goal.** With the coordination consumer NOT running, the
worker's retry sweeper republishes `invocation.archived`
until the CP comes back. Once the CP starts consuming, the
archive lands and the worker's local row is cleaned up.

#### Acceptance test

```text
TEST: retry_sweeper_recovers_from_cp_outage

Setup:    TestRuntime started WITHOUT the coordination
          consumer (a builder option,
          `start_without_coordination()`). With a short
          retry interval (e.g. 1s).
Action:   Push a mock LLM response, trigger an invocation,
          drive it to terminal. Worker publishes
          invocation.archived; no consumer picks it up.
Wait:     2 seconds — enough for retry sweeper to fire at
          least twice.
Assert:   - invocation_state row is still present with
            archive_status = "pending".
          - At least 2 invocation.archived events captured
            on fq.agent.<id>.invocation.archived.
Action 2: Start the coordination consumer.
Assert 2: - Within 3 seconds: invocation_archive row exists.
          - invocation_state row deleted.
```

#### Done when

- [x] Test green against live NATS.
- [x] TestRuntime gains a `start_without_coordination()`
      builder variant.
- [x] TestRuntime gains a way to start the coordination
      consumer post-hoc (or a separate
      `start_coordination_consumer()` method).

---

### Step 5 — Scenario: race between operator drop and worker archived

**Goal.** Operator drops an invocation while the worker is
still mid-step. Worker later emits `invocation.archived`
with `final_phase = "completed"`. The no-downgrade guard
keeps the owner status `Failed`. Both events appear in the
audit log.

#### Acceptance test

```text
TEST: drop_then_archived_keeps_owner_failed

Setup:    TestRuntime started.
Action:   Seed in-flight state. Call publish_invocation_drop
          (publishes operator_recovered). Wait briefly for
          CP to mark Failed. Then publish a synthetic
          invocation.archived with final_phase="completed"
          on the agent subject.
Assert:   - coordination_invocation_owner.status remains
            Failed.
          - Both operator_recovered and archived events are
            present in the captured chain.
          - The invocation_archive row's final_phase
            reflects the FIRST writer ("failed") — insert is
            idempotent on invocation_id.
```

#### Done when

- [x] Test green against live NATS.
- [x] Confirms the no-downgrade guard works through the
      live event bus, not just through direct handler calls.

---

### Step 6 — Subprocess smoke test for the binary

**Goal.** Catch the egregious binary-level regressions (CLI
arg parser breakage, missing imports in fq-cli, etc.) that
in-process tests can't see.

#### Test

A single integration test in `fq-cli/tests/` that:

1. Builds the binary via `cargo` (or uses `env!("CARGO_BIN_EXE_fq")`
   so cargo builds it as a test fixture).
2. Runs `fq --help`. Asserts exit 0 and stdout contains the
   `invocation`, `workers`, `status` subcommands.
3. Runs `fq status` against a config pointing at a tempdir
   (no NATS available — should print connection-failure but
   still exit gracefully). Skip this assertion if NATS is
   reachable, since then status would succeed; the point is
   the binary handles the error case sensibly.

#### Done when

- [x] `cargo test -p fq-cli --test smoke` green.
- [x] Test doesn't require NATS to be running (uses a
      bogus URL so connection fails predictably).

---

### Step 7 — Documentation and closing

- [x] Update `services/fq-runtime/README.md` testing table:
      mention the harness module and the acceptance-test
      scenarios it covers. Bump the "Count" column to
      reflect the new tests.
- [x] Move this plan to `docs/plans/closed/`.
- [x] Update parent plan's step-7 / step-8 / step-9 status
      blocks where the corresponding deferred acceptance
      tests are now covered.

## Cross-cutting concerns

- **No regression on existing tests.** Each step's "Done
  when" includes a full `cargo test -p fq-runtime` pass
  against live NATS.
- **Test isolation by uniqueness.** Every long-lived NATS
  resource (durable consumer names, subject filters tied
  to agent/worker ids) gets a unique suffix per test.
- **Test runtime overhead.** Each scenario costs ~1s of
  test time (component startup + scenario actions +
  shutdown). The bulk of `cargo test` time will still be
  in compile + the existing 260 tests.
- **Bugs surfaced by scenarios are fixed in the same step.**
  No `#[ignore]` escape hatch. If a scenario fails because
  the runtime's behaviour doesn't match the spec, the bug
  fix lands before the step is considered done. The whole
  point of building this harness now is to keep technical
  debt from accumulating into later plans.

## Risks and what we'll learn

| Risk | What would tell us | Mitigation |
|---|---|---|
| Pre-existing flake in `consumer_projects_events_into_store` blocks scenarios when run together | The full-suite run fails intermittently | Already documented; we work around it with NATS reset. Real fix is for that test to use `deliver_policy=new` rather than start-from-beginning — flag as a follow-up. |
| TestRuntime's startup latency becomes a tax | Scenarios feel slow; people skip the harness | Measure; if >2s per start, factor out shared setup (e.g. one EventBus per test thread). |
| Step 4's "without coordination" variant needs to expose internals that aren't currently `pub(crate)` | Builder method can't be implemented surgically | Either lift `run_test_consumer` to a `pub(crate)` helper, or add an explicit "don't start coordination" option on the harness builder. |
| Scenarios catch real bugs we then need to fix in scope | Step lands red and the fix balloons | Fix the bug in the same step. No `#[ignore]` escape hatch; the whole reason we're building this harness now is to keep this kind of debt from leaking into later plans. If a bug is genuinely out-of-scope (e.g. broken assumption from a *different* plan we'd need to revise), surface it to the user before deferring. |

## Closing condition

This plan closes when:

- All 7 steps' "Done when" boxes are ticked.
- Parent plan's step-7/8/9 status blocks reflect the
  deferred acceptance tests now landing here.
- This plan moves to `docs/plans/closed/`.
