# Plan: Native Reducer Prototype

**Date**: 2026-04-25
**Status**: Closed (initial slice landed)
**Design references**:
- [`docs/design/wasm-boundary-design.md`](../../design/wasm-boundary-design.md) — the boundary the reducer is built to.
- [`docs/design/2026-04-19-design-assessment.md`](../../design/2026-04-19-design-assessment.md) — recommended building a *native* reducer prototype before WASM packaging.

## Goal

Validate the core architectural claim of the boundary
design — that the agent harness can be expressed as a pure
synchronous `step(StepInput) -> StepOutput` reducer, with the
host driving the loop — by encoding it natively in Rust against
the existing runtime. WASM packaging is deliberately out of
scope at this stage; the architectural claim and the packaging
question are independent and validating them separately keeps
the prototype tractable.

## What the design assessment asked us to test

From [`2026-04-19-design-assessment.md`](../../design/2026-04-19-design-assessment.md):

> Build the reducer prototype before more design.
> Specifically: port `AgentExecutor::run()` to a state-enum
> reducer, behind a Rust trait, in a native crate. **No WASM
> yet.** The reducer claim is architectural; WASM is packaging.

> What the prototype should demonstrate or falsify:
> - Whether the state enum stays small and tractable for
>   realistic agents
> - Whether suspension and resumption actually work end-to-end
> - Whether parallel tool dispatch composes cleanly
> - Whether the resulting code is maintainable (subjective
>   but assessable)

## What landed

A new module `services/fq-runtime/crates/fq-runtime/src/reducer/`
containing:

- `types.rs` — boundary types (`StepInput`, `StepOutput`,
  `NextAction`, `CapabilityResult`, etc.) plus the `Reducer`
  trait. Types are JSON-serialisable so the move to the WASM
  component-model ABI later is structural, not a redesign.
- `harness.rs` — `Harness`, the native `Reducer` implementation
  as an explicit state machine. Synchronous, no async, no I/O.
  State is `{ phase, messages, iteration }` serialised as JSON
  bytes carried in `StepInput::state`.
- `runner.rs` — `ReducerRunner`, the host loop. Translates
  `NextAction`s into LLM/tool calls against the existing
  `LlmClient` and `ToolRegistry`, emits the same canonical
  events as `AgentExecutor`, and returns `InvocationOutcome`.

CLI integration: `fq trigger <agent> --reducer` runs the
selected agent through the reducer path instead of the legacy
in-process executor. The legacy path remains the default.

## Verification against the assessment criteria

### State enum stayed small

The persistent state is three fields (`phase`, `messages`,
`iteration`) with four `Phase` variants
(`Initial`, `AwaitingModel`, `DispatchingTools`, `Done`).
Adding retries, partial dispatch, or skill composition would
add fields and possibly variants, but the current shape is
tractable and gives no early signal of unbounded growth.

**Verdict**: positive datapoint. Caveat: this is exactly the
same logic as `executor::run` reshaped — features the legacy
executor doesn't have either are still untested in this shape.

### Suspension and resumption work

Two tests cover this:

- `harness::tests::state_round_trips_across_drop_and_resume` —
  drops the reducer mid-flight, instantiates a fresh one with
  the persisted state blob, continues to completion. The final
  output is identical.
- `runner::tests::reducer_suspend_resume_yields_same_completion`
  — same shape against the runner-built `AgentConfig`.

The test passes with `cargo test --lib`. No event bus required:
the reducer is fully pure, so suspend/resume is a function of
serde round-tripping.

**Verdict**: positive — at the reducer level this is structural
rather than incidental.

### Parallel tool dispatch composes cleanly

The reducer emits `NextAction::CallToolsParallel(Vec<...>)` when
the model returns >1 tool call in a turn (see
`harness::tests::parallel_tool_calls_dispatch_in_parallel`).

The runner currently dispatches them sequentially in request
order — the protocol contract says the host returns results in
request order, so concurrency is a host implementation detail.
True concurrent dispatch is a one-line refactor (use
`futures::join_all` over the tool calls); marked deferred
because validating the concurrency wasn't the point of this
prototype.

