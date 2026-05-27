# Plan: Deprecate the legacy `AgentExecutor`

**Date**: 2026-05-27
**Status**: Active
**Design references**:
- [`docs/design/wasm-boundary-design.md`](../../design/wasm-boundary-design.md) — reducer model the runner implements.
- [`docs/plans/closed/2026-04-25-native-reducer-prototype.md`](../closed/2026-04-25-native-reducer-prototype.md) — native reducer that made `AgentExecutor` redundant.

## Goal

Make `ReducerRunner` the only invocation path. Delete
`AgentExecutor` and its 17 tests. Reshape the `Worker` trait
to fit the reducer model naturally rather than the legacy
executor's signature. Close one gap surfaced during scoping
(no reducer-path gating test).

After this lands:

- New triggers via `fq run` go through the WAL / archive /
  coordination wiring (today they go through the legacy
  executor and bypass most data-arch-v1 features).
- `fq trigger` has one path, not two.
- One worker implementation, one set of tests, one error
  type, one set of imports.

## Context

Two facts surfaced while scoping this work:

1. **The legacy executor is the daemon's default trigger
   path.** `fq-cli/main.rs:1300` constructs an
   `AgentExecutor` and hands it to `TriggerDispatcher` as
   `Arc<dyn Worker>`. New invocations from a running `fq run`
   therefore *don't* exercise the reducer pipeline — they
   skip the WAL, the archive hand-off, the operator-visible
   ownership rows, and the recovery semantics that
   data-architecture-v1 just shipped. The ReducerRunner is
   only used by:
   - `fq trigger --reducer <agent>` (opt-in CLI flag)
   - the daemon's *resume* path (after restart)
   - the acceptance harness (all NATS-gated tests)
   Deprecating legacy completes step 8/9/10 for the daemon's
   normal trigger path — it's not just cleanup.
2. **The reducer-path gating check (`runner.rs:587`) has no
   dedicated test.** The legacy executor's
   `tool_not_in_agent_allowlist_is_denied` covers
   `executor.rs`. The reducer side was tested indirectly by
   `reducer_invocation_emits_single_parent_chain` until
   commit `c9fd92e` added `.tools(["file_read"])` to that
   test, removing the only path that exercised the
   synthetic-error gating arm on the reducer.

## Decisions taken on 2026-05-22

- **Keep the `Worker` trait, reshape it.** `Worker` continues
  to be the dispatcher's `Arc<dyn Worker>` contract, but its
  `run` signature is rewritten to match the reducer model
  rather than the legacy executor. Specifically: the
  reducer becomes a field of the runner (today it's a
  parameter), so `Worker::run` doesn't have to leak the
  reducer-generic through the trait.
- **Convert the equivalence tests, don't delete them.**
  `equivalent_event_sequence_for_{simple_completion,tool_call_loop}`
  assert the *exact canonical event sequence* a reducer
  invocation produces. That assertion has value as living
  documentation of the wire contract; only the
  legacy-comparison plumbing is dead weight. Drop the
  `run_through_legacy_and_reducer` helper, keep the literal
  expected sequences, rename the tests to
  `reducer_emits_canonical_event_sequence_for_*`.
- **`--reducer` flag is removed entirely.** `fq trigger`
  always uses ReducerRunner. The flag has been the right
  default since data-arch-v1 closed; the alternative path is
  going away.
- **Test budget: any behaviour the reducer doesn't already
  cover gets a new test before the legacy code goes.** If the
  scoping audit finds a behaviour only the legacy executor's
  tests prove (budget enforcement, max-iteration exit,
  specific cost-tracking edge case), that test gets a
  reducer-path equivalent before we delete the legacy one.
  No silent coverage loss.
- **No backward-compatibility shim.** No deprecation warning
  on the `--reducer` flag, no `AgentExecutor` left behind a
  feature gate. Hard cut.

## Implementation Steps

### Step 1 — Reducer-path gating test

**Goal.** Close the test gap surfaced during scoping. The
test goes in before any deletion so we can prove the gating
code stays correct through the reshape that follows.

#### Test

`worker::reducer::runner::tests::tool_not_in_agent_allowlist_is_denied_on_reducer_path`
— mirrors the legacy executor's test:

- Build an agent with `.tools(["file_read"])` only.
- Push a `FixtureClient` response that requests
  `file_write`.
- Run through `ReducerRunner`.
- Assert the captured `ToolResult` has `is_error: true` and
  `error_kind: Some(ToolErrorKind::PermissionDenied)`.
- Assert no `tool_dispatch` row was written to the worker
  store (the synthetic-error path bypasses the WAL).

NATS-gated.

#### Done when

- [ ] Test green against live NATS.
- [ ] No other reducer-path tests regress.

