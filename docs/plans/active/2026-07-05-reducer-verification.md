# Reducer verification: implementation plan

**Status:** active (2026-07-05) — claims R1–R7 reviewed and approved;
slices 1–5 landed the same day. Both pre-registered findings were
confirmed and fixed before the net (the budget accumulator and the
error-kind semantics), and slice 4's first property sweep caught a
third: resume dropped the trigger, re-seeding resumed conversations
with "(no input)" (fixed via worker-store schema v5). Adopts critique 3 of the
[2026-07-05 project assessment](../../design/2026-07-05-project-assessment.md)
and absorbs the "Round-trip invariants on `HarnessState`" thread of
[backlog § Reducer boundary invariants](../backlog.md).

The **"what"** and **"why"** live elsewhere — the reducer model in
[ADR-0014](../../adrs/accepted/0014-agent-harness-as-reducer.md) and
[wasm-boundary-design](../../design/committed/wasm-boundary-design.md), the
WAL/recovery/archive protocols in
[data-architecture](../../design/committed/data-architecture.md) (§3, §7,
§9.1), and the user-facing contract in the
[reducer harness guide](../../guide/reducer-harness.md). This doc is the
verification net: the claims those documents make, stated as executable
properties, plus the harness that checks them under faults.

**Verification leads** (the M1c/M2 pattern). The **trace oracle** (slice 1)
states the claims as pure predicates over recorded invocation traces and
gates every later slice; properties run on the same engine as the fq-store
conformance suite; the fault work follows fq-store's `tests/sim.rs` shape.
TLA⁺ is *not* planned up front — the subtlety here is crash-point coverage
and replay semantics, which DST covers; if the DST surfaces a genuine
interleaving protocol question (the M1c trigger), the WAL/archive hand-off
is the candidate for a model. Each slice is green on the fq-runtime
`just ci` gate before commit.

## Scope: what this verifies (and what it can't)

This plan verifies the **host machinery**: the `Harness` state machine, the
`ReducerRunner` loop, the `WorkerStore` WAL, recovery categorisation and
`resume()`, budget enforcement, and the canonical event emission. The LLM
stays behind `FixtureClient`/`MockAnthropicServer` — agent *behaviour* is
explicitly not the subject; the machinery treating a hostile-or-arbitrary
model correctly is.

This is the prerequisite for context-window management: compaction is a
mid-flight transformation of the state blob, and R4/R7 below are the spec
any compactor must preserve. Landing the net first turns that feature from
risky surgery into a checked refactor.

## What already exists

The net is not greenfield — the reducer layer is better covered than the
assessment's phrasing implied:

- **Harness unit tests** (`harness.rs`): the full happy path, parallel
  dispatch shape, `max_iterations` literality (`0` = stop signal),
  drop/restore round-trip via `ManualStepper`, determinism of `step`, and a
  `Phase::COUNT` calibration alarm tied to ADR-0014.
- **Doubles**: `FixtureClient` (in-process LLM), `MockAnthropicServer`
  (HTTP-level Anthropic contract), `ManualStepper` (reducer-only stepping),
  `test_support::events` (bus capture/assertion), `TestRuntime` (full
  `fq run` against live NATS — the acceptance tier).
- **Machinery designed for this**: the three-state WAL
  (`intent → dispatched → completed`, with LLM failures recorded
  `completed + is_error` precisely to avoid recovery ambiguity), the *pure*
  `recovery::categorise()` (SafeResume / SafeReplay / Ambiguous), `resume()`
  refusing terminal rows and ambiguous WAL state, and the archive
  pending/ack/sweeper hand-off.

What does **not** exist: any property-based test in fq-runtime, any fault
injection, any oracle over event/WAL traces, and any *hermetic* runner-level
test (the runner tiers above unit tests all need live NATS). Those four
gaps are this plan.

## The claims (the oracle checks these)

Volatile fields (event ids, timestamps, durations, measured costs) are
masked before comparison; "observational trace" below means the sequence of
(event kind, semantic payload projection) for one invocation, plus the
final `InvocationOutcome`.

- **R1 — Canonical sequence.** Every invocation's event chain is
  `triggered`, then per LLM turn `llm.request → llm.dispatched →
  llm.response`, per tool call `tool.call → tool.dispatched → tool.result`
  (results in request order for parallel dispatch), ending in **exactly
  one** of `completed` / `failed`, followed by `invocation.archived`. No
  duplicates, no gaps, no post-terminal emissions (archive republish by the
  sweeper excepted). Under a crash, the observed chain is a prefix of a
  canonical chain.