**Verdict**: composes structurally. Concurrency is not yet
exercised but the boundary supports it without changes.

### Code maintainability

Subjective assessment, recorded as written:

- `harness.rs` is ~430 lines including tests. The state
  transitions read straight-line: `initial → request_model
  → consume_response → dispatch_tools → consume_results →
  request_model …`.
- The state-enum approach didn't introduce match-statement
  blowup. Each phase has one entry function (`initial_step`,
  `model_response_step`, `tool_results_step`).
- The `last_result` pattern (host hands the result back via
  `CapabilityResult` rather than the reducer awaiting a future)
  is more verbose than `let response = llm.chat(…).await?` but
  not by much.

**Verdict**: comparable cost to the async version, with a
clean read order. Worth revisiting once skills, retries, and
multi-step reasoning are folded in.

### Behavioural equivalence with the legacy executor

Two tests in `runner::tests` (`equivalent_event_sequence_for_simple_completion`
and `..._for_tool_call_loop`) run the same scripted scenario
through both executors against a real NATS bus and assert the
emitted event sequence is identical (modulo invocation_id /
call_id).

These tests skip silently when `FQ_NATS_URL` is unset, mirroring
the existing `executor::tests` pattern. They have not been
exercised against a live NATS yet — that's a one-step verify
when the user starts the infrastructure.

**Verdict**: written and structurally complete; pending live
bus run.

## What this does not yet validate

Honest enumeration, not a buried disclaimer:

- **No live LLM end-to-end**: the unit tests use the fixture
  client; the equivalence tests need NATS but not a real
  provider. Running `fq trigger sample-agent --reducer` against
  a live Anthropic call will exercise the path end-to-end —
  this hasn't been done yet.
- **No durable state persistence**: the runner holds state in
  a local variable across iterations. Persistence to disk so
  an invocation can survive a host restart is a follow-up.
- **No genuine concurrent dispatch**: parallel tool calls run
  sequentially in the prototype. See above.
- **Cost matching not asserted**: the equivalence tests assert
  event order. Cost numbers should match because the runner
  reuses the legacy code path for pricing, but the assertion
  isn't there yet.
- **Realistic-scale agents**: the legacy executor runs a
  ~5-iteration tool loop and the reducer matches it. Whether
  the state machine stays tractable across multi-step
  reasoning, retries, partial dispatch, etc. is exactly the
  next thing to test as features land — not provable from this
  slice.
- **Performance**: not measured. The reducer is synchronous and
  cheap; the runner's overhead is one extra serde round-trip
  per step. This is in the noise compared to LLM call latency,
  but should be measured before WASM packaging adds ABI
  marshalling cost on top.

## Test results at close

```
cargo test -p fq-runtime --lib reducer::
running 10 tests
test reducer::harness::tests::pure_step_is_deterministic_for_equal_inputs ... ok
test reducer::harness::tests::end_turn_response_completes_invocation ... ok
test reducer::harness::tests::max_iterations_yields_failed ... ok
test reducer::harness::tests::parallel_tool_calls_dispatch_in_parallel ... ok
test reducer::harness::tests::step_zero_seeds_conversation_and_asks_for_model ... ok
test reducer::harness::tests::tool_call_response_dispatches_then_continues ... ok
test reducer::harness::tests::state_round_trips_across_drop_and_resume ... ok
test reducer::runner::tests::equivalent_event_sequence_for_tool_call_loop ... ok
test reducer::runner::tests::equivalent_event_sequence_for_simple_completion ... ok
test reducer::runner::tests::reducer_suspend_resume_yields_same_completion ... ok
```

All 103 lib tests pass. Equivalence tests skip silently without
NATS, same pattern as the legacy executor's tests.

## Recommendation for the next step

The architectural claim survived contact with code. Given that:

1. **Run the equivalence tests against live NATS** before
   anything else. The structural reasoning that they'll pass
   is sound, but a one-off `just infra-up && cargo test`
   verifies the host loop doesn't have a translation bug we
   missed.
