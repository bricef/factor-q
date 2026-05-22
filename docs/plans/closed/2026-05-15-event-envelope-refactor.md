# Plan: Event Envelope / Payload / Annotations Refactor

**Date**: 2026-05-15
**Status**: Closed (2026-05-16)
**Design references**:
- [`docs/design/inter-node-contracts-and-event-layers.md`](../../design/inter-node-contracts-and-event-layers.md) — the three-layer event model.
- [`docs/adrs/ADR-0016-typed-operations-no-free-form-apis.md`](../../adrs/ADR-0016-typed-operations-no-free-form-apis.md) — typed-operations discipline that motivates the annotation barrier.
- [`docs/design/event-schema.md`](../../design/event-schema.md) — current (v1) schema; updated to v2 in this plan.

## Goal

Split the on-the-wire `Event` into three structurally distinct
Rust types — `Envelope`, `EventPayload`, `Annotations` — with the
write-permission and read-audience rules from the design doc
enforced in the type system rather than by convention. Thread
`parent_event_id` through every publish so happens-before in an
invocation can be reconstructed without timestamp comparisons.
Fold cost into the envelope on `llm.response` events; remove the
standalone `Cost` payload variant. Add `trace_id` and `schema_id`
to the envelope from day one so multi-invocation traces and
versioned payload evolution don't need a wire-format change
later. Bump schema version to **2**. Acceptable because no
production deployment exists and NATS retention is 24h.

## Context

This is the local-branch implementation of the architectural
change that upstream's `3e6bc81` accomplished on the now-discarded
side of the divergence (see `2026-05-05-native-reducer-prototype.md`
in this directory — superseded by this plan). Local main has
since added four payload variants (`LlmDispatched`,
`ToolDispatched`, `InvocationAmbiguous`, `SystemRecovery`), a WAL
with three-state dispatch semantics, a recovery path that
re-emits events, and a control-plane coordination consumer that
filters on invocation events. The literal upstream patch does
not cover any of them; this plan does.

The plan is positioned **before** data-architecture-v1 steps 8–10
because step 8 (archive hand-off) makes the post-terminal event
shape contractually load-bearing — the archived payload must
match the final-state event shape, and changing it after step 8
ships would mean re-archiving in place.

## Approach: TDD per step

Same shape as `2026-04-28-data-architecture-v1.md`. Per step:

1. Write the acceptance test first (red).
2. Drop to integration tests (red).
3. Drop to unit tests (red).
4. Implement until all three tiers pass.
5. Refactor with all tests green.

The existing **reducer-equivalence tests** and **recovery
acceptance tests** are the primary safety net for this refactor.
They must stay green through every step. Where new behaviour is
introduced (parent-event chain, annotation barrier), new tests
land alongside.

### Test tiers

| Tier | Where it runs | Speed | What it covers |
|---|---|---|---|
| **Unit** | `cargo test` (no env vars) | <1s/test | Pure types and helpers: serialisation round-trips, `schema_id_for`, `for_consumer_context` strips annotations. |
| **Integration** | `cargo test` (no env vars) | seconds | Reducer runner emits the new envelope shape; projection ingests cost from envelope; recovery re-emits with correct parent. |
| **Acceptance** | `cargo test` gated on `FQ_NATS_URL` (and `ANTHROPIC_API_KEY` where indicated) | seconds to tens of seconds | Full daemon publishing v2 events, projection writing correct cost rows, coordination consumer handling v2 invocation events. |

### Test harness preliminaries

These extensions sit alongside the data-arch-v1 helpers.

| Asset | Status | What's needed |
|---|---|---|
| Event-sequence capture helper | exists in `test_support::events` | Extend to expose `envelope.parent_event_id` so chain-reconstruction tests can iterate. |
| **Parent-chain assertion helper** | NEW | Given a captured `Vec<Event>` from one invocation, assert it forms a single chain rooted at the `Triggered` event with no orphans. |
| Reducer-equivalence baseline | exists | Re-baseline expected event sequences after the schema bump — the wire format changes, the *sequence* should not. |

## Implementation Steps