- **R2 — Write-ahead discipline.** For every side-effecting action: the
  post-step state row is persisted before the action starts; the `intent`
  row lands before dispatch, `dispatched` before the external call is
  issued, `completed` (result or `is_error`) after it returns. Corollary
  checked at every crash point: the WAL never *misdescribes* reality —
  `intent`-only implies the external call was never issued; `completed`
  implies the result is durable.
- **R3 — Recovery soundness.** Over every reachable WAL state:
  `categorise()` = SafeResume ⇒ re-running from the persisted state is
  observationally equivalent to an uninterrupted run (given the
  data-architecture tool-idempotency constraint — the DST's recording tools
  assert at-most-once external dispatch for non-ambiguous recoveries);
  SafeReplay ⇒ feeding the stored result back is equivalent to the
  uninterrupted run; Ambiguous ⇒ never auto-resumed, surfaced, and no
  external action is re-fired. Recovery itself terminates: every
  non-terminal row ends resumed-to-terminal or explicitly ambiguous.
- **R4 — Resume equivalence.** For any scripted run and *any* step
  boundary: suspend + resume (fresh runner, reopened store) yields the same
  observational trace and final outcome as the uninterrupted run. This is
  the guide's "suspension is structural" claim, made executable.
- **R5 — Budget ceiling.** Cumulative invocation cost is checked after
  every LLM call on the single evented path (agent turns, MCP sampling,
  evaluators alike); total spend never exceeds `budget` + the cost of the
  one in-flight call; `BudgetExceeded` is terminal and evented. **The
  accumulator survives crash/resume** — an invocation's lifetime spend, not
  per-attempt spend, is what the ceiling bounds.
- **R6 — Termination.** Every invocation reaches a terminal outcome within
  `HOST_STEP_BUDGET` host steps and `max_iterations` LLM turns (literal
  semantics, `0` = stop) for *every* reducer/LLM behaviour, including
  adversarial scripts (perpetual tool loops, malformed results, empty
  responses). `resume()` of a terminal invocation is refused; recovery
  cannot loop.
- **R7 — State-blob integrity** (absorbs backlog §1.5 thread 2).
  `HarnessState::load ∘ save` is identity; `load` (corrupt/stale persisted
  state) and `save` (reducer bug) both reject semantically invalid states
  via `HarnessState::validate` — the phase ↔ contents invariants written
  out from `initial_step` / `model_response_step` / `tool_results_step`
  (`Initial` ⇒ empty messages; `AwaitingModel` ⇒ non-empty, last message
  System/User/Tool; `DispatchingTools` ⇒ last message Assistant with
  non-empty `tool_calls`; `Done` ⇒ `step` refuses). Violations surface as
  evented `InternalError` failures — never a panic, never a silent wedge.
  Unknown-field tolerance (serde defaults) is pinned so older blobs load —
  the schema-evolution property compaction will later rely on.

## Pre-registered findings

Stated before the net is built, to be confirmed or refuted by it:

1. **R5 failed as written — confirmed and fixed ahead of the net
   (2026-07-05).** `resume()` reconstituted `InvocationTotals::default()` —
   pre-crash spend was forgotten, so the ceiling was per-attempt. Fixed by
   folding totals from the WAL's completed dispatch rows at resume
   (errored dispatches excluded, matching the live path; sub-costs stay
   zero, safe per ADR-0018 §5); pinned by `resume_enforces_lifetime_budget`,
   verified to fail against the unfixed code. Slice 6 still owns the
   property coverage (random pricing scripts, sub-budget semantics, the
   attempt-vs-lifetime duration question).
2. **Outcome/error-kind coarseness — confirmed and fixed ahead of the
   net (2026-07-05).** Worse than filed: three distinct failure paths all
   returned `ExecutorError::MaxIterationsExceeded`, *and* the genuine
   max-iterations case was evented as `runtime_error` (`FailureKind` had
   no max-iterations variant). Fixed by the invariant the review asked
   for: invocation-level failures return
   `ExecutorError::InvocationFailed { kind, message }` carrying exactly
   the `FailureKind` the `failed` event was emitted with, and
   `FailureKind` gained `MaxIterations`. Pinned by two regression tests
   (`..carries_the_max_iterations_kind`, `..carries_the_runtime_error_kind`).
   Consciously untouched: `FailurePhase` has no reducer-step phase (the
   step-error path reports `llm_response`) — a smaller shape question the
   R1 oracle can revisit.