2. **Run `fq trigger sample-agent --reducer` against a real
   Anthropic call.** Same cost-of-verification reasoning. This
   is the literal end-to-end the user asked for, gated only on
   infra being up.
3. **Then pick one of three forks**:
   - **Fork A — push features into the reducer.** Add retries,
     dynamic model switching, or skill composition. This is
     where the state-machine ergonomics question gets a real
     answer. Recommended over Fork B/C because the assessment
     called design debt out as the bigger risk than packaging.
   - **Fork B — durable suspension.** Persist state on every
     step, allow host restart mid-invocation. Independently
     useful and cheap given the boundary already supports it.
   - **Fork C — WASM packaging.** Now that the boundary works
     natively, follow the existing
     [`2026-04-19-wasm-harness-prototype.md`](./2026-04-19-wasm-harness-prototype.md).
     The native crate effectively becomes the WASM guest crate
     with `#[no_std]` and component-model bindings on top.

The assessment's strongest concern was that we'd accumulate
design debt without validating the core claim. That risk has
been retired. The remaining concerns (network proxy, workspace
snapshotting, tool catalogue) are still real and still
unaddressed; if Fork A/B/C feels premature, addressing one of
those design gaps next is also defensible.

## Closing condition

This plan closes with the prototype landed and tested. The
WASM-specific
[`2026-04-19-wasm-harness-prototype.md`](./2026-04-19-wasm-harness-prototype.md)
was the natural successor for Fork C above but is now
**deferred (2026-05-22)** — the security rationale that
motivated it was reframed by
[`docs/design/tool-isolation-model.md`](../../design/tool-isolation-model.md),
which puts WASM around individual tools rather than the whole
harness. See that plan's status block for the details.

---

## Post-close addendum (2026-04-25)

Items the original report listed as pending have since been
exercised. Captured here rather than rewriting the snapshot
above, so the original honest enumeration of "what this does
not yet validate" stays intact as a record of the prototype's
state at close.

### Live verification — done

- **Equivalence tests against live NATS.** All 10 reducer
  tests pass with `FQ_NATS_URL=nats://localhost:4222`,
  including the two equivalence tests that were skipping
  before. Reducer path emits the same canonical event
  sequence as the legacy executor.
- **Real Anthropic end-to-end, both paths.** `simple-responder`
  agent: legacy path $0.000091 / 733 ms; reducer path
  $0.000091 / 745 ms. `file-reader` agent (tool-use loop):
  reducer path $0.001959 / 1979 ms with the four expected
  phase transitions logged (`Initial → AwaitingModel →
  DispatchingTools → AwaitingModel → Done`).
- **Cost matching.** Not asserted in tests, but observed
  identical to six decimal places on the simple-completion
  case. Reducer reuses the pricing code path so this is
  structural rather than coincidental.

### First reducer-aware feature shipped: `self_inspect`

A host-fulfilled tool that exposes the runtime's
invocation-scoped state (model, budget, iterations, available
tools) to the agent. Validated three things the prototype
hadn't: (1) the state enum stayed flat when adding a new
feature — no new phases needed; (2) equivalence held under a
new tool because both paths share the synthesis function in
`crate::introspection`; (3) the suspend-resume claim crossed a
tool-dispatch boundary, not just a model-turn boundary
(`reducer_suspends_and_resumes_across_tool_dispatch`).

End-to-end against real Anthropic:
> *"I'm running Claude Haiku 4.5, and I have $0.0489 remaining
> of my $0.05 budget."* — design principle #2 ("no
> confabulation where data exists") working in practice.

### What still hasn't been validated

Items from the original report that remain open:

- Concurrent parallel tool dispatch (still sequential).
- Durable state persistence — now in active design via
  [`docs/design/data-architecture-requirements.md`](../../design/data-architecture-requirements.md).
- Realistic-scale agents (multi-step reasoning, retries,
  skill composition).
- Performance numbers under load.
