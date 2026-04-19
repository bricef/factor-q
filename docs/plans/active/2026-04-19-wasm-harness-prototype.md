# Plan: WASM Harness Prototype

**Date**: 2026-04-19
**Status**: Active
**Design reference**: [`docs/design/wasm-boundary-design.md`](../../design/wasm-boundary-design.md)

## Goal

Prove that the agent harness can compile to a WASM component
targeting the `factorq:agent-harness@0.1.0` interface, that it
runs against a host-implemented capability set with behavioural
equivalence to the current native executor, and that the
debugging and observability stories are tractable enough to
develop against in practice.

This is a feasibility test, not a production rollout. The
deliverable is a working prototype that informs the decision
about whether to promote WASM to the default execution path.

## Scope

In scope:
- Author the WIT package for `factorq:agent-harness@0.1.0`
  capturing the `step(StepInput) -> StepOutput` guest export
  and the supporting types (`NextAction`, `CapabilityResult`,
  `ModelRequest`, `ModelResponse`, etc.) from the boundary
  design doc.
- Re-implement the logic of `AgentExecutor::run()` as a
  reducer state machine in a new guest crate. State enum
  covers the loop phases (initial → awaiting model → dispatching
  tools → complete/failed). Guest code is synchronous Rust —
  no tokio, no async, no futures in the guest.
- Implement the host-side step loop: call `step`, execute the
  returned `NextAction` (using existing `LlmClient` for model
  calls and `ToolRegistry` for tool calls), feed the result
  back, repeat.
- Implement host-side state persistence (in-memory for
  prototype, sufficient for verifying the interface works).
  Durable persistence is out of scope.
- Embed `wasmtime` in `fq-runtime` and wire the executor path
  to instantiate the guest component and drive the step loop.
- Run the existing phase-1 example agents end-to-end through
  the WASM path.
- Compare behaviour against the native executor.

Out of scope:
- Durable state persistence across host restarts. Suspension
  and migration work structurally in the prototype (the host
  can suspend between steps in-process) but the full
  persistence story is a follow-up.
- Streaming capabilities.
- Production switchover — the native executor remains the
  default after this prototype.
- Multi-node capability routing.
- JSON schema validation on tool arguments (future
  enhancement).
- Guest-side ergonomic SDK for state-machine authoring
  (decide after the prototype shows how the raw version
  feels).

## Verification criteria

The prototype succeeds if all of these hold. Each criterion is
a concrete check, not a soft goal.

### Compilation

- [ ] The guest crate compiles to the component-model target
  with no patched dependencies.
- [ ] Required guest crates (serde, serde_json, and whatever
  else the state-machine logic needs) work unmodified. Because
  the guest is synchronous, the long tail of async-runtime
  compatibility questions does not apply.
- [ ] Incremental rebuild time is under 10 seconds — slower
  than that undermines iterative development and is itself a
  blocker.

### Behavioural equivalence

- [ ] Each phase-1 example agent (`hello-world`,
  `code-reviewer`, `mcp-echo`) produces equivalent outputs
  when run through the WASM path vs the native path.
- [ ] Events emitted during execution have matching structure
  and content (modulo a harness-version metadata field).
- [ ] Cost tracking via the pricing table produces matching
  totals within rounding.
- [ ] Error paths (budget exceeded, tool failure, LLM error)
  produce matching terminal events.

### Reducer-model properties

- [ ] Given the same sequence of `StepInput`s, the guest
  produces identical `StepOutput`s across runs. Purity is
  structurally guaranteed by the boundary but should be
  empirically verified.
- [ ] The host can suspend an in-flight invocation at a step
  boundary by persisting `state` + `last-result`, tear down
  the guest instance, instantiate a fresh one, and continue
  from the suspended point with no observable difference in
  the final output.
- [ ] Parallel tool dispatch via `CallToolsParallel` actually
  runs tools concurrently on the host and returns results in
  request order.

### Debugging (the open question from the boundary design)

