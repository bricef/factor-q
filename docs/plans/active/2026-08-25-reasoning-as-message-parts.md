# Reasoning as a first-class message part — execution plan

**Status:** active (2026-08-25). Tracking issue:
[#437](https://github.com/bricef/factor-q/issues/437). Contract precondition
for [#414](https://github.com/bricef/factor-q/issues/414) (multi-node MVP,
confirmed exit criterion) and currently drawn as a blocker for
[#424](https://github.com/bricef/factor-q/issues/424) (module coupling epic) —
though §2.2 below re-measures that second claim and finds it no longer
describes the tree.

**Decisions carried in from the issue (maintainer, 2026-07-28):**

- **Parts vector, not a narrow scalar.** `Message` carries an ordered part
  list. The narrow `reasoning: Option<String>` option is explicitly *not* a
  stepping stone — do not implement it. Governing principle: *if the upstream
  APIs return a part vector, we should too.*
- **Schema bumps are not a constraint.** Pre-alpha; model the domain
  correctly and bump `SCHEMA_VERSION` as needed.
- **genai is a dependency, not a ceiling.** Resolution ladder, in order:
  fix upstream → fork → migrate. We do not design factor-q's message contract
  around a client library's limitation.
- **Dedicated design work is in scope**, with an ADR as the vehicle.

**Decisions taken 2026-08-25 (maintainer):**

- **Reasoning reaches the event log and the transcript**, not the reducer
  state blob alone. This settles open question 2 of the issue, and makes the
  consumer-barrier extension (§4.4) load-bearing rather than theoretical.
- **Scope is the shape, the OpenAI-compatible provider path, and the cost
  decomposition** — and **the upstream genai contribution is a required
  outcome of this work**, not separable follow-on. §5 scopes it.

**Design settled by the co-design session of 2026-08-25**, recorded as
[ADR-0034](../../adrs/accepted/0034-reasoning-as-a-content-part.md). The session
answered three of this plan's original open questions and added one invariant:

- **Parts are an internal concern and stop at the operator boundary.**
  `Message` is parts-carrying and `ChatResponse` / `ModelResponse` /
  `LlmResponsePayload` take `Vec<AssistantPart>`; `TurnAction` and
  `TranscriptEntry` are not parts-shaped. Provider vocabulary never crosses
  into the operator domain (ADR-0034 D3).
- **The response chain carries parts anyway**, despite a response being
  converted-into rather than replayed — pragmatic, and it avoids a conversion
  seam that would need undoing when a second part kind arrives.
- **Reasoning renders behind a flag**, in the CLI *and* the dashboard. Because
  the transcript is composed over turns and never the reverse, this pulls the
  **operator surface and the dashboard into scope** — a flag can only reveal
  what the layer beneath already carries.
- **The data model distinguishes absence from opacity** (I7 below). New, and
  the strongest constraint the session produced.
- **`TurnFilter::abbreviate` is deferred** — the system is correct without it.

**Decided 2026-08-26 (maintainer), closing this plan's Q1:** `Message` becomes
an **enum over turn kinds**, not a `{role, parts}` struct. The turn kind is the
variant, so the role is carried by the type. See §4.2 — this is a stronger
form of the change than the original question posed, and it deletes invalid
states that both options considered would only have relocated.

## The one-line principle

**Reasoning is the model's own working, handed back to itself.** The boundary
that matters is the **invocation**, not the turn: *within* one, reasoning is
continuity and must round-trip; *across* one, reasoning is coupling and must
be stripped. Every decision below follows from which side of that line a given
piece of code sits on.

That sentence is also the resolution of the issue's open question 1. The
codebase currently reads as *"reasoning is deliberately excluded"* because of
`annotation_keys::REASONING` and the `for_consumer_context()` strip. That
discipline is about a **consumer** never seeing a **producer's** reasoning,
because that is what makes fresh-context verification path-independent.
Replaying a model's own reasoning inside its own single conversation is not
that: one agent, one context, one invocation, and the reasoning is the
model's own working being returned to it exactly as the provider's contract
assumes. §4.4 writes the rule down so a future contributor cannot read this
change as breaking §6 of `inter-node-contracts-and-event-layers.md`.

## 1. What is dropped, and where

Every line number in the issue body is stale — `events.rs` was extracted into
the `fq-ops` contract crate (ADR-0031 Phase 1) after the issue was written.
The map as it stands on `main` (`9477254`):

| Layer | Location today | State |
|---|---|---|
| Provider → internal | `fq-runtime/src/llm/genai.rs:378` `from_provider_response` | Builds from `first_text()` only. `ChatResponse.reasoning_content` is never read. |
| Internal → provider | `fq-runtime/src/llm/genai.rs:284` `convert_message` | Emits `Text` + `ToolCall` parts only. No reasoning part is ever constructed. |
| Internal response | `fq-runtime/src/llm.rs:34` `ChatResponse` | `{content, tool_calls, stop_reason, usage}` — no field. |
| Reducer boundary | `fq-runtime/src/worker/reducer/types.rs:170` `ModelResponse` | Same four fields. The reducer structurally cannot see reasoning. |
| Event log | `fq-ops/src/events/llm.rs:127` `LlmResponsePayload` | Same four fields plus `round` / `origin`. |
| Request path | `fq-ops/src/events/llm.rs:62` `Message` | `content: Option<String>` — no parts list, so there is nowhere to put reasoning on the way back in. |

**The drop the issue does not name, and the one that matters most:**

> `fq-runtime/src/worker/reducer/harness.rs:329`

```rust
state.messages.push(Message {
    role: MessageRole::Assistant,
    content: response.content.clone(),
    tool_calls: response.tool_calls.clone(),
    tool_call_id: None,
});
```

This is the replay path. `HarnessState.messages` is the conversation the
reducer rebuilds and re-sends on **every** turn; it round-trips through the
opaque `state` blob (`invocation_state.state_blob`, JSON) between steps.
Everything else in the table above is plumbing whose only purpose is to get
reasoning *to* and *from* this line. A change that lands the parts vector
everywhere but leaves line 329 dropping reasoning has achieved nothing.

The drop is at least clean rather than corrupting: `first_text()` filters to
`is_text()` parts, so reasoning is never silently concatenated into `content`.

## 2. What has changed since the issue was written

Four corrections, each of which moves work off or onto the critical path.

### 2.1 The vocabulary moved to `fq-ops`

`Message`, `LlmResponsePayload` and `SCHEMA_VERSION` now live in the `fq-ops`
contract crate. In `fq-runtime`, `events.rs` is a 34-line re-export facade
whose only local content is the `LlmError → LlmErrorKind` conversion. The
import path (`crate::events::…`) is unchanged, so this is a file boundary
move, not an API one — but it relocates the whole change surface into a
different crate.

### 2.2 #424's blocking rationale no longer describes the tree

The gate on #424 reads: *"`events` has fan-in 10, so this change ripples
through the same ten modules that epic restructures. One disruption, not
two."* Re-measured on `main` with `just lint-coupling`:

| | #424's table (2026-07-27) | `main` today |
|---|---|---|
| `fq-runtime::events` prod lines | 1,329 | **33** |
| `fq-runtime::events` fan-out | 2 (`agent`, `worker`) | **1** (`llm`) |
| `fq-runtime::events` fan-in | 10 | 12 |
| fq-runtime cycle group | 11 modules | **15 modules** |

The fan-in is a **re-export** fan-in. Re-exports do not churn when the
re-exported struct gains a field; only sites that *construct or destructure*
`Message` need touching, and those are countable (§3.3). The vocabulary that
genuinely changes now lives in `fq-ops::events` (1,744 prod lines, fan-out 2,
fan-in 3), inside `fq-ops`'s own small 3-module cycle (`agent` ↔ `events` ↔
`worker`) — which is **not** the 15-module fq-runtime cycle that #424
restructures.

**Action: none here — parked 2026-08-26.** The evidence is captured on
[#424](https://github.com/bricef/factor-q/issues/424) and
[#415](https://github.com/bricef/factor-q/issues/415), and the block stands
until someone chooses otherwise. It is recorded in this plan only because
re-measuring the graph was a by-product of scoping the change, not because
anything here waits on it. See §8.

Two findings went to those issues rather than living here, since they are
cleanup work and not reasoning work: the cycle is now carried by
`worker → trigger → views → worker` rather than by anything touching the
event vocabulary, and `fq-runtime::events` is one ~12-line `From` impl away
from being a pure sink (fan-in 12, fan-out 0) — its only remaining outbound
reference.

### 2.3 Upstream genai is in better shape than recorded

Verified against the pinned source (`genai = "0.6"` → `0.6.5` in
`Cargo.lock`), not against the issue's summary:

- **OpenAI-compatible round-trip is confirmed complete.** Response side:
  `openai/adapter_impl.rs::to_chat_response` reads `/message/reasoning` then
  `/message/reasoning_content` into `ChatResponse.reasoning_content`
  **unconditionally** — `capture_reasoning_content` gates only the *streaming*
  path, so factor-q needs no option flag. Request side:
  `openai/adapter_shared.rs:375` collects `ContentPart::ReasoningContent`
  parts from the assistant turn and hoists them into the sibling
  `reasoning_content` field, under a comment naming the case directly:
  `// Echo reasoning_content back for providers that require it (Kimi, DeepSeek)`.
  Note the **asymmetry**: we read a sibling field and write a content part.
- **`ContentPart` has gained `ThoughtSignature(String)`** since the issue was
  written, and `ToolCall` carries a `thought_signatures` field — the Gemini
  path the issue lists as blocked on us.
- **The Anthropic path is far less blocked than assumed.** genai's Anthropic
  parser routes every *unrecognised* block type into
  `ContentPart::Custom(CustomPart { model_iden, data })` — raw provider JSON
  preserved verbatim, tagged with the producing model.
  **`redacted_thinking` already survives this way today.** Only `thinking` is
  special-cased, into the lossy `reasoning_content: Option<String>` that drops
  `signature`. See §5.

  Two consequences. First, the upstream fix needs **no new part type** — the
  library already has a lossless, model-tagged carrier. Second,
  `CustomPart.model_iden` is *literally* the producing-model tie that #414's
  cross-model strip invariant requires; upstream independently reached the
  same modelling conclusion.

### 2.4 Two of #414's proposed exit criteria are already met

#78 (`runner.rs` split) is **closed** — `worker/reducer/runner/` now exists
with `llm.rs`, `replay.rs`, `server_request.rs`, `failure.rs`, `config.rs`.
#189 is open but `fq-cli/src/lib.rs` is **233 lines** across 18 modules; the
6.3k-line headline is resolved even if the listener/shutdown-join extraction
is not. Only #191 (`mcp.rs`, 1,851 lines) and the two security PoCs (#399,
#400) remain outstanding of the five *proposed* criteria.

**Consequence for sequencing:** STATUS.md's deliberately-open ordering
question — *"whether #437 lands before or after #78/#189/#191, since both
touch `runner.rs`"* — is largely moot. `runner.rs` is already split. This work
should not wait on #191.

## 3. Target invariants

What must be true when this is done.

**I1 — Round-trip fidelity.** For an OpenAI-compatible provider that returns
`reasoning_content`, the assistant message factor-q sends on turn N+1 carries
that provider's reasoning back on the wire, byte-identical to what was
received.

**I2 — Model-tie.** Every reasoning part names the model that produced it.
Replaying a reasoning part to a *different* model is impossible without an
explicit strip — reasoning blocks are model-tied by provider contract, and
ADR-0003 guarantees per-agent model selection, so cross-model edges exist by
construction. Enforced at a single choke point, not by convention.

**I3 — Invocation boundary.** Reasoning replays freely within the invocation
that produced it and never crosses an invocation boundary into a consuming
agent's context — whether it arrives via annotations *or* via a payload part.
This extends §6 of `inter-node-contracts-and-event-layers.md` to parts.

**I4 — Cost is a decomposition, never a new charge.** Capturing
`reasoning_tokens` splits `output_tokens` into thought-vs-spoken for
visibility. `total_cost` is unchanged, before and after, to the cent.

**I5 — Nothing is silently lost.** A provider part factor-q does not
understand is preserved or refused, never dropped on the floor. The failure
mode this whole issue documents is a *silent* one; the fix must not introduce
another.

**I7 — Absence and opacity are different facts.** Reasoning the system carries
but cannot interpret is reported as *present-and-opaque*, never as absent and
never flattened into "no reasoning". Presentation may collapse the distinction
(`opaque — click to see raw`); the data model may not. This is the sharper
sibling of I5: I5 says do not lose it, I7 says do not misreport it. It matters
because the failure this whole change exists to fix is a *silent* one, and
rendering opaque reasoning as absence would reproduce that class of error one
layer up — an operator reading a transcript would conclude the model did no
thinking when it demonstrably did.

**I6 — Ordering is a provider concern.** Provider order is preserved on the
way in. Each adapter emits in whatever order its own API requires (Anthropic
demands thinking blocks first in an assistant turn; OpenAI-compatible carries
reasoning as a sibling field where position is meaningless). We promise the
vector is ordered; we do not promise a canonical cross-provider order.

## 4. Design — settled by ADR-0034

Per CONTRIBUTING's co-design practice and lesson 1 of the
[fq-ops design review](../../reviews/2026-07-21-fq-ops-design-review-learnings.md)
(*"when review comments correct ontology, stop coding and model"*), Phase 0 was
a modelling session. **It has now run** (2026-08-25), and its output is
[ADR-0034](../../adrs/accepted/0034-reasoning-as-a-content-part.md) — which is the
authority for what follows. This section is the working summary; where the two
disagree, the ADR wins. What remains genuinely open is in §8, reduced from five
questions to two.

### 4.1 What a reasoning part is

Three shapes exist across providers, and code branches on the difference:

| Shape | Provider | Readable text | Continuity token |
|---|---|---|---|
| Plain | Kimi, DeepSeek (`reasoning_content`) | yes | none |
| Signed | Anthropic (`thinking` + `signature`) | yes | yes — and it **encrypts the full reasoning**, so the visible text is not the payload |
| Opaque | Anthropic (`redacted_thinking`), Gemini (`thought_signature`) | none | yes — the token *is* the content |

Applying lesson 2's test (*"does any code branch on this distinction?"*): yes,
in three places — the transcript renders plain and signed text but must render
opaque as `[redacted]`; each adapter encodes the three differently on the
wire; and the strip rule's *consequence* differs (dropping plain text costs
continuity, dropping a signed block within a tool-use turn is a protocol
violation). So the distinction is real structure, not speculative machinery.
Proposed:

```rust
pub enum Reasoning {
    /// Readable working, no continuity token. Kimi, DeepSeek.
    Plain(String),
    /// Readable working plus a token that must be echoed verbatim.
    Signed { text: String, token: OpaqueToken },
    /// No readable content — the token is the content.
    Opaque(OpaqueToken),
}
```

All three carry the producing model (I2). Only `Plain` gets a live
encoder/decoder in this work; the other two are named because they are
documented provider realities, and their adapters land with §5.

### 4.2 What `Message` becomes

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

**The turn kind is the variant, so the role is carried by the type.** Four
invalid states stop being representable: a `Tool` message with no correlation
id, a reasoning part in a user turn, a tool call in a user turn, a tool result
in an assistant turn. `genai.rs:296`'s *"tool role message is missing
tool_call_id"* is **deleted, not relocated** — `ToolResult` carries its own id,
so the error's precondition ceases to exist.

That distinction is the whole reason this shape won over `{role, parts}`. A
role field beside a flat part vector would have traded one runtime-checked
invariant for another (*"ToolResult in a non-Tool role"*), both caught in the
same adapter. Only the enum removes the question from the type. The full
comparison, including the two richer ontologies considered and rejected, is in
[ADR-0034](../../adrs/accepted/0034-reasoning-as-a-content-part.md) § Alternatives.

Each variant carries the parts its turn kind can hold, rather than sharing one
flat `ContentPart`. Upstream shows the cost of not doing this: genai carries
the comment *"ToolCall is not valid in user content for Anthropic; skip
gracefully"* — an invalid combination handled by silently dropping it, which is
the failure mode I5 exists to prevent.

**A response is an assistant turn**, so `ChatResponse`, `ModelResponse` and
`LlmResponsePayload` carry `Vec<AssistantPart>` rather than a flat part type. A
response structurally cannot contain a tool result.

**`ToolResults` makes the batched shape expressible for the first time** — and
that is all it does here. Today `tool_results_step` pushes one `Message` per
result, and genai maps each `ChatRole::Tool` message to its own Anthropic user
message, so N parallel results become N consecutive user messages where
Anthropic's documented shape is one user message with N `tool_result` blocks.
**This plan does not change that behaviour.** The path also has no hermetic
coverage — `mock_anthropic.rs` builds responses carrying one `tool_use` block —
so the fix needs test infrastructure that does not exist yet. Tracked
separately (§8) so the shape change and the behavioural fix stay reviewable
apart.

`User(String)` is a deliberate minimum: every user message factor-q constructs
is plain text, and MCP sampling already flattens multimodal content upstream of
this type via `sampling_message_text`. It becomes `Vec<UserPart>` when binary
content arrives — cheap, pre-alpha, and not worth a one-variant enum today.

### 4.3 Where reasoning lives (settled 2026-08-25)

Event log **and** transcript. Consequences to build:

- `LlmResponsePayload` carries reasoning parts; `SCHEMA_VERSION` 2 → 3 with a
  changelog entry in `event-schema.md`.
- `TranscriptEntry::Assistant` gains a reasoning field, **rendered behind a
  flag** — settled 2026-08-25. Default-off, because reasoning is the least
  useful part of a transcript read for *what happened* and can be large.
- Retention follows the existing event-log rules. Per the cost-retention
  principle this is additive: nothing that exists today is windowed or lost.

### 4.6 The operator boundary — where parts stop

The transcript is *"a rendering composed over turns, never the reverse"*
(`fq-ops/src/turn.rs`). So a transcript flag can only reveal what `TurnState`
already carries, and reasoning must reach the operator atom for §4.3 to have
anything to render. That is what pulls `turn.*` and the dashboard into scope.

But the atom gets a **reasoning concept, not a parts concept** (ADR-0034 D3).
`TurnAction` carries `cost_usd`, `is_error`, `round`, `initiating_turn` — an
operator vocabulary with no wire concepts in it, and importing a provider part
taxonomy would be the vocabulary leak of lesson 7. So the internal three-way
`Reasoning` enum reduces at this boundary to:

```rust
pub struct TurnReasoning {
    /// Readable working, when the provider exposed any.
    pub text: Option<String>,
    /// Carried but not interpretable here. `None` means there genuinely
    /// was none — never that it was dropped.
    pub opaque: Option<Value>,
}
```

Which is I7 made concrete: it distinguishes all four honest states — no
reasoning (`reasoning: None` on the turn), Plain, Signed (both fields), and
Opaque (`opaque` alone). Signatures never cross this boundary as signatures;
nothing above the adapter can do anything with one.

**Two consumers, one type.** `fq invocation transcript` and the dashboard both
render `fq_ops::transcript::TranscriptEntry` (`render.rs:484`), so the flag and
the opaque affordance are built once. The dashboard is where
`opaque — click to see raw` lives; the CLI's `--json` stays honest by
construction, since it emits the structure above verbatim.

### 4.4 The consumer barrier, extended to parts

**This is the sharp edge, and the issue does not name it.**
`for_consumer_context()` strips **annotations**. Reasoning as a first-class
part lands in `LlmResponsePayload` — the **payload** layer, which the barrier
does **not** strip. So "make reasoning first-class" silently moves reasoning
from the *excluded* side of the barrier to the *permissive* side.

`for_consumer_context()` has **no production caller today** (only tests),
because multi-node does not exist yet — which is precisely why writing the
rule now is free and writing it after #414 ships is not. §7 of
`inter-node-contracts-and-event-layers.md` currently reads flatly
*"Reasoning traces / chain-of-thought → annotations, with strict barrier
enforcement"*, which becomes wrong-as-written the moment reasoning is a
payload part. Both that line and §6 need amending to state I3.

### 4.5 Where the strip is enforced

Single choke point in the genai adapter's `convert_message` — the last gate
before the wire, and the only place that knows both the target model and the
provider encoding. Asserted by the oracle, not left to convention (design
principle 3: safe by construction, not by restriction).

## 5. The upstream genai contribution — a required outcome

Scoped against `genai 0.6.5`. Four changes, all small, all in
`jeremychone/rust-genai`. Per §2.3 the library already has a lossless,
model-tagged carrier (`ContentPart::Custom`), so **no new part type is
required** — which is what makes this tractable rather than a rewrite.

| # | Site | Today | Change |
|---|---|---|---|
| G1 | `anthropic/adapter_shared.rs:527` | `"thinking" => reasoning_content.push(item.x_take("thinking")?)` — text kept, `signature` discarded | Preserve the block losslessly. `redacted_thinking` already takes the `Custom` path at the `other_typ` arm; route `thinking` the same way, or add a signature field to the reasoning representation. |
| G2 | `anthropic/streamer.rs:152-155` | Captures `thinking` text; discards `signature_delta` | Same fix on the streaming path, so both parsers agree. |
| G3 | `anthropic/adapter_shared.rs:227` and `:271` | `ContentPart::Custom(_) => {}` (and `ReasoningContent`, `ThoughtSignature`) — empty arms in **both** the user- and assistant-content branches | Echo `Custom` parts verbatim when `model_iden.adapter_kind == Anthropic`. This is the actual round-trip. |
| G4 | `bedrock/converse.rs` | Same drop | Same fix. Anthropic signatures are cross-platform (Claude API / Bedrock / Vertex), so one modelling decision covers all three. |

**Two candidate shapes for G1, to be offered in the PR discussion rather than
picked unilaterally:**

- **(i) Route `thinking` to `ContentPart::Custom`.** Purely additive, no
  breaking change to a public enum, and reuses the path `redacted_thinking`
  already takes. Weakness: `Custom` semantically means *"we don't know what
  this is"*, which is a small lie for a block the parser demonstrably
  recognises.
- **(ii) Give the reasoning part a signature field.** Semantically honest.
  Weakness: `ContentPart::ReasoningContent(String)` is a public tuple variant,
  so this is a breaking change for downstream users.

Upstream will likely prefer (i); (ii) is the better model. Lead with (ii),
offer (i) as the compatible fallback.

**Sequencing so an external review cycle never blocks us.** G1–G4 are
independent of factor-q's own phases, so the PR opens at the *end of Phase 1*
— as soon as the ADR fixes the shape we are arguing for — and runs in
parallel with Phases 2–4. The maintainer's resolution ladder is the
contingency: if upstream stalls or rejects on design grounds, we fork and
point Cargo at it; migration is the third rung and not contemplated here.
**We are never blocked on the merge**, only on the shape decision, which is
ours.

**Anthropic-side verification of the round-trip is what
[`experiments/reasoning-round-trip/`](../../../experiments/reasoning-round-trip/)
already exists to measure**, and its `echo` arm is exactly the correct
protocol this contribution enables. Re-run it against a build carrying the
fork to confirm the `echo` arm is reachable through factor-q rather than only
through a hand-rolled probe.

## 6. Execution plan (PR-sized)

| Phase | Deliverable | Gates |
|---|---|---|
| **0** ✅ | **Modelling session** — ran 2026-08-25. Output: [ADR-0034](../../adrs/accepted/0034-reasoning-as-a-content-part.md). | done |
| **1** ✅ | **ADR accepted + doc amendments.** Move ADR-0034 `draft/` → `accepted/`, add its README row. Amend `inter-node-contracts-and-event-layers.md` §6/§7 (I3), and `event-schema.md` (`llm.response` shape, the annotation-barrier section, the `reasoning` key's row, and a v2→v3 changelog). **Documentation only — no code.** | ADR accepted, `check-links` green |
| **1b** | **Upstream genai PR opened** (§5, G1–G4). Runs in parallel from here. | PR open, shape argued |
| **2** | **Oracle first, then the type.** Build the judge before the thing it judges (lesson 10: *"thirteen reworks with zero behavioural regressions"*). Then `Message` becomes the turn-kind enum (§4.2) and the response chain takes `Vec<AssistantPart>`, with a no-op encoder. ~30 construction sites, of which 3 are tool-role; `Message.tool_call_id` has exactly one consumer. **`SCHEMA_VERSION` 2 → 3 lands here, with the shape it describes** — bumping it in Phase 1 would have events claiming v3 while still carrying v2 payloads. Behaviour unchanged; the diff is shape only. | golden net green, `just quality` + `just runtime-ci` |
| **3** | **OpenAI-compatible read + write.** `from_provider_response` reads `ChatResponse.reasoning_content`; `convert_message` emits `ContentPart::ReasoningContent`; `harness.rs:329` carries it into the replayed conversation. The cross-model strip (I2) lands here with its assertion. | I1, I2, I5 |
| **4** | **The operator surface and the transcript.** `TurnAction::Assistant` gains `TurnReasoning` (§4.6) — the reduction that keeps parts internal. `TranscriptEntry` follows, then the flag in both consumers: `fq invocation transcript` and the dashboard, including its `opaque — click to see raw` affordance. Golden files move here. | I7, golden updated |
| **5** | **Cost decomposition.** `reasoning_tokens` into `TokenUsage` from `completion_tokens_details`; `convert_usage` reads it; `total_cost` provably unchanged. | I4 |
| **6** | **Anthropic encoder** behind the resolved genai dependency (upstream or fork). Re-run the round-trip experiment's `echo` arm through factor-q. | I1 on Anthropic |

Phase 2 is the one to hold the line on: a pure shape change with an unchanged
golden net is the cheapest possible place to discover the model is wrong.

## 7. Test plan

Per the issue, the gap is **silent**, so tests assert on the outbound wire
shape and never on output quality.

- **Wire-shape round-trip (I1).** Fixture-backed two-turn test over an
  OpenAI-compatible provider: turn 1 returns `reasoning_content` and a tool
  call; assert the assistant message factor-q sends on turn 2 carries that
  `reasoning_content` back, byte-identical. This is *the* test the issue asks
  for. `test_support/mock_anthropic.rs` is the existing pattern; an
  OpenAI-shaped sibling is needed.
- **Cross-model strip (I2).** Property test: for any conversation carrying
  reasoning from model A, a request built for model B contains no reasoning
  part. Model-tie is a shape invariant, so it is property-checkable rather
  than example-checkable.
- **Invocation boundary (I3).** Assert `ConsumerView` exposes no reasoning
  part, for every payload variant that can carry one. Cheap now, and the only
  moment it *can* be written before #414 gives it a caller.
- **Cost decomposition (I4).** Assert `reasoning_tokens` is captured **and**
  that `total_cost` is bit-identical with and without the split. The second
  half is the one that matters.
- **State-blob compatibility.** `validate_state_blob` and the resume path
  (`invocation_resume.rs`) must reject a v2 blob cleanly rather than
  mis-parse it. Per STATUS.md pre-alpha, breaking in-flight resumes is
  acceptable; breaking them *silently* is not (I5).
- **Oracle + DST.** The reducer verification net (claims R1–R7) covers the
  replay path. A parts-shaped conversation must pass resume-equivalence and
  the crash DST unchanged — that is the strongest available evidence the shape
  change is behaviour-preserving.

## 8. Open questions

**All five are now resolved, and nothing blocks execution.** Three were settled
by the 2026-08-25 session, Q1 by the 2026-08-26 one, and Q5 is parked as work
that belongs to a different effort. What they settled is in the decisions block
and in ADR-0034.

- ~~**Q5 — Restate or withdraw the #424 gate?**~~ **Parked 2026-08-26.** The
  evidence in §2.2 is captured on
  [#424](https://github.com/bricef/factor-q/issues/424) and
  [#415](https://github.com/bricef/factor-q/issues/415); the block stands
  until someone chooses otherwise. **Nothing in this plan depends on it** —
  it governs when a separate cleanup epic may start, not what this work does.
  The cheap resolution is to read the `Coupling metrics` CI comment on Phase
  2's PR: that phase is a pure shape change, so a zero-delta coupling diff
  retires the gate's premise as a measured fact rather than an argued one.

**Split out rather than answered:**

- **Parallel tool results are not batched.** N results become N consecutive
  Anthropic user messages where the documented shape is one message with N
  `tool_result` blocks (§4.2). Pre-existing, independent of reasoning, and
  needs mock coverage for multi-`tool_use` responses before it can be fixed
  under test. §4.2's `ToolResults` variant makes the correct shape expressible;
  the behavioural fix is tracked as [#511](https://github.com/bricef/factor-q/issues/511).

**Closed by the session:**

- **Q2 — Transcript rendering default** → behind a flag (§4.3), in the CLI and
  the dashboard alike.
- **Q3 — How far parts propagate** → through the response chain, stopping at
  the operator boundary (§4.6, ADR-0034 D3). Neither of the two candidate
  extremes: not `Message`-only, and not all the way up.
- **Q4 — Is `Opaque` reachable?** → yes, and it is *required* to be
  representable. I7 makes naming it the point rather than speculative
  structure: a system that cannot say "opaque" can only say "absent", which is
  a lie. Note this inverts the lesson-2 reading in the original question —
  the variant earns its place on honesty grounds, not on room-to-grow grounds.