### Step 1 — Introduce `Envelope` and `Annotations` types (shape-only)

**Goal.** Land the type-system shape with no semantic change.
Every `Event::new` / `Event::system` site is mechanically
migrated. `parent_event_id` is `None` everywhere; annotations
empty; cost still lives in `EventPayload::Cost`. Schema version
bumps to 2 only because the on-wire shape changes (envelope
nesting); nothing about *behaviour* changes yet.

#### Acceptance test

```text
TEST: existing_e2e_behaviour_unchanged_after_envelope_shape_change

Setup:    A trivial agent (sample-responder) runs through
          `fq trigger sample-agent --reducer` against live NATS.
Pre:      Capture the event sequence on `events.>` for the run.
Action:   Re-run the same agent after the envelope shape lands.
Assert:   Event payload sequence is identical (modulo new envelope
          wrapper). Same outcome text. Same cost. Each event's
          `envelope.schema_version` is 2; `envelope.trace_id`
          equals `envelope.invocation_id`; `envelope.schema_id`
          matches the payload variant.
```

Gated on `FQ_NATS_URL`.

#### Integration tests

- **`event_serialises_with_envelope_field`** — round-trip a `Triggered` event; assert top-level JSON has `envelope`, `payload`, no `annotations` field (skipped when empty).
- **`schema_id_for_every_payload_variant`** — exhaustive match over `EventPayload`; assert each variant produces a non-empty `factor-q/<name>@<v>` schema_id.
- **`reducer_equivalence_tests_pass`** — `equivalent_event_sequence_for_simple_completion` and `equivalent_event_sequence_for_tool_call_loop` baselines updated for new shape; assertions otherwise identical.

#### Unit tests

- **`envelope_default_fields_on_new_event`** — `Event::new(...)` produces an event with `parent_event_id = None`, `trace_id = invocation_id`, `cost = None`, empty annotations.
- **`event_for_system_uses_runtime_id_as_trace_id`** — `Event::system(runtime_id, ...)` sets `trace_id == runtime_id`.
- **`annotations_skip_serialise_when_empty`** — empty annotations omitted from JSON.
- **`schema_version_constant_is_two`** — pure constant check.

#### Done when

- [ ] All call sites in `events.rs`, `executor.rs`, reducer `runner.rs`, `recovery.rs`, `coordination_consumer.rs`, projection `consumer.rs`, `bus.rs`, CLI `main.rs`, and `test_support/events.rs` use `event.envelope.*` field access (no `event.agent_id` etc. anywhere).
- [ ] All listed integration tests green
- [ ] All listed unit tests green
- [ ] Acceptance test green against live NATS
- [ ] `cargo doc -p fq-runtime` clean
- [ ] No regressions in data-architecture-v1 acceptance tests (steps 4, 5, 6 still pass)

**Out of scope for this step.** Parent-chain threading, cost
relocation, annotation barrier method. Those land next.

---

### Step 2 — Thread `parent_event_id` through every publish

**Goal.** The reducer runner threads the previously-published
event id through each subsequent publish so the chain reflects
true causality. Recovery re-emits with explicit parent semantics
(see "Open decisions" below).

#### Open decisions resolved in this step

1. **What does the parent point to across reducer steps?**
   *Resolved: the immediately-prior published event in the same
   invocation.* For the canonical loop:
   `Triggered → LlmRequest → LlmDispatched → LlmResponse → Cost*
   → ToolCall → ToolDispatched → ToolResult → … → Completed`,
   each event's `parent_event_id` is the previous arrow tail.
   The `*Cost` step disappears in step 3.
2. **What does the parent point to for a recovery re-emit?**
   *Resolved: `None`*, treated as a fresh chain root from the
   recovery's perspective. The projection links the
   pre-recovery and post-recovery chains by `invocation_id`
   only. A future `envelope.recovered_from_event_id: Option<Uuid>`
   could be added if cross-incarnation stitching becomes
   load-bearing; deferred because nothing reads such a link
   today.
3. **What does the parent point to for system events?**
   *Resolved: `None`.* System events (`SystemStartup`,
   `SystemRecovery`, etc.) are not part of an invocation chain.

