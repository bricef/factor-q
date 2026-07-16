# ADR-0007: Inter-Agent Communication

## Status

Accepted (2026-07-05). Graduated from the April 2026 draft, which enumerated
agent-level messaging patterns and spawn/exec semantics but took no decision.
Extends [ADR-0014](0014-agent-harness-as-reducer.md) (harness-as-reducer) to the
multi-agent case; uses [ADR-0012](0012-graph-definition-format.md) (the graph
definition format) as one of its two authoring surfaces; enforces its boundary
through [ADR-0016](0016-typed-operations-no-free-form-apis.md) (typed
operations); and adds spawn to the capability-grant lineage of
[ADR-0017](0017-mcp-human-in-the-loop.md) /
[ADR-0018](0018-mcp-server-initiated-execution.md). Arising from the
[2026-07-05 project assessment](../../reviews/2026-07-05-project-assessment.md)
§7, which flagged this as the frontier the project's "multi-agent" identity
depends on.

## Context

factor-q's identity is a multi-agent runtime, but multi-agent is unbuilt: there
is no agent-to-agent handoff primitive and no graph executor (ADR-0012's format
is a spec without an engine). The April draft framed the problem as a menu of
*agent-level* messaging patterns (fire-and-forget, request/response, streaming,
broadcast, pub/sub) plus spawn/exec, and decided nothing. This ADR decides —
and reframes: the messaging menu is the wrong altitude, because **agents do not
communicate over the transport; the harness does.**

## Decision

1. **Agents never touch the transport; the harness owns the wire.** This is the
   reducer principle ([ADR-0014](0014-agent-harness-as-reducer.md)) one level
   up: the agent expresses *intent* (spawn this; here is a payload), the harness
   performs all I/O. Agents do not publish to NATS, address subjects, or know the
   transport exists. The boundary is made **structural** by the three-layer wire
   format, enforced by types ([ADR-0016](0016-typed-operations-no-free-form-apis.md)):
   - **Envelope** — harness scope only (event/trace/parent ids, schema version,
     cost). Agents cannot address it.
   - **Metadata** — agent → harness (an agent's typed requests to the harness).
   - **Payload** — agent ↔ agent, or agent → output (the content agents
     exchange).

2. **One execution substrate, two authoring surfaces.** The **graph executor**
   is the sole runtime primitive. Two ways author topology over it:
   **declared graphs** (the static [ADR-0012](0012-graph-definition-format.md)
   YAML, executed by the harness) and **spawn** (a grant-gated capability by
   which an agent constructs topology *at runtime* — parent-child,
   sibling-sibling, optionally awaited). Spawn and join are **syntactic sugar**:
   the harness desugars them into executor operations (dynamic subgraph
   construction). The authoring surfaces are orthogonal (a grant vs a YAML
   artifact); the execution substrate is **one engine**, so budget, ordering,
   trace-stitching, and failure routing have a single implementation, not two.

3. **The graph subsumes the messaging menu.** Every pattern the draft listed
   reduces to (graph edge vs spawn) × (await vs not): fire-and-forget = a
   non-awaiting edge; request/response = spawn-with-await; pipeline (the draft's
   "exec") = a linear non-awaiting edge; fan-out = spawn of siblings. So none are
   first-class primitives. In particular, **an agent is deliberately *not* a
   `TriggerSource`** — it would be a redundant, less-controlled second path that
   also leaks the transport to agents. Triggers stay
   operator/schedule/external-facing.

4. **Nodes are heterogeneous; being composed does not require the spawn
   capability.** Because the graph composes from *outside*, a node may be an LLM
   agent, a capability-less agent, or a deterministic script, and needs no spawn
   grant to *be* composed. Spawn (composing from *inside*) is granted only where
   an agent genuinely needs runtime delegation. This is least privilege by
   construction, and it lets the right worker run each node — a deterministic
   step need not, and should not, be an LLM agent.

5. **Budget is enforced per traversal, at the executor, at runtime.** Spawn
   builds topology at runtime, so no static per-node analysis can bound a
   traversal; enforcement is a runtime-debited ceiling over the whole traversal,
   unifying declared and spawned. Two backstops of different kinds:
   - A **non-zero per-node cost floor (ε):** every node execution — LLM or
     deterministic — debits at least ε, reflecting the real CPU/wall-clock cost
     of running anything. This makes the budget a **complete** termination
     guarantee: cycles are permitted, every hop costs > 0, so budget exhaustion
     bounds any cycle — including cycles of purely deterministic nodes. The
     budget is the **semantic** bound (you stop because you spent your
     allowance).
   - A separate **hop ceiling** (the graph-scope analogue of `HOST_STEP_BUDGET`)
     is the **mechanical** backstop against a runaway traversal, independent of
     cost.

6. **Error propagation follows the two surfaces, and they nest.** A **spawn** is
   a supervision relationship: the child's outcome — success or failure —
   returns to the parent as a reducer `CapabilityResult` on its next step, the
   same channel tool and sampling results already use
   ([ADR-0018](0018-mcp-server-initiated-execution.md)). A **graph edge** is
   inter-node routing: the executor owns a node's outcome and routes it per the
   graph. The two compose — a graph node is an agent that may itself spawn — so
   spawn is intra-node supervision nested inside the executor's inter-node
   routing.

7. **The executor is verified to the reducer's bar.** This boundary concentrates
   spawn, graph execution, per-traversal budget, grants, trace-stitching, and
   failure routing in the harness. That is the right place for it (control,
   security, testability), and it raises the verification bar: the oracle-and-DST
   net built for the reducer
   ([reducer verification](../../plans/closed/2026-07-05-reducer-verification.md))
   extends to graph scope — a traversal produces a canonical, stitched event
   trace; the per-traversal budget holds under a crash mid-graph. The two-layer
   (reducer + graph) proof is a correctness guarantee unusual in this space, and
   a deliberate goal.

8. **Build order: the substrate first.** The first thing stood up is a
   **declared two-node graph** (e.g. implementer → reviewer) on a minimal graph
   executor — no spawn, no dynamic construction. Because nodes may be
   deterministic, the executor's invariants (ordering, budget debit, trace
   stitching, failure routing) are testable with **zero LLM calls**. Spawn is
   layered on afterward as sugar over the proven substrate.

