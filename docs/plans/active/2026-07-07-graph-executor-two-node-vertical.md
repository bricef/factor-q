# Graph executor — the two-node vertical (`propose → review`)

**Status:** draft (2026-07-07). The concrete Q20 first vertical for the graph
executor decided in
[ADR-0007](../../adrs/accepted/0007-inter-agent-communication.md) ("build the
declared two-node graph first; verify the executor to the reducer's bar").
Grounded in the signatures cornerstone
([signatures as primitive](../../design/aspirational/signatures-and-optimization-hierarchy.md),
[schemas required](../../design/aspirational/inter-node-contracts-and-event-layers.md))
and the node/edge/budget model whiteboarded 2026-07-07. This spec is the
buildable artifact; it deliberately implements only the model's *default*
subset and defers the rest (see Scope).

## Why this vertical

It is the smallest composition that is still *real*: two signature-typed
nodes, one edge, one traversal budget — enough to verify the executor
rigorously, yet a genuine workflow (propose a change, review it) we have
been doing by hand all session, so we can judge whether the executor does it
*right*, not merely that it runs. It is also the first two rungs of the
separation-of-duties pipeline (reviewer distinct from proposer), so it is on
the path to "bot-ify everything", not a detour — and it is expected to
surface the identity need concretely (a reviewer's verdict is only
trustworthy if the reviewer is a distinct, grant-scoped identity;
[identity design](../../design/aspirational/agent-identity-and-attestation.md)).

## The model (the executor implements the default subset)

Per the signatures cornerstone, the primitive is the **signature**, not the
agent:

- A **signature** is `(typed input schema, typed output schema, intent)` — a
  declarative unit of work.
- A **node** binds a signature to an **implementation strategy**. For Q20 the
  binding is *static* and the strategy is a single agent; late binding and
  other strategies (function, MCP tool, subgraph) are a Q100+ tunable behind
  the same signature — which is what makes an agent and a graph
  interchangeable.
- An **edge** is typed, directed, and carries `{ from, to, guard, bind }`.
  The **guard** is a predicate on `from`'s `(outcome, output)`; the **bind**
  maps `from`'s typed output into `to`'s typed input. Defaults: guard =
  "`from` completed", bind = pass the output through. (The same guard concept
  later covers conditional routing and error routing — not exercised here.)
- **Schemas are required**, never optional — opaque content is a typed
  wrapper (`RawText`, or `Promoted<T>` for size-aware content), never an
  untyped node.
- **Budget** is one **shared traversal pool** (USD) plus a **hop ceiling**.
  Each hop decrements the pool by `max(actual_cost, ε)` and the hop counter
  by 1; `ε > 0` guarantees termination even for cyclic graphs. Per-node spend
  is **attributed** (recorded) despite the shared pool — that attribution is
  the credit-assignment signal the future graph-level optimizer consumes.
- **Fail-fast:** an unguarded node failure fires no downstream edge, so the
  branch starves and the traversal ends (a clean, attributed terminal, not
  quiet-wrongness).
- A **single agent is a one-node graph** — the executor is the one dispatch
  path; `trigger → traversal` subsumes `trigger → invocation`.

## The two signatures

Schema notation below is illustrative (the concrete type language is the
signatures work; see
[type & signature discovery](../../design/aspirational/type-and-signature-discovery.md)).
The *shapes* are the spec.

### Signature: `propose`

**Intent:** "Given a scoped change task for the factor-q repository,
implement the smallest change that does it, validate it, and open a
reviewable pull request."

```
input  { task: RawText }                       // the scoped change description
output {
  pr_number: u64,
  pr_url:    String,
  branch:    String,
  summary:   RawText,                           // what changed and why
  checks:    { ci_passed: bool },
}
```

**Implementation (Q20, static binding):** the existing `m0-loop` agent —
grants: `file_read`, `file_write`, `shell`; env `PATH`, `GH_TOKEN`.

### Signature: `review`

**Intent:** "Given a pull request, produce a typed review verdict with
structured findings, without modifying the code."

```
input  { pr_number: u64, pr_url: String }
output {
  verdict:  enum { approve, request_changes, reject },
  findings: [ { file: String, line: u64?, severity: enum { blocking, major, minor, nit }, summary: RawText } ],
  summary:  RawText,
}
```

**Implementation (Q20, static binding):** a new **read-only** `m0-reviewer`
agent — grants: `file_read`, `shell` (for `gh` to fetch the diff), env
`PATH`, `GH_TOKEN`. **No `file_write`.** That missing write grant is the
first real separation-of-duties instance *and* the thing that makes the
verdict's trust depend on distinct identity — the gap this vertical is meant
to expose.

## The edge and the traversal

```
edge  propose → review
  guard: propose.outcome == Completed            // default (fail-fast if propose fails)
  bind:  { pr_number: propose.output.pr_number,
           pr_url:    propose.output.pr_url }     // typed field map into review.input

traversal propose-and-review
  budget:   12.00        // USD, shared pool
  max_hops: 8            // hop ceiling
  epsilon:  0.001        // USD, ε cost floor (termination guarantee)
  attribution: per-node spend recorded (propose $x, review $y) — optimizer signal
```

**What the verdict is for (Q20):** it is reported to the human, who still
gates the merge. The *typed* verdict is exactly what a future `merge` node's
guarded edge would consume (`guard: review.output.verdict == approve`), so
this vertical produces the merger's input contract now; the `merge` node and
its guarded edge are the next rung, not this one.

## Verification (to the reducer's bar)

The executor is verified the way the reducer was
([Principle 6](../../design/committed/design-principles.md): a verified,
swappable seam) — the traversal is **event-sourced and deterministic**, so it
replays like an invocation
([ADR-0026](../../adrs/accepted/0026-event-log-system-of-record.md)). Pin:

- node dispatch and typed I/O binding (the `bind` produces a valid
  `review.input` from `propose.output`);
- edge guard/bind evaluation;
- budget accounting — shared-pool decrement, per-node attribution, the `ε`
  floor, and the hop ceiling — including a synthetic cyclic graph that must
  terminate on budget;
- fail-fast: a failed `propose` yields a traversal terminal with `review`
  never dispatched;
- the degenerate case: a one-node graph behaves identically to today's plain
  invocation;
- replay: re-running a recorded traversal reproduces the same outcome.

Plus a live two-node integration on the dogfood repo (`m0-loop` proposes,
`m0-reviewer` reviews) as the real exercise.

## Scope — Q20 only; explicitly deferred

- **No optimizer.** Node-level and graph-level optimization are Q100/Q200 and
  node-before-graph (signatures doc, Decision 6). We *record* the attribution
  signal; we do not act on it.
- **Static binding only.** Late/dynamic binding and non-agent strategies
  (function, MCP tool, subgraph) are the Q100+ tunable behind the signature.
- **Sequential only.** No parallel nodes — which sidesteps the
  shared-workspace / worktree concurrency problem entirely (that bites only
  when parallel nodes land).
- **Default guard/bind only.** No conditional or error-routing edges yet; the
  merger's `verdict == approve` guard is the next rung.
- **Identity surfaced, not closed.** The read-only reviewer makes the
  distinct-identity need concrete; closing it is the identity work, driven by
  this finding rather than built speculatively.

## What it will teach (the loop is a gap-discovery engine)

Expected findings, to be handled as they surface rather than pre-designed:
the identity need (reviewer-verdict trust); the ergonomics of the typed
`bind` (is field-mapping the right authoring surface?); whether an
event-sourced traversal replays as cleanly as an invocation; and — once
parallel nodes arrive — worktree concurrency for co-writing nodes.