3. **Discovered by slice 4, first property sweep (2026-07-05): resume
   dropped the trigger.** `resume()` passed a null trigger, reasoning
   "the harness only consumes it on step 0, which we've moved past via
   replay" — but replay *starts at* step 0, so every resumed invocation
   re-seeded its conversation with `"(no input)"` instead of the
   original request. The WAL preserved every LLM and tool result while
   losing the invocation's *input*; a resumed agent would continue a
   conversation whose first user message was wrong. The pre-existing
   safe-replay test missed it because it asserted completion, not
   conversation fidelity — observational equivalence caught it at the
   first boundary of the first exhaustive sweep. Fixed by worker-store
   schema **v5**: `invocation_state` persists
   `trigger_source`/`trigger_subject`/`trigger_payload`, the run loop
   writes them, and `resume()` reconstructs the trigger from the row
   (legacy pre-v5 rows warn and degrade to the old behaviour). Pinned
   by the slice-4 suite itself.

## Slices

| # | Slice | Validates | Status |
|---|---|---|---|
| 1 | Trace oracle — claims as pure predicates over recorded (events, WAL, outcome) traces; existing happy-path tests re-driven through it | the net itself, R1/R6 shape | **done** — `test_support::oracle`: the grammar automaton (LLM triples, tool spans with nested sampling, synthetic lone error results, one terminal + archived, envelope chain), 9 hermetic oracle tests, wired into the two canonical runner traces |
| 2 | `HarnessState::validate` + state-blob property tests (round-trip, corruption/invalid-state rejection, unknown-field tolerance) | R7 | **done** — validate at load and save, targeted corruption tests, byte round-trip, unknown-field tolerance, two proptest properties |
| 3 | Hermetic sim harness — scripted LLM + recording tools + in-memory event sink + tempdir `WorkerStore`, seeded scheduler, fault plan | enables 4–7 | **done** — `test_support::sim`: `EventSink` + `Clock` seams through `RunnerConfig` (production defaults unchanged), `SimWorld` with `RecordingSink` publish faults, `ScriptedTool` dispatch recording, `SimClock`; three smoke tests: hermetic oracle-valid run, same-seed byte-identical determinism, crash-at-publish → resume with at-most-once tools. Note: sim lives in-crate (`test_support` is `#[cfg(test)]`), not `tests/sim.rs` as originally named — revisit if test_support ever gets feature-exposed |
| 4 | Resume-equivalence properties — random scripts × every step boundary | R4 | **done** — `observational_trace` masking in the oracle (volatile fields: per-call UUIDs, measured durations, clock stamps); exhaustive fixed-script boundary sweep + 24-case proptest over scripts × boundaries × seeds. Found and fixed finding 3 (resume dropped the trigger) on the first sweep |
| 5 | Crash DST — fault plans over every WAL/publish/dispatch boundary; recovery categorisation soundness; ambiguous handling; archive hand-off under ack loss | R2, R3, R1-under-faults | **done** — `crash_dst` in the sim: exhaustive sweep over every publish index (all five WAL classes asserted covered: nothing-persisted / SafeResume / Ambiguous / SafeReplay / already-terminal), 24-case proptest, crash-while-resuming, sweeper heal + ack-quiescence (via the `EventSink` seam widened to `ArchiveRetrySweeper`), LLM-error canonicality; oracle gained prefix and resume modes. **No new findings** — recovery, refusal, and at-most-once held at every point. Store-fault axis deferred to M3 (see Fault model); consume-side double-ack stays NATS-tier |
| 6 | Budget properties — random pricing scripts, sampling/evaluator origins, crash/resume accumulation (resolves finding 1) | R5 | todo |
| 7 | Soak — long randomised runs with all oracles on; CI-hermetic seeds + a deep local soak recipe | everything, in volume | todo |

Slices 1–2 land immediate value with no new infrastructure (the oracle
runs over what `test_support::events` already captures; `validate` is
specified in the backlog). Slice 3 is the investment the rest rides on.

## Decisions taken up front

- **Placement + naming.** The oracle and sim live in fq-runtime:
  `test_support::oracle` (pure predicates, usable from any tier) and
  `tests/sim.rs` (mirroring fq-store's DST naming so the culture reads
  across services). Hermetic — in the default `just ci` path, not the
  NATS-gated tier.
- **Property engine:** `proptest`, matching the fq-store conformance suite.
- **Determinism seams.** The runner reads wall-clock and entropy through
  free functions (`now_ms()` / `rand_u64()`); slice 3 threads an injectable
  clock + seeded entropy through `RunnerConfig` (production default:
  system), the same injected-clock judgment M2 made for TTL testing. No
  behavioural change outside tests. *(Landed as designed; one scope note:
  `Event::new` still stamps envelope timestamps from the system clock —
  the oracle and equivalence checks treat those as volatile, so full
  envelope-timestamp determinism is deferred until a claim needs it.)*
- **The event-sink seam.** `EventBus` is concrete NATS; hermetic DST needs
  an in-memory sink. Slice 3 extracts the narrowest trait that covers the
  runner's publish path (publish + chained-publish), with `EventBus` as the
  production impl — the same seam-not-abstraction judgment as M2's `bus`
  feature. The NATS-gated tiers keep exercising the real bus.