---

### Step 2 — Reshape `Worker` trait around the reducer

**Goal.** Move the reducer from `ReducerRunner::run`'s
parameter list onto a field of the runner so the `Worker`
trait doesn't have to expose the `R: Reducer` generic. After
this step, `ReducerRunner` implements `Worker` and the
dispatcher can hold it via `Arc<dyn Worker>`.

#### Shape

```rust
pub struct ReducerRunner<R: Reducer + Send + Sync> {
    bus: EventBus,
    pricing: Arc<PricingTable>,
    tools: Arc<ToolRegistry>,
    store: Arc<WorkerStore>,
    worker_id: WorkerId,
    reducer: R,     // NEW
}

#[async_trait]
pub trait Worker: Send + Sync {
    async fn run(
        &self,
        agent: &Agent,
        llm: &dyn LlmClient,
        trigger_source: TriggerSource,
        trigger_subject: Option<String>,
        trigger_payload: Value,
    ) -> Result<InvocationOutcome, ExecutorError>;
}

impl<R: Reducer + Send + Sync> Worker for ReducerRunner<R> { ... }
```

The reducer becomes a constructor argument:
`ReducerRunner::new(bus, pricing, tools, store, worker_id, harness)`.
Production code passes `Harness::new()`; tests can pass a
stub reducer when they need to.

#### Done when

- [ ] `ReducerRunner` holds its reducer as a field.
- [ ] `Worker` trait reshape compiles; `ReducerRunner`
      implements it.
- [ ] All existing call sites that construct a runner pass
      `Harness::new()` (no behaviour change).
- [ ] Full lib suite green.

---

### Step 3 — Migrate `TriggerDispatcher` to `ReducerRunner`

**Goal.** Daemon's normal trigger path goes through the
reducer. New invocations from `fq run` start exercising the
WAL, archive, and coordination wiring that data-arch v1
already built.

`fq-cli/main.rs:1300` currently:

```rust
let worker: Arc<dyn fq_runtime::Worker> = Arc::new(AgentExecutor::new(
    bus.clone(),
    pricing.clone(),
    tools.clone(),
));
```

Becomes (using the reshaped trait from step 2):

```rust
let worker: Arc<dyn fq_runtime::Worker> = Arc::new(ReducerRunner::new(
    bus.clone(),
    pricing.clone(),
    tools.clone(),
    worker_store.clone(),
    worker_id.clone(),
    Harness::new(),
));
```

The existing `resume_runner` becomes the same `worker` value
— no need for two runners.

#### Tests to update

- `control_plane::dispatcher::tests` uses `AgentExecutor`
  as a `Worker` stand-in. Switch to `ReducerRunner` (or a
  test stub).

#### Done when

- [ ] Daemon's `fq run` constructs one `ReducerRunner` and
      hands it to the dispatcher.
- [ ] Dispatcher tests green using the new shape.
- [ ] Full lib suite green against live NATS.
- [ ] Manual smoke: `fq run`, then `fq trigger` in another
      window, verify an `invocation.archived` event lands in
      the archive table afterwards (it wouldn't have before
      this step).

---

### Step 4 — `fq trigger` always uses reducer; remove `--reducer` flag entirely

**Goal.** Single user-facing trigger path.

- Remove the `reducer: bool` field from the CLI's `Trigger`
  command struct.
- Delete the `else` branch in `trigger_agent` that
  constructs `AgentExecutor`.
- The function becomes ~20 lines shorter.

`fq trigger --reducer foo` will fail with a clap unknown-arg
error after this — no shim, no warning, no no-op grace. The
flag stops existing.

#### Done when

- [ ] `fq trigger` always uses `ReducerRunner`.
- [ ] `fq trigger --reducer` fails as an unknown argument.
- [ ] `fq trigger --help` no longer mentions `--reducer`.

---

### Step 5 — Coverage audit for legacy-only behaviours

**Goal.** Before deleting `executor.rs`, walk every test in
its `mod tests` block and confirm an equivalent reducer-path
test exists. If not, write one.

#### Candidate gaps to look for

- Budget exceeded (`InvocationOutcome::BudgetExceeded`) —
  the reducer has the same enum; need a test that proves
  the reducer path *emits* the budget-exceeded outcome
  correctly.
- Max iterations exceeded — same story.
- Cost-tracking edge cases (e.g., zero-cost responses,
  cache reads, multi-turn cost aggregation).
