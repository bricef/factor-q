# ADR-0034: Reasoning is a content part, and parts stop at the operator boundary

## Status

Draft (2026-08-25). Output of the co-design session of 2026-08-25, per
[CONTRIBUTING § Design sessions](../../../CONTRIBUTING.md) — *"the session's
output is the design document itself"*. Records the maintainer decisions taken
on [#437](https://github.com/bricef/factor-q/issues/437) on 2026-07-28 and
2026-08-25. Execution is sequenced by
[the plan](../../plans/active/2026-08-25-reasoning-as-message-parts.md).

Contract precondition for [#414](https://github.com/bricef/factor-q/issues/414)
(confirmed exit criterion). Amends
[`inter-node-contracts-and-event-layers.md`](../../design/aspirational/inter-node-contracts-and-event-layers.md)
§6–§7 and [`event-schema.md`](../../design/committed/event-schema.md).

## Context

factor-q discards model reasoning end to end. Nothing captures it from the
provider response, no internal type has a field to hold it, and nothing
replays it on the next turn. A factor-q invocation *is* a multi-turn
conversation — the runner replays the whole thing on every step — so at turn N
the model sees its own prior text, its tool calls, and the tool results, but
not the reasoning behind any of it.

For Anthropic models that is graceful degradation; the visible assistant text
survives and models write their plan into it. **For reasoning-first models it
is a correctness problem.** Kimi- and DeepSeek-class models carry the substance
of a turn in `reasoning_content`; the visible `content` is a thin statement of
the conclusion, not the work that produced it. The failure is silent and
compounds with `max_iterations` — long agentic loops lose the most.

[ADR-0003](../accepted/0003-model-agnostic-per-agent.md) commits factor-q to
model-agnostic, per-agent model selection. This gap quietly excludes a model
class from that promise, and it is reachable today: `api_shape =
"openai-compatible"` plus an endpoint override is exactly how Moonshot/Kimi
would be wired right now.

**The root shape is not a missing field.** `Message` is
`{role, content: Option<String>, tool_calls, tool_call_id}` — text-shaped — and
so is every representation derived from it. Every provider's reasoning
mechanism is a content part in an ordered assistant turn, sitting alongside
text and tool calls and sometimes carrying an opaque continuity token. There is
no slot for that in a single `Option<String>`, at any of the layers that
currently have one.

An assistant turn exists in four kinds of representation, and the text-shaped
assumption is baked into all of them:

| Kind | Type | Purpose |
|---|---|---|
| Conversation state | `HarnessState.messages: Vec<Message>` | **The only thing replayed to the model.** |
| Durable record | `LlmResponsePayload`, `llm_dispatch` WAL | The system of record ([ADR-0026](../accepted/0026-event-log-system-of-record.md)). |
| Derived fact | `TurnState` / `TurnAction` | The operator's atom, served by `turn.*`. |
| Rendering | `TranscriptEntry` | CLI pretty/JSON and the dashboard. |

## Decision

**D1 — Reasoning is a content part, and `Message` is an enum over turn
kinds.** The narrow `reasoning: Option<String>` alternative is rejected
outright and is **not** to be implemented as a stepping stone (maintainer,
2026-07-28).

```rust
pub enum Message {
    System(String),
    User(String),                  // Vec<UserPart> when binary lands
    Assistant(Vec<AssistantPart>),
    ToolResults(Vec<ToolResult>),  // one turn, N results
}

pub enum AssistantPart {
    Text(String),
    Reasoning(Reasoning),
    ToolCall(MessageToolCall),
}
```

**The turn kind is the variant, so the role is carried by the type rather than
by a `role` field a part vector can disagree with.** This deletes four invalid
states that `{role, parts}` would leave representable: a `Tool` message with no
correlation id, a reasoning part in a user turn, a tool call in a user turn,
and a tool result in an assistant turn. It also makes *N tool results in one
turn* expressible for the first time — see D1b.

Parts are admissible by construction, not by convention: each variant carries
the parts that turn kind can hold. A single flat `ContentPart` reused across
variants would smuggle the invalid states back in, and upstream shows what that
costs — genai carries the comment *"ToolCall is not valid in user content for
Anthropic; skip gracefully"*, i.e. an invalid combination handled by silently
dropping it.

**D1b — One turn's tool results are one message.** A corollary of D1:
`ToolResults(Vec<ToolResult>)` carries every result answering a single
assistant turn. This matches the Anthropic wire shape (`tool_result` blocks
batched in one user turn) and genai's own model, whose adapter already loops
over a tool message's parts. It makes the correct shape *expressible*; it does
**not** by itself change what the harness emits — that is
[#511](https://github.com/bricef/factor-q/issues/511).

**D2 — A reasoning part has three shapes**, because code branches on the
difference:

| Shape | Provider | Readable text | Continuity token |
|---|---|---|---|
| Plain | Kimi, DeepSeek (`reasoning_content`) | yes | none |
| Signed | Anthropic (`thinking` + `signature`) | yes | yes — and it **encrypts the full reasoning**, so the visible text is not the payload |
| Opaque | Anthropic (`redacted_thinking`), Gemini (`thought_signature`) | none | yes — the token *is* the content |

Every reasoning part names the model that produced it.

**D3 — Parts are an internal concern and stop at the operator boundary.**
`Message` is the enum above; `ChatResponse`, `ModelResponse` and
`LlmResponsePayload` carry `Vec<AssistantPart>` — **a response is an assistant
turn**, so it takes that turn kind's part type rather than a flat one, and a
response structurally cannot contain a tool result. `TurnAction` and
`TranscriptEntry` are **not** parts-shaped. The operator
surface receives a *reasoning* concept, never a *parts* concept, and provider
vocabulary — part taxonomies, signatures, thought tokens — never crosses into
it.

**D4 — The data model distinguishes absence from opacity.** Reasoning that
reaches the operator surface is **named even when it cannot be rendered**.
Opaque reasoning is reported as present-and-opaque, with its raw form
reachable; it is never omitted, and never flattened into "no reasoning".

> It is still information flowing through the system. That we cannot interpret
> it does not reduce its significance to the system's behaviour.

**Presentation may collapse what the data model must distinguish.** A UI
rendering `opaque — click to see raw` is correct; a data model that reports
`reasoning: none` for the same turn is lying. The concrete shape at the
boundary:

```rust
pub struct TurnReasoning {
    /// Readable working, when the provider exposed any.
    pub text: Option<String>,
    /// Provider content carried but not interpretable here. `None`
    /// means there genuinely was none — never that it was dropped.
    pub opaque: Option<Value>,
}
```

Which distinguishes all four honest states: no reasoning at all
(`reasoning: None` on the turn), Plain (`text`), Signed (`text` + `opaque`),
Opaque (`opaque` alone).

**D5 — Reasoning is model-tied, and the strip is enforced, not conventional.**
Replaying a reasoning part to a model other than the one that produced it is
impossible without an explicit strip. Enforced at a single choke point in the
provider adapter — the last gate before the wire, and the only place that knows
both the target model and the provider encoding — and asserted by the oracle.

**D6 — The invocation is the boundary between replay and exposure.** Within
one invocation, reasoning replays freely: it is the model's own working handed
back to itself, exactly as the provider contract assumes. Across an invocation
boundary it is stripped, **whether it arrives via annotations or via a payload
part**.

**D7 — Reasoning reaches the event log and the transcript**, and renders
behind a flag in both the CLI and the dashboard. Retention follows the existing
event-log rules; this is additive and windows nothing that exists today.

**D8 — Reasoning tokens are captured as a decomposition.** `reasoning_tokens`
splits `output_tokens` into thought-vs-spoken for visibility. `total_cost` is
unchanged, to the cent, before and after.

## Rationale

**The transport shape is the domain shape — but whose domain?** The governing
principle from #437 is *if the upstream APIs return a part vector, we should
too*. Anthropic returns an ordered array of `text` / `thinking` /
`redacted_thinking` / `tool_use` blocks; OpenAI-compatible providers return
parts plus sibling fields. A single `Option<String>` cannot represent that, and
every workaround re-encodes a shape the provider already gave us.

But there are **two domains here, not one**, and D3 is the line between them.
The conversation-as-the-model-sees-it is a provider artefact: ordered parts,
continuity tokens, model ties. The conversation-as-the-operator-sees-it is a
different thing entirely — `TurnAction` carries `cost_usd`, `is_error`,
`round`, `initiating_turn`, concerns that have no place on the wire. Making the
atom parts-shaped would import provider vocabulary into the operator domain,
which is exactly lesson 7 of the
[fq-ops design review](../../reviews/2026-07-21-fq-ops-design-review-learnings.md):
*"infrastructure vocabulary in model types is a permanent leak."* The operator
does not want a part vector; they want "here is the model's working, and here
is what it said."

**The response chain carries parts anyway (D3, first half), despite a response
being converted-into rather than replayed.** This is the pragmatic half of the
decision: it keeps one shape across every wire-facing type and avoids a
conversion seam that would need undoing the first time a second part kind —
images, documents — needs a home.

**Why the invocation is the right boundary (D6).** The codebase currently reads
as *"reasoning is deliberately excluded"*, because of
`annotation_keys::REASONING` and the `for_consumer_context()` strip. That
discipline is about a **consumer** never seeing a **producer's** reasoning,
which is what makes fresh-context verification path-independent. Replaying a
model's own reasoning inside its own single conversation is not that: one
agent, one context, one invocation. The distinction was nowhere in writing, so
a contributor could reasonably have read this change as breaking §6 of the
inter-node contracts. It is now written down.

**Why D4 is a decision and not a detail.** The failure this ADR exists to fix
is a *silent* one. A fix that renders opaque reasoning as absence would
reproduce the same class of error one layer up — an operator reading a
transcript would conclude the model did no thinking, when in fact it did
thinking we chose not to display. Absence and opacity are different facts and
the system must say which one it is holding.

## Consequences

### Positive

- Reasoning-first models work as their providers intend; ADR-0003's promise
  extends to a model class it silently excluded.
- Reasoning gains a name in the type system, which is the precondition #414
  needs: the cross-model strip becomes a graph invariant that can be expressed
  and enforced, rather than a convention that degrades into a bug billing input
  tokens on every cross-model edge.
- The enum **deletes** four invalid states rather than moving them, including
  the `MessageRole::Tool` + `tool_call_id: Option<_>` pairing that has a live
  runtime error to prove it (`genai.rs:296`, *"tool role message is missing
  tool_call_id"*). `ToolResult` carries its own id, so the error's precondition
  ceases to exist and the check is deleted, not relocated.

  This is worth stating precisely because the intermediate option —
  `Message { role, parts: Vec<ContentPart> }` — **would** have moved them:
  it trades "Tool role missing its id" for "ToolResult part in a non-Tool
  role", both runtime-checked in the same adapter. Only making the turn kind
  the variant removes the question from the type. An earlier draft of this ADR
  credited the intermediate option with the deletion; that was wrong.
- Consumers that switched on `role` gain exhaustiveness checking. `turn.rs`'s
  `is_opening_request` — *"the message list carries no assistant and no tool
  message"* — becomes a match the compiler completes.
- The thought-vs-spoken cost split becomes visible, which for reasoning-first
  models is most of the bill.

### Negative

- Breaking event-schema change (`SCHEMA_VERSION` 2 → 3) and a break in
  persisted reducer state, so in-flight invocations do not survive the upgrade.
  Acceptable per STATUS.md's pre-alpha position, which explicitly puts
  compatibility, migration and rollback out of scope — but it must fail
  loudly rather than mis-parse.
- The change surface spans the LLM adapter, the reducer, the event log, the
  operator atom, the transcript, the CLI and the dashboard. It is wide even
  though each layer's diff is shallow.
- Anthropic support depends on upstream `rust-genai` work, which is a required
  outcome of this effort but paced partly by an external review cycle. The
  fork rung of the resolution ladder is the contingency.

### Neutral

- Reasoning is retained indefinitely alongside the rest of the event log. That
  is a retention posture, taken deliberately; it should be revisited if and
  when a privacy or data-residency requirement appears.
- The parts vector is ordered, and provider order is preserved on the way in.
  Each adapter emits in whatever order its own API requires — Anthropic demands
  thinking blocks first in an assistant turn; OpenAI-compatible carries
  reasoning as a sibling field where position is meaningless. **We promise the
  vector is ordered; we do not promise a canonical cross-provider order.**

## Alternatives considered

**A narrow `reasoning: Option<String>` scalar.** Small diff, unblocks
Kimi/DeepSeek immediately. Rejected by the maintainer on 2026-07-28: it adds a
second scalar beside `content` that does not generalise — no signature, no
ordering, and nothing to build on when images, documents, or Anthropic thinking
blocks need a home. Explicitly not to be used as a stepping stone.

**`Message { role, parts: Vec<ContentPart> }` — a role field beside a flat part
vector.** The shape every provider uses on the wire, and the obvious middle
option. Rejected: it leaves four invalid combinations representable and
runtime-checked, and a flat part enum forces the silent-skip behaviour genai
already exhibits. The wire-fidelity argument for it does not apply here —
`Message` is the *replay* type, built by the reducer and converted at the
adapter; nothing ever deserialises a provider response into it. Wire fidelity
is the response chain's job, and D3 keeps it there.

**Richer type machinery over parts** — a role type parameter, phantom role
markers, or a trait declaring which roles admit which parts. Rejected on
lesson 2's second test: *is any consumer generic over this abstraction?* No —
every consumer handles one turn kind at a time, and nothing iterates parts
polymorphically across roles. A trait declaring admissibility is also exactly
the trait/descriptor duality lesson 4 warns about; the enum is the value that
duality dissolves into.

**Hoisting reasoning out of the ordered vector** —
`Assistant { reasoning: Vec<Reasoning>, body: Vec<BodyPart> }`, which would make
Anthropic's "thinking blocks first" structural rather than conventional.
Tempting, and rejected by I6: it asserts reasoning never interleaves with text
or tool calls, which is not established across providers. Order stays a
provider concern, expressed as one ordered vector the adapter emits in its own
required order.

**Parts all the way to the operator surface.** Uniform, and one fewer
conversion. Rejected as a vocabulary leak (D3 rationale): the operator surface
would acquire provider concepts it can do nothing with, and would be coupled to
every future change in a provider's part taxonomy.

**Reasoning in the annotations layer.** Has surface appeal —
`annotation_keys::REASONING` already exists for it. Rejected on two counts:
annotations are *advisory* and reasoning is load-bearing for the next turn's
correctness, and the annotation barrier would strip it at exactly the moment
the model needs it back. The existing key remains what it always was: a slot
for an agent's *self-reported* working, with no writer.

**Keeping reasoning only in the reducer state blob.** Keeps it out of the event
log entirely, so no retention question arises. Rejected: it would be invisible
for debugging and absent from the transcript, and the event log is the system
of record — a fact material to the model's behaviour that is deliberately kept
out of the log is a hole in that claim.

## Open questions (deferred by decision)

- ~~**Q1 — Does the tool result become a part?**~~ **Closed 2026-08-26:** yes,
  and further than the question assumed — the turn kind became the enum
  variant (D1), which deletes the invalid states rather than relocating them.
- **Q2 — Which upstream genai shape to argue.** Giving the reasoning part a
  signature field is semantically honest but breaks a public enum variant;
  routing to `ContentPart::Custom` is purely additive but means *"we don't know
  what this is"* about a block the parser demonstrably recognises. Lead with
  the honest one, offer the compatible one as fallback.
- **Q3 — `TurnFilter::abbreviate`.** Deferred by decision (2026-08-25): the
  system is correct without it. Revisit when `turn.list` actually strains the
  8 MiB frame ceiling — reasoning is a second unbounded field on that path, and
  the first that is routinely rather than exceptionally large.
- **Q4 — Restate or withdraw #424's gate on this work.** Its stated rationale
  (*"`events` has fan-in 10"*) re-measures on `main` to a 33-line re-export
  facade with fan-out 1, and the vocabulary that changes now lives outside
  fq-runtime's cycle group. Maintainer call.
- **Q5 — Does the parallel-tool-result batching get fixed here?** No. D1b makes
  the batched shape *expressible*; changing what the harness emits is a
  separate behavioural fix with its own test coverage gap
  (`mock_anthropic.rs` builds responses carrying one `tool_use` block, so the
  parallel path has no hermetic coverage today). Tracked as
  [#511](https://github.com/bricef/factor-q/issues/511) so the shape change and
  the behaviour change stay reviewable apart.

## References

- [#437](https://github.com/bricef/factor-q/issues/437) — the issue, its
  provider analysis, and the maintainer decisions of 2026-07-28.
- [Execution plan](../../plans/active/2026-08-25-reasoning-as-message-parts.md) —
  phases, invariants, test plan, and the upstream genai scope.
- [`experiments/reasoning-round-trip/`](../../../experiments/reasoning-round-trip/) —
  the probe that refuted the silent-disable hypothesis on Anthropic, and whose
  `echo` arm is the correct protocol this ADR enables.
- [ADR-0003](../accepted/0003-model-agnostic-per-agent.md) — per-agent model
  selection, which makes cross-model edges structural.
- [ADR-0014](../accepted/0014-agent-harness-as-reducer.md) — the reducer loop
  whose replay path this corrects.
- [ADR-0026](../accepted/0026-event-log-system-of-record.md) — why D7 puts
  reasoning in the log.
- [Operator surface domain model](../../design/committed/operator-surface-domain-model.md) —
  atoms, views, and the boundary D3 draws.
- [fq-ops design review learnings](../../reviews/2026-07-21-fq-ops-design-review-learnings.md) —
  lesson 7 (vocabulary leaks) and lesson 2 (structure must carry semantic
  weight).