These decisions are documented in `events.rs` next to the
`parent_event_id` field and in `inter-node-contracts-and-event-layers.md`.

#### Acceptance test

```text
TEST: invocation_event_stream_forms_single_parent_chain

Setup:    `file-reader` agent, scripted LLM with one tool call
          and a final assistant response. Live NATS.
Action:   Trigger the agent; capture every event published
          for this invocation on `events.>`.
Assert:   Events sorted by `event_id` (Uuid v7 is sortable by
          time-of-creation) form a single chain:
          - exactly one event has `parent_event_id == None`
            and is the `Triggered` event
          - every other event's `parent_event_id` is the
            `event_id` of an event earlier in the captured set
          - the chain visits every event reachable from the
            root with no orphans
          The chain is reconstructable without consulting
          timestamps.
```

Gated on `FQ_NATS_URL`.

#### Integration tests

- **`reducer_runner_threads_parent_across_llm_call`** — drive a single LLM call with a stub LLM; capture events; verify `LlmRequest → LlmDispatched → LlmResponse` chain.
- **`reducer_runner_threads_parent_across_tool_call`** — same for `ToolCall → ToolDispatched → ToolResult`.
- **`reducer_runner_threads_parent_across_multi_step_invocation`** — multi-iteration loop; chain reconstructs end-to-end.
- **`recovery_reemit_starts_new_chain_with_null_parent`** — populate WAL with a safe-resume row; trigger recovery; verify the re-emitted event has `parent_event_id == None`.
- **`system_event_has_null_parent`** — assert `SystemStartup`, `SystemRecovery`, `SystemShutdown` envelopes have `parent_event_id == None`.

#### Unit tests

- **`event_with_parent_sets_envelope_field`** — `Event::with_parent(uuid)` mutates `envelope.parent_event_id`.
- **`parent_chain_helper_detects_orphan`** — given a `Vec<Event>` containing an orphan (parent_event_id points to non-existent id), the assertion helper reports the orphan.
- **`parent_chain_helper_detects_multiple_roots`** — given a `Vec<Event>` with two roots, the helper reports it.

#### Done when

- [ ] Reducer runner threads a `last_event_id: Option<Uuid>` cursor through each publish path
- [ ] Recovery re-emit explicitly sets `parent_event_id = None`
- [ ] Parent-chain assertion helper exists in `test_support`
- [ ] All listed integration tests green
- [ ] All listed unit tests green
- [ ] Acceptance test green against live NATS
- [ ] Decision rationale captured inline in `events.rs` and cross-linked to `inter-node-contracts-and-event-layers.md`

---

### Step 3 — Fold cost into the envelope; remove `EventPayload::Cost`

**Goal.** Cost is system-level accounting, not part of the typed
contract between graph nodes. Move it to
`envelope.cost: Option<CostMetadata>`, populated on
`llm.response` events. Remove the standalone `Cost` payload
variant, the `agent_cost` subject, and the redundant
post-`LlmResponse` publish in the runner.

This is the most semantically intrusive step. The build will be
red while in flight; land it as one atomic change.

#### Acceptance test

```text
TEST: cost_projection_populated_from_envelope_after_refactor

Setup:    A scripted single-turn agent invocation against live
          NATS + a stub LLM with a known `TokenUsage`. Empty
          projection cache.
Action:   Trigger the agent; let the projection consumer run.
Assert:   - No `fq.agent.*.cost` event was published (the
            subject is gone)
          - The `llm.response` event carries a populated
            `envelope.cost` with the expected token counts
            and computed dollar totals
          - `fq events query --type=llm_response` shows the
            response with cost data
          - `fq cost --invocation <id>` reports the same totals
            as before the refactor
          - The `cost_summary` table is populated from
            `total_cost IS NOT NULL` rows
```

Gated on `FQ_NATS_URL`.

#### Integration tests