## Rationale

- **Two primitives are complete.** Every draft pattern reduces to (graph edge vs
  spawn) × (await vs not); the graph generalizes to any topology and spawn adds
  runtime construction. Nothing else is needed as a primitive.
- **The boundary is the reducer model, kept.** Agents-as-pure-intent is what
  already makes the reducer portable, testable, and secure; extending it upward
  keeps those properties and avoids handing agents the transport — a large
  attack surface.
- **One engine, not two.** Orthogonal *surfaces* over one *substrate* is what
  stops budget and failure fragmenting into two divergent models — the exact
  "stress-tested together" concern the assessment raised.
- **Heterogeneous least-privilege nodes** match work to worker and keep spawn — a
  powerful capability — rare.

## Consequences

- **A graph executor becomes a committed build**, with the reducer's
  verification treatment. The envelope's reserved `trace_id` / parent fields,
  unused today, become load-bearing for stitching multi-invocation traces.
- **Spawn is a new capability in the grant model** (lineage
  [ADR-0017](0017-mcp-human-in-the-loop.md) /
  [ADR-0018](0018-mcp-server-initiated-execution.md)) with granular
  sub-capabilities (whom an agent may spawn, how deep). Access control is
  exercised across the agent boundary for the first time.
- **Budget accounting moves up a level** — per-invocation → per-traversal — and
  gains the ε floor and hop ceiling. Per-invocation budgets remain the per-node
  accounting the traversal sums.
- **`TriggerSource` stays as-is** (manual/subject/schedule); this ADR
  deliberately does not add an agent variant.

## Open questions (deferred to the executor design)

1. **Node-failure semantics** — how a graph handles a failed node (failure
   edges, retry policy, compensation) is unspecified, and the
   [ADR-0012](0012-graph-definition-format.md) format has to grow it (failure is
   today expressible as neither node nor edge). The sharpest remaining decision.
2. **Executor mechanics and the spawn grammar** — the concurrency/join model,
   scheduling, and the exact typed spawn/await surface — settled when the
   substrate is built.
3. **ε sizing** — small enough not to distort real cost accounting, which makes
   budget/ε a loose hop bound and is why the mechanical hop ceiling is retained
   independently.