Evaluation criteria agreed in advance — these turn "is it
debuggable?" from a vibe into a checklist:

- [ ] A panic or trap in the guest produces a trap message
  that identifies the component function where it occurred,
  not just an opcode offset.
- [ ] `tracing` spans opened in the host around capability
  dispatch appear in log output alongside guest-emitted `log`
  capability calls, interleaved correctly by time.
- [ ] `println!` in the guest (or equivalent stdio redirection)
  reaches the operator during development.
- [ ] A representative planted bug (e.g., an off-by-one in
  tool-call dispatch) can be diagnosed from the event log and
  logs within order-of-minutes, not hours. Qualitative check,
  recorded as pass/fail with notes.
- [ ] If DWARF-in-WASM yields useful symbolic stack traces in
  `wasmtime`, record as a win. If not, document the fallback
  strategy (log-based diagnosis; reproducer in a native build
  for gdb/lldb).

If these criteria aren't met, the failure modes and severity
are recorded. The prototype is still useful as a feasibility
data point even when some criteria fail — the point is to
know.

### Performance sanity (measured, not gated)

Not pass/fail. Numbers inform lifecycle model decisions:

- `wasmtime` instance creation per invocation. Acceptable:
  under 10ms. Concerning: over 100ms.
- Per-`step` call overhead (ABI marshalling + function
  dispatch). Typical benchmarks suggest microseconds; confirm.
  A typical agent loop has ~5–15 step calls, so total
  overhead is single-digit milliseconds against LLM calls
  that take seconds.
- Memory footprint of a resident guest instance. Target under
  10MB; concerning over 50MB.
- Guest-state serialization size across typical invocations.
  Should be KB, not MB. Larger values suggest the state enum
  is holding too much.

Numbers in the "concerning" zone inform whether
instance-per-invocation or instance-pooling is the right
lifecycle model.

## Risks

- **Component model tooling churn**: the spec is stable-ish but
  tooling is actively evolving. Pin versions; re-evaluate if
  churn imposes significant maintenance cost.
- **State enum ergonomics**: writing the harness as an explicit
  state machine may turn out to be notably painful at this
  scale. Risk is mitigated by the fact that the state space
  is small (~5 variants) and the transitions are simple; but
  the prototype should verify by actually writing it.
- **Debugging story worse than expected**: possible. The
  verification criteria force us to confront this empirically.

The previously-flagged `tokio-on-WASI` risk is eliminated by
the reducer model — the guest is synchronous and doesn't need
an async runtime at all.

## Structure

New crates:
- `crates/fq-harness-wit/` — the WIT package, versioned as a
  standalone component interface.
- `crates/fq-harness-guest/` — the guest harness implementation,
  compiled to a WASM component.
- `crates/fq-harness-host/` — host-side capability
  implementations, bridged to existing `fq-runtime` types.

Existing crate changes:
- `fq-runtime`: add feature flag `wasm-executor` selecting the
  WASM path. Default remains the native executor.
- `fq-cli`: add `--wasm` flag to `fq trigger` for invoking the
  WASM path explicitly during development.

## Deliverables

1. WIT package checked in as
   `crates/fq-harness-wit/wit/world.wit`.
2. Guest crate compiling to a `.wasm` component, embedded in
   the host binary at build time.
3. Host-side step loop driver, action executors bridging to
   `LlmClient` / `ToolRegistry`, and in-memory state store.
4. Integration tests: each phase-1 example agent runs
   successfully through the WASM path.
5. A short report appended to this plan on close documenting:
   - Which verification criteria were met.
   - Actual performance numbers.
   - Debugging tractability assessment with notes.
   - State-machine ergonomics assessment: is a guest SDK
     worth building, or is raw state-enum code acceptable?
   - Recommendation: promote to default, iterate further, or
     abandon.

## Closing condition

Plan closes when all in-scope items are complete and the
report is written. The report feeds into the decision about
whether to make WASM the default execution path.