- **Recording tools.** DST tools are scripted (canned outputs, injectable
  errors/latency) and *record every external dispatch*; the oracle asserts
  at-most-once dispatch outside Ambiguous — making the tool-idempotency
  constraint's load-bearing assumption observable instead of assumed.
- **Crash semantics.** A "crash" in the sim = abort the runner future at a
  fault point, drop it, reopen the `WorkerStore` from disk, run recovery.
  SQLite's own durability is assumed (not our claim to verify); fault
  points sit between our operations, not inside SQLite.
- **Equivalence is observational.** Masked-field trace equality as defined
  under "The claims" — never byte-equality of event JSON, so unrelated
  envelope evolution doesn't false-fail R4.
- **Live LLM stays out of this tier.** `FixtureClient` for the sim,
  `MockAnthropicServer` where HTTP shape matters; the smoke tier remains
  the only live-model surface.

## Fault model (slice 5 axes)

Crossed with random scripts and seeds; every axis point also re-checked by
the oracle post-recovery:

- **Crash points:** before/after the step's `upsert_invocation_state`;
  between `intent` and `dispatched`; between `dispatched` and the external
  call returning; between the call returning and `completed`; before/after
  each event publish (`llm.*`, `tool.*`, terminal, archived); between
  terminal event and archive-pending mark; during recovery itself
  (crash-while-resuming).
- **LLM outcomes:** provider error, timeout, malformed/empty response,
  `is_error` completion, cost spikes that cross the budget mid-run.
- **Tool outcomes:** `is_error` results, `HostError`, `Cancelled`,
  sandbox violations, zero-tool and unknown-tool calls.
- **Archive hand-off:** ack never arrives (sweeper republish), ack after
  crash, double-ack.
- **Store faults:** write failure surfaced at each WAL call site (the
  runner must fail the invocation loudly, not proceed un-journaled).
  *Deferred (2026-07-05, agreed at slice-5 scoping): needs a trait seam
  over the worker store — rides with the storage plan's M3 trait
  boundaries instead of hand-rolled test scaffolding; captured as an M3
  scope addendum there. The weaker no-silent-swallow claim holds
  structurally today (every WAL call site `?`-propagates).*

## Out of scope

- **Tool-parameter schema validation** — backlog §1.5 thread 3 stays
  deferred; its re-trigger conditions are unchanged by this plan.
- **Concurrent parallel dispatch** — the runner dispatches sequentially
  today (documented gap). The oracle pins "results in request order"; when
  the `join_all` refactor lands, R4-style equivalence (sequential ≡
  concurrent) becomes one new property, not a new plan.
- **Compaction / context-window management** — this plan is its
  prerequisite, not its implementation. Forward contract: a compactor must
  preserve R4 (equivalence from the compacted state) and R7 (validity).
- **MCP protocol conformance** — owned by the MCP test suite; the sim
  covers sampling/evaluator calls only as budget-bearing LLM origins (R5).
- **Graph-executor contracts** — per the backlog, system-wide validation
  waits for that work; nothing here blocks on it.

## Closing condition

Every claim R1–R7 has an oracle exercising it in the hermetic `just ci`
path (fixed seed set for CI; a `just soak`-style recipe for deep local
runs); the crash DST covers every fault-model axis; finding 1 is resolved
with a pinned regression test and finding 2 is filed with a decision;
backlog §1.5 thread 2 is closed into slice 2; the reducer-harness guide's
suspend/resume section links the equivalence property as its evidence.

## Sequencing note

fq-runtime-side only — proceeds in parallel with the storage plan's M3+
(different crates, no file overlap). It should land **before** Memory
integration and context-window management put new weight on the harness;
slices 1–2 are immediately valuable even if later slices pause for M3.

## References

- [2026-07-05 project assessment](../../design/2026-07-05-project-assessment.md)
  (critique 3) — why this, why now
- [Backlog § Reducer boundary invariants](../backlog.md) — thread 2 absorbed
  here; threads 1 and 3 unchanged
- [ADR-0014 — agent harness as reducer](../../adrs/accepted/0014-agent-harness-as-reducer.md)
  · [wasm-boundary-design](../../design/committed/wasm-boundary-design.md)
- [data-architecture](../../design/committed/data-architecture.md) §3
  (worker store), §7 (recovery), §9.1 (three-state WAL)
- [Reducer harness guide](../../guide/reducer-harness.md) — the contract
  being verified
- Pattern precedent: [M1c GC plan](../closed/2026-06-30-m1c-gc-implementation.md)
  · [M2 access-control plan](../closed/2026-07-03-m2-access-control.md)
  · fq-store `tests/sim.rs` / conformance suite