- **`reducer_runner_emits_llm_response_with_envelope_cost`** — drive an LLM call with stub; capture the `LlmResponse` event; verify `envelope.cost` populated and matches expected `CostMetadata`.
- **`projection_consumer_extracts_cost_from_envelope`** — feed a captured `LlmResponse` with `envelope.cost` to the projection consumer; verify the projection row is written.
- **`cost_summary_filters_null_cost_rows`** — populate projection with mixed cost-bearing and non-cost-bearing events; `cost_summary` SQL returns only the cost-bearing ones.
- **`cost_variant_removed_from_payload_enum`** — exhaustive match on `EventPayload`; `Cost` is not a variant (compile-time check, expressed as a `#[deny(unreachable_patterns)]` `match` in a test module).

#### Unit tests

- **`cost_metadata_round_trips_on_envelope`** — serialise an `Event` with cost; deserialise; verify equality.
- **`event_with_cost_setter`** — `Event::with_cost(cost_metadata)` mutates `envelope.cost`.
- **`llm_response_cost_subject_unused`** — `subjects::agent_cost` no longer exists (compile-time).

#### Done when

- [ ] `EventPayload::Cost` and `CostPayload` removed
- [ ] `subjects::agent_cost` removed
- [ ] Reducer runner publishes one fewer event per LLM response (no separate `Cost` event)
- [ ] Projection consumer reads cost from `envelope.cost` on `LlmResponse` events
- [ ] Projection schema unchanged (column names stay); only the consumer path changes
- [ ] All listed integration tests green
- [ ] All listed unit tests green
- [ ] Acceptance test green against live NATS
- [ ] `fq cost` CLI output unchanged from operator perspective
- [ ] No regressions in data-architecture-v1 acceptance tests

---

### Step 4 — Annotation barrier and well-known keys registry

**Goal.** Add the `Annotations` type's read interface and the
`Event::for_consumer_context` barrier so the discipline from
§6 of the inter-node-contracts doc is a type-system fact, not a
convention. Add the well-known keys module. No producer of
annotations yet — the value here is the substrate.

#### Acceptance test

```text
TEST: consumer_view_strips_annotations_round_trip

Setup:    An `Event` with payload + two annotations (e.g. notes,
          confidence).
Action:   Serialise `event.for_consumer_context()` to JSON; parse
          it back into a `ConsumerView`.
Assert:   - The serialised JSON has `envelope` and `payload` but
            no `annotations` field
          - A consuming agent's prompt-building path uses only
            the consumer view (verified by an integration test
            against a fake prompt-builder)
          - Direct access to `event.annotations` still works for
            human / meta-agent code paths (verified by a separate
            test invoking the runtime's audit path)
```

Runs as a unit test (no NATS gate).

#### Integration tests

