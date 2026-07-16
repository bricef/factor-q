# Context Management

## Status

Draft (2026-07-05). Answers the open half of
[2026-07-05 project assessment](../../reviews/2026-07-05-project-assessment.md) §3
("context-window management… is unstarted with no design… it couples to
Memory, so its absence will be felt exactly when Memory ships"). This doc
defines the *architecture* — a harness-owned, pluggable seam with an explicit
trigger policy — and treats specific compaction/elision/summarization
techniques as swappable contents, because the techniques will evolve and
context windows will grow. The seam and the policy are the commitment; the
strategies behind them are not.

**Design-ahead** (this doc lives in `aspirational/`): the seam and policy are
proposed here, not yet built or recorded as an ADR. Per the
[design-docs convention](../README.md), when the seam is implemented the
decision is recorded as an ADR and this doc graduates to `committed/`.

## Context

A long-running agent accumulates conversation history — model turns, tool
calls, tool outputs — without bound. Today factor-q has the **null policy**:
the conversation grows in the reducer's `HarnessState.messages` and nothing
reduces it. That is fine for short invocations and fatal for the long-running,
autonomous agents the runtime exists to host. This is the mechanism that most
directly determines whether an agent survives a long task.

Two forces shape the design:

- **Techniques are unsettled and moving fast.** Production harnesses in
  2025–2026 (Claude Code, Cursor, Cline, Roo, Copilot, Aider, OpenHands,
  Letta/MemGPT, Manus, Cognition/Devin) use a shifting mix of summarization,
  truncation, tool-result eviction, retrieval, sub-agent offloading, and
  hierarchical paging — and the frontier (cache-aware compaction, RL-trained
  self-summarization, dedicated compression models) is advancing. We should not
  bet the runtime on one technique.
- **Context windows are growing.** As windows enlarge, trigger thresholds relax
  and aggressive summarization becomes less necessary, while unbounded-history
  retrieval remains relevant. A static policy would be wrong within a year.

Both point to the same conclusion: **make the mechanism pluggable behind a
defined seam, and commit only to the seam and the trigger policy.**

## Principles

1. **Context management is a harness concern, not an agent concern.** Consistent
   with the reducer boundary ([ADR-0014](../../adrs/accepted/0014-agent-harness-as-reducer.md))
   and the inter-agent boundary ([ADR-0007](../../adrs/accepted/0007-inter-agent-communication.md)):
   the agent expresses intent and produces content; the harness owns the wire —
   *and the window*. An agent never manages its own context. (It may *influence*
   it — e.g. mark a note as durable — but through typed harness operations, not
   by editing its own history.)

2. **A defined seam, with strategies plugged in behind it.** The runtime commits
   to the interface, not the implementation. A naive default ships first; better
   strategies replace it without touching the runtime.

3. **Trigger policy is separate from strategy.** *When* to reduce context (a
   policy) is orthogonal to *how* (a strategy). Both are pluggable, independently.

4. **Cache-safety is a hard constraint on every strategy, not an afterthought.**
   Long agent runs are only economical with prompt caching, and the naive
   compaction move destroys it. The seam makes the cache-safety invariants
   (below) a contract every strategy must honour.

5. **The CAS is the durable backbone.** Elision is *restorable*, not lossy:
   dropped content is addressable by CID in the content store
   ([ADR-0026](../../adrs/accepted/0026-event-log-system-of-record.md)), so a
   strategy can drop bytes from the window while keeping the record whole and
   retrievable. Conversation history and the event log are the same data shape
   (append-mostly, prefix-redundant) on the same substrate.

## The seam

Context reduction sits in the harness loop, between "assemble the conversation"
and "build the model request." Two pluggable dimensions, illustrative shape
(the exact traits firm up at build time; the *shape* is the commitment):

```text
trait ContextPolicy {
    // WHEN. Assess the current context; return a trigger, or None to proceed.
    fn assess(&self, ctx: &ContextView) -> Option<Trigger>;
}

trait ContextStrategy {
    // HOW. Reduce the context in response to a trigger, returning the new
    // conversation view plus a manifest of what was elided (→ CIDs) and
    // summarized — so the reduction is recorded, restorable, and observable.
    async fn reduce(&self, ctx: &ContextView, trigger: Trigger, cas: &Cas) -> Reduction;
}
```

- **`ContextView`** — the current conversation (messages, an estimated token
  count, the window size, the pinned/immutable prefix boundary, the current tool
  set, and a "consequential action pending?" hint the policy can act on).
- **`Trigger`** — why reduction fired (threshold crossed, semantic boundary
  reached, pre-consequential-action, budget pressure), carrying enough for the
  strategy to choose its approach.
- **`Reduction`** — the new conversation, plus a manifest mapping elided content
  → CID and recording any summary. The manifest is what makes elision restorable
  and the operation auditable.

Strategies **compose**: the shipped default is a `Layered` strategy that runs
sub-strategies in stages (evict, then summarize) — each stage itself a
`ContextStrategy`.

Every reduction **emits an event** (`context.compacted` / `context.elided`),
for three reasons: it is a cache-invalidation boundary the trace should record;
it feeds the M0 proxy metrics (compaction frequency and cost are part of "how
much human-equivalent work per token"); and it lets the verification net check
the operation (below).

## Cross-cutting invariants (the contract every strategy honours)

These are the reason the seam exists rather than a free function — they are easy
for a naive strategy to violate and expensive when it does.

1. **Append-don't-splice (cache-safety).** A prompt cache matches a prefix from
   the start of the request; rewriting history mid-stream invalidates the cache
   from the edit point on, forcing a full re-read at the cache-write premium.
   The rule the field converged on: **appended** summaries and **tail
   truncation** preserve the cache; **mid-stream replacement** destroys it.
   Structure the conversation as an **immutable prefix + appendable tail**;
   strategies operate only on the tail, and prefer append/truncate over splice.
   A compaction that must rewrite the prefix pays a one-time miss and then
   re-establishes a cache on the *smaller* new prefix — acceptable if rare, ruin
   if frequent. (Anthropic's own compaction generates the summary via a
   cache-*sharing* call — same prefix + an appended instruction — so producing
   the summary is a cache read; only the next turn rebuilds cache, on the short
   new history.)

2. **Stable tool set (cache-safety).** Tools are part of the cached prefix, so
   adding or removing one invalidates the entire conversation cache — as
   destructive as a history rewrite. Tool churn within a run is forbidden;
   changing tools mid-run either waits for a boundary or hides behind stable
   deferred stubs. factor-q already refreshes tools only *between* invocations
   ([ADR-0020](../../adrs/accepted/0020-mcp-notification-handling.md)), which is
   cache-friendly; the graph executor
   ([ADR-0007](../../adrs/accepted/0007-inter-agent-communication.md)) must
   preserve a stable tool set across a node's traversal.

3. **Restorability.** Elided content is not destroyed — it is addressable by CID
   in the CAS. A strategy that drops bytes keeps the pointer (Manus's rule:
   "drop the content, keep the URL"), so any elision is reversible and the full
   record survives for retrieval, audit, and the event log.

4. **Batch invalidations.** When an operation *must* invalidate the cache (a
   necessary eviction/compaction), it should free enough to be worth it — clear
   a lot, rarely, not a little, often (the `clear_at_least` pattern). Frequent
   small invalidations are the dominant cost failure mode.

5. **Observability.** Every reduction is an event, so the trace stays canonical
   and the cost/frequency are measurable.

## Trigger policy (the *when* dimension)

The default policy combines several signals; each is tunable, and the whole
policy is replaceable:

- **Token-fraction ceiling.** A hard "must reduce before the next call" bound
  (e.g. a configurable % of the window). This is a *ceiling*, not the primary
  trigger — the field's near-universal default, but a blunt one, because it
  fires at arbitrary points.
- **Semantic boundaries (preferred).** Reduce at a subtask/turn boundary, not
  mid-reasoning — so compaction never discards something the agent was about to
  use, and the boundary aligns with a natural cache reset.
- **Pre-consequential-action.** Before an irreversible or budget-spending step,
  optionally re-ground the relevant context (see the experimental strategy) — the
  highest-ROI moment to spend tokens on correctness.
- **Budget pressure.** Tie into the per-traversal budget
  ([ADR-0007](../../adrs/accepted/0007-inter-agent-communication.md)) so context
  cost participates in the same ceiling.

As windows grow, the token-fraction ceiling relaxes and the policy leans harder
on semantic boundaries and retrieval; the seam absorbs that shift without
runtime change.

## Strategy menu (the *how* dimension)

Strategies behind the seam, cheapest-and-most-settled first. This is a menu, not
a commitment; the **default** is marked, the rest are pluggable, and one is
**experimental**.

- **Tool-result eviction — the default's first stage, highest yield.** Stale
  tool outputs (file reads, command output) dominate the window and age fastest.
  Evict them first, keeping the CID (restorable). Anthropic reports ~−84% tokens
  from eviction alone on a 100-turn test; it is the cheapest large win, and the
  CAS makes it non-lossy.
- **Summarization — the default's second stage, only if still over budget.**
  Boundary-aligned, append-don't-splice, batched, structured (decisions / files
  touched / open tasks / current work, not prose), summary-call generated as a
  cache-sharing fork. This is the universal floor; it ships as the default.
- **Truncation / sliding window** — drop oldest tail turns behind the pinned
  prefix. Cheapest, no LLM cost, hard loss; a fallback where summarization is
  unavailable.
- **Retrieval over history** — keep a working set in-window; the full history
  lives in the CAS and relevant slices are retrieved on demand. This is the
  Memory layer ([ADR-0013](../../adrs/accepted/0013-memory-as-mcp-service.md));
  see Interactions.
- **Sub-agent / delegation offload** — not a compaction strategy but a
  structural one: a spawned child works in its own window and returns only a
  compact payload, so the parent's context never holds the subtask's reasoning.
  This is ADR-0007's spawn doing double duty; the coarse context boundary.
- **Structured external notes** — durable working state (a `NOTES.md`-equivalent,
  or Memory) written outside the window and re-read on demand, surviving
  compaction and interruption.
- **Selective re-grounding + verify-and-patch — EXPERIMENTAL, the novel bet.**
  Because the CAS holds the full uncompacted history, a strategy can compact
  *from originals* rather than summary-of-summary — turning drift from unbounded
  (compounding) into bounded (single-step). The naive form (re-read everything)
  is too expensive and exceeds the window; the viable form is *selective*:
  retrieve the relevant original chunks by CID and re-ground **that slice** (or
  *verify* the running summary's load-bearing claims against originals and
  *patch* only the drifted parts). Deploy it strategically — before a
  consequential action, or on a retrieval miss — where correctness ROI justifies
  the tokens. This appears to be genuinely non-standard practice (the field does
  summary-of-summary, or retrieval-of-originals for *recovery*, but not
  re-grounding for *drift control*), so it is flagged experimental and validated
  against real runs before it becomes a default. **It is one pluggable strategy,
  not foundational** — the seam exists precisely so bets like this can be tried
  and discarded without disturbing the runtime.

## Interactions

- **CAS ([ADR-0026](../../adrs/accepted/0026-event-log-system-of-record.md)).**
  The durable full history that makes elision restorable and re-grounding
  possible. Note the cost boundary: CAS *dedup* is a storage win, not an
  inference win — feeding originals back into a model call is full tokens. So
  re-grounding is a quality-for-tokens lever, spent selectively.
- **Memory ([ADR-0013](../../adrs/accepted/0013-memory-as-mcp-service.md)).** The
  retrieval strategy *is* Memory: retrieving relevant history slices, and
  retrieving durable notes, are the same operation. This is the coupling the
  assessment predicted — context management and Memory are co-designed, not
  sequential.
- **Spawn ([ADR-0007](../../adrs/accepted/0007-inter-agent-communication.md)).**
  Delegation is the coarse-grained context boundary; a well-structured graph
  bounds each node's window by construction. Caching note: a fresh subagent is a
  cold cache; a fork inherits the parent's — a knob for whether a child re-reads
  context.
- **Prompt caching.** The cache-marker placement (already implemented on the
  Anthropic path) must land breakpoints on the immutable-prefix boundary the
  strategies preserve, so the stable prefix caches across turns and only the
  churning tail is uncached.
- **M0 proxies ([M0 plan](../../plans/active/2026-07-05-m0-close-the-loop.md)).**
  Compaction frequency and cost are part of cost-per-accepted-change; the
  `context.compacted` events feed the measurement.

## Verification

Context management is harness-scoped, so it inherits the runtime's verification
ethos ([reducer verification](../../plans/closed/2026-07-05-reducer-verification.md)).
Invariants worth a net: every elision is restorable (the manifest round-trips
against the CAS); reduction preserves the immutable prefix (cache-safety is
mechanically checkable, not just reviewed); the post-reduction trace stays
canonical; and — for re-grounding — a re-grounded summary is at least as
faithful as the summary-of-summary it replaced (a differential check against
originals). The seam makes these testable at the trait boundary without a live
model.

## Current state and build order

- **Today:** the null policy — history grows unbounded in the reducer state blob.
- **First:** the seam itself (`ContextPolicy` × `ContextStrategy`, the
  `ContextView`/`Reduction` types, the `context.compacted` event, the
  cache-safety invariants enforced at the boundary), plus the **default
  strategy**: two-stage evict-then-summarize, boundary-aligned,
  append-don't-splice, CAS-backed restorable elision.
- **Then:** retrieval (with Memory) and the experimental re-grounding strategy,
  each behind the seam, validated against real dogfood runs before promotion.

## Open questions

- **The trigger-policy default's exact signals and thresholds** — settle against
  real runs, and expect them to move as windows grow.
- **The re-grounding mechanism** — retrieve-and-re-ground vs verify-and-patch,
  and how to select the load-bearing originals (a retrieval-quality problem).
  Experimental until measured.
- **Where the pinned/immutable-prefix boundary sits** — system prompt + which
  durable context — and how it interacts with the graph executor's per-node
  context.
- **The `context.compacted` event schema** — enough to record the manifest
  (elided CIDs, summary reference) without bloating the trail.

## References

- [2026-07-05 project assessment](../../reviews/2026-07-05-project-assessment.md) §3 — the
  critique this answers.
- [ADR-0014](../../adrs/accepted/0014-agent-harness-as-reducer.md) — the
  harness-owns-the-loop boundary this extends.
- [ADR-0026](../../adrs/accepted/0026-event-log-system-of-record.md) — the CAS /
  durable history backbone.
- [ADR-0013](../../adrs/accepted/0013-memory-as-mcp-service.md) — Memory, the
  retrieval layer.
- [ADR-0007](../../adrs/accepted/0007-inter-agent-communication.md) — spawn as
  the coarse context boundary; per-traversal budget.
- [M0 plan](../../plans/active/2026-07-05-m0-close-the-loop.md) — where
  compaction cost shows up in the proxies.