- Synthetic-tool-error paths the legacy executor proves:
  unknown tool name (registry miss), tool-execution failure,
  agent without the tool declared (step 1's new test).
- The legacy `tool_not_in_agent_allowlist_is_denied` —
  superseded by step 1's reducer test.

#### Output

A short audit note in the closing commit listing each
legacy test and either:
- "covered by reducer test X", or
- "added new reducer test Y in this step".

#### Done when

- [ ] Audit complete; every legacy test maps to either a
      reducer equivalent or a new reducer test in this
      step.
- [ ] All new reducer tests green.

---

### Step 6 — Convert the equivalence tests

**Goal.** Salvage the canonical-sequence assertions from the
two `equivalent_event_sequence_for_*` tests; drop the legacy
half.

- Delete `run_through_legacy_and_reducer` and
  `strip_wal_dispatched`.
- Rename to `reducer_emits_canonical_event_sequence_for_simple_completion`
  and `_for_tool_call_loop`.
- Each test now drives one invocation through `ReducerRunner`
  and asserts against the literal expected sequence
  (`["triggered", "llm_request", "llm_dispatched", "llm_response", "completed"]`,
  etc.).

#### Done when

- [ ] Two converted tests green.
- [ ] `AgentExecutor` is no longer imported by any test.

---

### Step 7 — Delete `worker/executor.rs` and clean up exports

**Goal.** The legacy executor and everything that exports it
goes.

- Delete `services/fq-runtime/crates/fq-runtime/src/worker/executor.rs`.
- Remove `pub use executor::{AgentExecutor, ...}` from
  `worker/mod.rs`. The `Worker` impl in `mod.rs:91` goes
  with it (or moves to `runner.rs` as part of step 2's
  reshape).
- Remove `AgentExecutor` from `lib.rs`'s re-exports.
- Remove the `executor` module declaration entirely if
  nothing remains in it.

The shared error and outcome types (`ExecutorError`,
`InvocationOutcome`) **stay** but move out of
`executor.rs`. Natural new home: a small `worker/outcome.rs`
or `worker/runner_types.rs` module. `ExecutorError` is also
used by the reducer path and renaming it to `RunnerError`
is bikeshedding — leave the name alone for now, rename in a
follow-up if it bugs anyone.

#### Done when

- [ ] `executor.rs` gone.
- [ ] No references to `AgentExecutor` anywhere in the
      workspace.
- [ ] `cargo check --workspace --all-targets` clean.

---

### Step 8 — Documentation, close

**Goal.** Update docs that referenced the legacy executor;
move the plan to closed/.

- `docs/guide/reducer-harness.md` mentions "legacy executor"
  in places — drop or rephrase past-tense.
- `docs/design/wasm-boundary-design.md` references
  `AgentExecutor::run` as the behavioural baseline; update
  to `ReducerRunner::run`.
- Any closed plan that referred to the legacy executor as
  "still active" gets a small note that it's gone.
- Move this plan to `docs/plans/closed/`.

## Cross-cutting concerns

- **No regression on existing tests.** Each step's
  "Done when" includes a full `cargo test -p fq-runtime`
  pass against live NATS.
- **Bugs surfaced get fixed in the same step.** Same
  discipline as the acceptance harness. If step 3's
  daemon-now-uses-reducer change exposes a real bug, fix
  it before moving on.
- **Documentation lands with the code.** Step 7's exports
  cleanup includes the doc-comment audit in the same
  commit; step 8 covers the design-doc updates.

## Risks and what we'll learn

| Risk | What would tell us | Mitigation |
|---|---|---|
| The daemon's regular trigger path has never been load-tested through the reducer; some bug hides | Step 3's manual smoke test fails, or one of the acceptance harness scenarios starts behaving differently when invoked via dispatcher | Fix the bug in step 3. The harness already exercises the key paths in isolation; integrating them through the dispatcher should be additive. |
| Some behaviour only the legacy executor proves goes uncaught | Step 5's audit identifies it; or a future bug report does | Step 5 is the explicit guard. If we miss something, the bug is fixable; we don't lose data. |
| Test names referencing the legacy path proliferate before the deletion | Lots of tests with "executor" in the name still exist after step 7 | Step 5 covers the audit; step 7's "no references" check is the gate. |
| Converting equivalence tests masks a real divergence | The reducer's sequence drifts and we don't notice because we've baked the current sequence into the assertion | Acceptable risk for documentation tests. The acceptance harness covers the behaviours that matter. |

## Closing condition

This plan closes when:

- All 8 steps' "Done when" boxes are ticked.
- `AgentExecutor` is gone from the workspace; `cargo check
  --workspace --all-targets` clean; full lib suite green
  against live NATS; lints + fmt clean.
- This plan moves to `docs/plans/closed/`.
- A one-line note in the parent data-arch-v1 plan's closing
  summary acknowledges that step 3 of this plan (daemon's
  trigger path migrates to reducer) closes a latent gap in
  what was previously called "complete."