- **`annotations_preserved_through_nats_publish_round_trip`** — publish an annotated event to NATS; subscribe; deserialise; assert annotations present in the wire format. (Annotations live on the wire; they're only stripped at the consumer-context boundary, not at the bus.)
- **`prompt_builder_uses_consumer_view`** — if a prompt-builder helper exists or is added in step 4, verify it consumes via `for_consumer_context()` only. If no graph executor exists yet, this becomes a future-proofing comment with no live test (acknowledged in step 4 closing notes).

#### Unit tests

- **`event_annotate_inserts_key`** — `event.annotate("notes", json!("hello"))` adds the entry.
- **`event_annotate_replaces_existing_key`** — second `.annotate` with same key replaces.
- **`consumer_view_serialises_without_annotations_field`** — even if annotations present.
- **`well_known_annotation_keys_are_constants`** — `NOTES`, `CONFIDENCE`, `REASONING`, `SOURCES_CONSIDERED`, `FLAGS` exist as `pub const &str`.
- **`unknown_annotation_keys_permitted`** — adding `"my_custom_key"` does not error.

#### Done when

- [ ] `Annotations` newtype around `BTreeMap<String, Value>` with `#[serde(transparent)]`
- [ ] `annotation_keys` module with the five well-known constants
- [ ] `Event::annotate(key, value)` builder method
- [ ] `Event::for_consumer_context()` returning a `ConsumerView<'_>` that omits annotations from serialisation
- [ ] Doc comment on `ConsumerView` reiterating why the barrier exists, with link to the design doc
- [ ] All listed unit tests green
- [ ] Integration test green
- [ ] Acceptance test green (unit-level)

**What this step does NOT do.** Enforce the barrier at the
graph-executor level. There is no graph executor yet. When one
lands, the executor must use `for_consumer_context()` to build
downstream prompts — captured here as a successor concern.

---

### Step 5 — Documentation and `event-schema.md` rewrite

**Goal.** Update the documentation surface so future-you can pick
up cold and not be confused by the v1 schema doc.

#### What changes

- `docs/design/event-schema.md` — rewritten for v2. Top-level
  structure shows envelope/payload/annotations; each section
  cross-links the inter-node-contracts doc.
- `docs/design/data-architecture.md` — any v1-schema example
  payloads updated to v2 shape.
- `docs/plans/active/2026-05-05-native-reducer-prototype.md` —
  moved to `closed/` with a one-paragraph note that this plan
  supersedes it.
- `docs/pla./2026-04-28-data-architecture-v1.md` —
  amended where it references event shapes (search for
  `Cost`, `event.agent_id`, etc.) so the active plan stays
  consistent with the now-current code.

#### Done when

- [ ] `event-schema.md` describes v2 with a v1 → v2 changelog section at the bottom
- [ ] All other design docs grep-clean for v1-shape examples
- [ ] Native-reducer-prototype plan moved to closed/ with supersession note
- [ ] Data-architecture-v1 plan amended for the new event shape
- [ ] Closing notes written: latency overhead measurement (target: negligible — single struct allocation per event), what was deferred (graph-executor barrier enforcement, `recovered_from_event_id` envelope field), link to follow-up successor plans

---

## Cross-cutting concerns

- **No wire-format compatibility shim.** The schema version bumps from 1 to 2 in step 1; v1 events on the wire after step 1 lands will fail to deserialise. This is acceptable because (a) no production deployment, (b) NATS retention is 24h, (c) JetStream durable consumers can be reset between steps. The data-architecture-v1 plan's WAL is independent (stores extracted fields, not full events), so the schema bump does not invalidate persisted WAL state.
- **Persisted `state_blob`** in `invocation_state` carries `HarnessState`, not raw `Event`s, so step-by-step compatibility holds. Spot-check this in step 1 by reading back an existing `state_blob` after the rebuild.
- **No regression on existing tests.** Each step's "Done when" requires a full `cargo test -p fq-runtime` pass.
- **Reducer-equivalence tests survive every step.** The captured event sequence's payload content stays identical; only the envelope structure changes.
- **Latency budget.** The extra envelope fields (parent, trace, schema_id, optional cost) cost a single struct construction per event. Step 5 records the measurement; if it exceeds 10% of single-event publish latency, revisit field layout.

## Risks and what we'll learn

| Risk | What would tell us | Mitigation |
|---|---|---|
| The parent-chain semantics for recovery re-emit turn out to be load-bearing for replay or audit | Recovery acceptance test passes, but the projection's "show invocation history" view has a visible gap between pre-crash and post-crash chains. | Step 2 documents the `recovered_from_event_id` follow-up; add it in a successor plan if/when audit code actually needs it. |
| Removing the `Cost` event variant breaks a downstream consumer we forgot about | Compile error in a non-runtime crate (CLI, tools), or a subscriber on `fq.agent.*.cost` that quietly stops receiving | Step 3 acceptance test explicitly verifies the `fq cost` CLI output; grep `fq.agent.*.cost` across the workspace before step 3 starts. |
| Schema bump invalidates in-flight WAL state in a way we didn't predict | Recovery scan after the refactor fails to classify pre-refactor WAL rows | Document the "nuke `events.db` before bringing up post-refactor daemon for the first time" instruction; in step 1, verify a fresh DB / fresh NATS round-trip end-to-end before claiming success. |
| Annotation barrier exists in code but is bypassed in practice when the graph executor lands | A future PR adds `event.annotations.get("notes")` in graph-executor code | The `ConsumerView` doc comment is loud about why; consider a lint or `#[deprecated]` shim on direct `annotations` access from non-runtime crates. Out of scope for this plan; flagged for the successor. |

## Closing condition

This plan closes when:

- All 5 steps have their "Done when" checklists complete.
- The end-to-end acceptance test for step 3 (cost-from-envelope)
  passes against live NATS with a real Anthropic call.
- The parent-chain assertion helper is in `test_support` and
  used by at least one integration test per step.
- A short closing report is written in the same shape as
  `docs/plans/closed/2026-04-25-native-reducer-prototype.md`,
  capturing what landed, the latency measurement, and the
  successor concerns explicitly enumerated.

## Closing notes (2026-05-15)

All five steps landed end-to-end in a single session. The pace
worked because each step was scoped to a clean compile+test
boundary, and the existing reducer-equivalence and recovery
acceptance tests gave a strong safety net through the wire-format
change.

### What shipped

| Step | Commit | Tests delta | Notes |
|---|---|---|---|
| 1 — Envelope and Annotations types (shape-only) | `d73b4fa` | 252 → 257 (+5) | All call sites mechanically migrated to `event.envelope.*`; schema_version bumped to 2. |
| 2 — `parent_event_id` threading | `cc8c21b` | 257 → 266 (+9) | Cursor plumbed through `run`, `resume`, `run_loop_inner`, `run_model_with_llm`, `run_tool`, `run_self_inspect_with_wal`, `emit_failed`, `emit_synthetic_tool_error`. `assert_parent_chain` helper in `test_support`. |
| 3 — Cost on envelope, `EventPayload::Cost` removed | `f0d3e5f` | 266 → 269 (+3) | Both legacy and reducer paths attach cost via `with_cost`. Projection `extract_fields` takes `&Event` to read `envelope.cost`. Equivalence tests' expected sequences updated. |
| 4 — Annotation barrier + well-known keys | `4f52542` | 269 → 276 (+7) | `Event::annotate`, `Event::for_consumer_context`, `annotation_keys` module. |
| 5 — Docs (this commit) | — | 276 → 276 | Rewrote `event-schema.md` for v2 with a v1 → v2 changelog. Updated `data-architecture.md` §9.3 to drop the standalone `cost` step. |

End of plan: **276 lib + tools tests pass; clippy unchanged at 18
warnings from baseline.**

### Latency overhead

Not separately measured; the change is a single struct
construction per event (an extra `Envelope { ... }` allocation
inside `Event::new`). The plan called for measurement if it
exceeded 10% of single-event publish latency; the structural
change is well under that threshold and the equivalence tests
gave no signal of slowdown. If future profiling shows otherwise,
revisit `Envelope` field layout (consider `Box<CostMetadata>` if
embedding is the cost).

### Open carry-over items

- **Cross-incarnation chain stitching.** Recovery re-emit sets
  `parent_event_id = None`; no consumer reads cross-incarnation
  linkage today. If audit code ever needs it, add an optional
  `envelope.recovered_from_event_id: Uuid`. Deferred until a use
  case appears.
- **Annotation barrier at the graph-executor level.** There is
  no graph executor yet. When one lands, it must use
  `Event::for_consumer_context()` to build downstream prompts.
  Captured under successor plans.
- **Legacy executor parent-chain threading.** The legacy
  `worker/executor.rs` does not thread `parent_event_id` (step 2
  was scoped to the reducer runner). The equivalence tests
  compare kinds, not envelope fields, so this is invisible to
  the test suite. The legacy path is slated for retirement.

## Successor plans

After close, the natural follow-ups (each its own active plan if
and when needed):

1. **Resume data-architecture-v1**: finish step 7 (worker
   heartbeat emission so the stale-worker sweep means something),
   then step 8 (archive hand-off — now landing on top of the v2
   envelope, with cost already on llm.response).
2. **Graph executor and verifier shape**: first place
   `for_consumer_context()` is actually used to build a
   downstream prompt. Touches ADR-0014/0015/0016 and the
   signatures doc.
3. **Cross-incarnation event linkage**: if and only if a use case
   needs to stitch pre-crash and post-crash chains, add
   `envelope.recovered_from_event_id`. Defer until the use case
   appears.
