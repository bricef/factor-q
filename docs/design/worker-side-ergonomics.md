# Worker-side Ergonomics

A companion to [`agent-orchestration-tools.md`](agent-orchestration-tools.md). Where that doc is about the primitives an orchestrator uses to coordinate a fleet, this one is about the primitives an agent uses *inside a node* — while it's actually doing the work.

## Why a separate doc

The orchestration-tools spec was unintentionally one-sided: it solves problems a node has *when it is spawning other nodes*, and says little about the problems a node has *while it is executing*. Those are genuinely different concerns and they want separate design reasoning.

From an orchestrator's perspective, a worker is a node that receives a prompt and returns a result. That abstraction is clean and composable. From inside the worker, it's the whole universe: context management, tool-call decisions, uncertainty, errors, interruption, and the relationship with the next turn of itself. None of that is addressed by better `AgentMap` semantics.

This doc catalogues the primitives that would make worker-side execution ergonomic. It is not a tier-ordered spec — it is grouped by concern, because the concerns are orthogonal to each other and the "frequency of use" dimension is much harder to judge for worker-side primitives than for orchestrator-side ones.

## Guiding principles

**An agent should be able to reason about and influence its own execution, not just produce results.** The current model treats the agent as a pure function from prompt to output; worker-side ergonomics acknowledges that agents are stateful processes with budgets, uncertainty, interruption risk, and the ability to self-govern — and gives them the tools to act on that.

**No confabulation where data exists.** This principle — that anything the runtime tracks about an agent's execution must be exposed through a tool rather than left to inference — is the single largest driver of the surfaces in this doc. `SelfInspect`, `ToolVersions`, `LoadCheckpoint`, and the structured error surface all exist in its service. Canonical statement in [`design-principles.md`](design-principles.md#2-no-confabulation-where-data-exists).

---

## Self-introspection

Primitives that let the agent see its own state. Today the agent is flying on vibes about everything from remaining budget to permitted tools.

### `SelfInspect`

```
SelfInspect({
  include?: ("budget" | "context" | "tools" | "model" | "lineage")[],   // default: all
})
  → {
      budget?: {
        tokens_used, tokens_remaining,
        cost_used, cost_remaining,
        duration_used_ms, duration_remaining_ms,
      },
      context?: {
        tokens_in_use,
        context_window_size,
        messages_in_history,
        oldest_turn_at: timestamp,
      },
      tools?: {
        allowed: [{ name, version, capabilities }],
        sandbox: { fs_read, fs_write, exec_cwd, env, network },
      },
      model?: {
        id, provider, tier_alias?,   // e.g. "claude-sonnet-4-6", "anthropic", "default"
        capabilities: string[],      // e.g. ["extended-thinking", "prompt-caching-v2"]
      },
      lineage?: {
        handle_id,
        parent_id,
        depth,
        spawn_tree_summary,
        graph_instance_id?,          // if running inside an AgentGraph
      },
    }
```

**Semantics.** Read-only snapshot of the agent's own execution context. No side effects, no token cost beyond the small response.

**Why it matters.** The obvious argument is "the agent would make better decisions with this information" — true, but secondary. The stronger argument is that **without introspection, the agent's self-reports are confabulated, not absent.**

Asked about its own state, an agent without introspection does not fail loudly. It produces plausible-sounding answers inferred from visible conversation length and pattern-matched from training data — phrases like "I'm approaching context limits" or "tokens remaining are getting low" emitted at moments that *look* like they warrant them, with no underlying signal. Those answers pass sniff tests often enough that nobody notices they're invented. The agent appears to know what it's doing right up until the moment it runs out of context mid-sentence, calls a tool it no longer has access to, or blows a budget it reported as half-remaining.

This is worse than silent absence of information. Silent absence would prompt the agent to ask, the orchestrator to instrument, or the user to notice. Confabulated self-reports actively mislead everyone downstream — the agent itself on later turns, the orchestrator reasoning about its remaining capacity, any self-improvement loop learning from traces, and the user asking a reasonable question.

The cost of fixing this is near zero. The harness already tracks every field `SelfInspect` would surface — token counts for budget enforcement, sandbox configuration for tool dispatch, model identity for request routing, spawn lineage for the event bus. Exposing it to the agent is a read path over existing state, not new instrumentation. **The information is already being collected; withholding it is what costs.**

Pacing, spawn decisions, and budget requests are what the agent *does with* introspection once it has it. They are not the reason introspection must exist.

### `ToolVersions`

```
ToolVersions({ names?: string[] })
  → [{ name, version, fingerprint, last_changed_at, changelog_ref?: ArtifactRef }]
```

**Semantics.** Reports the versions and capability fingerprints of tools available to the agent. The `fingerprint` is a hash over the tool's input/output schema and documented behaviour — stable across patch releases, changes when semantics change.

**Why it matters.** Tool behaviour drifts. A self-improvement loop (or a long-lived agent) needs to notice when the ground has moved under it — a regression or a semantic change that invalidates a learned pattern. A surfaced fingerprint lets the agent detect this deterministically rather than by failed pattern-match.

---

## Self-governance

Primitives that let the agent constrain, checkpoint, and validate its own execution. This is the section with the most leverage for safety and cost: every one of these tools prevents a class of error the agent would otherwise only catch after the fact.

### Checkpoints

The most load-bearing primitive in this doc. Design question: given factor-q's reducer/event-sourced semantics, don't we get checkpointing for free?

**What is free.** The event bus records every tool call, every LLM round-trip, every decision. If an execution dies mid-flight, the runtime can replay events from the bus and reconstruct the exact state at the last committed event boundary. For crash recovery within a single execution, no agent-side primitive is needed — the runtime can handle it automatically.

**What is not free, and why an explicit checkpoint primitive still earns its place.**

1. **Granularity.** Replay only reaches event boundaries. Partial reasoning between tool calls is not persisted. For most purposes that's fine.
2. **Cost.** Replaying a 40-turn execution to resume at turn 41 means re-reading 40 turns of context into a fresh agent instance. For long-running work this is expensive in tokens even if deterministic. A curated checkpoint summary is orders of magnitude cheaper to hand a resumed agent.
3. **Cross-session resumption.** If the resumption happens hours or days later — in a different orchestrator session, possibly on a different model — raw replay reconstructs the original agent's frame, which may not be what the successor needs. A checkpoint is *the agent's own summary of its understanding*, which survives translation to a different context better than raw history.
4. **Handoff.** Sometimes the successor is a different agent type entirely ("I've analysed, now the synthesiser takes over"). Replay forces the successor into the original agent's frame. A checkpoint is a deliberate handoff artefact that expresses progress in terms the successor can use.

So the answer is: crash recovery within a single execution is free from event sourcing; **semantic resumption, cheap resumption, and handoff want an explicit, agent-controlled surface**. The two mechanisms coexist — checkpoints are just another event type on the bus, consulted by the runtime under the same reducer machinery.

#### Tool surface

```
Checkpoint({
  summary: string,                     // the agent's own words: what has been established, what remains
  structured_state?: any,              // optional machine-readable state, conforms to agent's own state schema if declared
  next_steps?: string,                 // what the agent was about to do next
  artifacts?: ArtifactRef[],           // refs to anything produced so far that a successor would need
  confidence?: number,                 // 0..1 — how complete/reliable this summary is
  supersedes_prior?: boolean,          // default true — later checkpoints replace earlier ones in resumption
})
  → { checkpoint_id, event_ref }
```

```
LoadCheckpoint({
  handle_id?: HandleRef,               // defaults to current execution
  strategy: "latest" | "at_event" | "id",
  at_event?: EventRef,
  id?: string,
})
  → { checkpoint_record | null }
```

**Semantics.**

- **Emission.** `Checkpoint` writes an event to the bus like any other event. It is non-blocking and cheap. Agents are encouraged to checkpoint at natural progress boundaries — not on every turn, but whenever a meaningful milestone has been reached.
- **Resumption policy** is a runtime decision, surfaced as configuration on the handle. Default: if a checkpoint exists, the resumed agent is given the latest checkpoint's summary/state as its starting context plus access to `LoadCheckpoint` if it wants to pull further detail. If no checkpoint exists, fall back to event replay (possibly truncated).
- **`LoadCheckpoint`** lets the resumed agent pull its own checkpoint record explicitly — useful if the runtime handed it a minimal summary but it wants the full structured state, or wants to walk backwards through earlier checkpoints.
- **Supersession.** By default, newer checkpoints supersede older ones. Setting `supersedes_prior: false` keeps the earlier checkpoint addressable (useful for workflows that branch and may want to resume from an earlier fork).

**Interaction with durable handles.** A durable handle identifies *a piece of running work*. A checkpoint is *a savepoint within that work*. Handles are external references (who is doing what); checkpoints are internal structure (how far has it got). A handle with no checkpoints resumes via replay; a handle with checkpoints resumes via the latest one. The handle store records a pointer to the latest checkpoint event as denormalised state for fast lookup on resumption.

**Interaction with reducer semantics.** Checkpoints are events. They participate in the same append-only log as every other event. Projections over the event stream can include checkpoint materialisation for free. Replay semantics are preserved: replaying up to a checkpoint event's timestamp reconstructs the full state; starting from a checkpoint event skips the preceding history. Both modes are valid and the runtime picks based on the resumption policy.

**Why an agent-facing surface matters.** The runtime *could* automatically emit checkpoints at regular intervals (every N turns, or on budget-fraction boundaries). That's a useful default and probably worth having. But it cannot produce *semantic* checkpoints — the agent's own account of what it has established — because that requires the agent's understanding. The exposed surface is how the agent contributes its knowledge to its own future-self's resumption.

### `RequireApproval`

Declarative approval gates — the agent can pause its own execution at a structured gate before doing something irreversible, rather than typing "are you sure?" in prose and hoping the user reads it.

```
RequireApproval({
  action: string,                      // short label, e.g. "git push --force", "rm -rf /srv/data"
  blast_radius: "local" | "shared" | "external",
  explanation: string,                 // why this action, why now, what happens if it's wrong
  reversible: boolean,
  alternatives?: string[],             // other options the approver could choose
  timeout_s?: number,                  // if unanswered, defaults to "deny"
  notify?: SinkSpec,                   // how to reach the approver (defaults to harness's configured channel)
})
  → {
      decision: "approved" | "denied" | "alternative",
      chosen_alternative?: string,
      approver: string,
      message?: string,
    }
```

**Semantics.** Blocking call. Execution pauses at a structured gate; the runtime surfaces the request through the configured approval channel (CLI prompt, webhook, Slack message, etc.). On `"denied"` or timeout, the agent must choose a different path or fail cleanly.

**Why a structured gate beats prose asking.** The prose form "are you sure?" is unreliable in every direction: the user may not see it, may misread what's being asked, and cannot record a standing policy (`"this user always approves local-scope writes"`). A structured gate is inspectable by the harness, recordable as a policy, batchable for review, and auditable after the fact.

### `ValidatePlan`

```
ValidatePlan({
  plan: string | structured_plan,
  validator_type: string,              // agent type to run as critic
  criteria?: string[],                 // specific concerns to check for
  model?: ModelSelector,               // likely a cheap model
  timeout_s?: number,
})
  → {
      verdict: "proceed" | "revise" | "abort",
      issues: [{ severity, description, suggestion? }],
      revised_plan?: string,
  }
```

**Semantics.** Cheap, early critique of a plan before the agent starts executing it. Effectively a lightweight `AgentLoop` where the agent is the worker, a cheaper model is the reviewer, and the result is a verdict the agent uses to decide whether to proceed, revise, or escalate.

**Why it earns its place.** Plan validation is the cheapest possible error-correction mechanism — catching a mistake in the plan before any code runs, any command executes, any token is spent on execution. Today the agent has no standard way to reach for this short of manually orchestrating an `AgentSpawn`; making it a single tool call means agents will actually use it.

---

## Lightweight help

The orchestration primitives (`AgentSpawn`, `AgentMap`) are heavyweight — new context, new budget, new trace. Small questions deserve small ceremony.

### `Consult`

```
Consult({
  model: ModelSelector,
  question: string,
  context?: string,                    // optional background, small
  max_tokens?: number,                 // default low — this is for short answers
  output_schema?: JSONSchema,
})
  → { answer: string | parsed_json, tokens_used }
```

**Semantics.** One-shot prompt to a model, no conversation, no tool access, no history, no trace beyond a brief event. Returns a single response and discards the conversation. Cheaper than `AgentSpawn` because it skips agent loading, sandbox setup, and event overhead.

**Why it matters.** "Is this regex right?", "summarise these 20 lines", "which of these three names is clearer?" are questions where paying full spawn overhead is absurd. Today the agent either pollutes its own context doing it itself, or skips the check entirely. `Consult` gives a cheap, low-ceremony channel for small questions — the kind of thing a human developer does dozens of times a day by glancing at docs or asking a colleague.

**Relationship to `AgentSpawn`.** `Consult` is the floor of the spectrum; `AgentSpawn` is the ceiling. If the question needs tool access, multi-turn reasoning, or any context accumulation, use `AgentSpawn`. If it's a one-shot question with a one-shot answer, use `Consult`.

### `ShareContext`

```
ShareContext({
  include: ("turn_ids" | "artifact_refs" | "summaries")[],
  turn_ids?: string[],                 // specific prior turns from the caller's history
  artifact_refs?: ArtifactRef[],
  summaries?: string[],                // curated natural-language summaries
})
  → { context_bundle_ref }
```

Used as a field on `AgentSpawn`/`AgentMap` children: `inherited_context?: ContextBundleRef`.

**Semantics.** Structured, selective sharing of context fragments from the parent to a child — specific turns, specific artifacts, or specific summaries, not the whole history. The child receives the bundle as a named context block it can reference.

**Why a middle path matters.** Today context sharing is all-or-nothing: either the parent restates everything in the child's prompt (expensive, error-prone), or the child flies blind. Neither is right for "here are three specific facts you need, the rest is irrelevant." `ShareContext` makes the middle path expressible.

---

## Structured errors

The single biggest quality-of-life improvement for a worker agent. Everything else in this doc is incremental; this is categorical.

### `ToolError` as a first-class value

Tool failures currently return strings. Pattern-matching on strings is brittle, language-dependent, and makes retry/escalation logic fragile.

Proposal: every tool returns either a success value or a `ToolError`:

```
ToolError = {
  kind: "timeout" | "permission_denied" | "rate_limited" | "validation_failed"
      | "not_found" | "conflict" | "dependency_unavailable" | "budget_exceeded"
      | "internal_error" | "user_cancelled" | "sandbox_violation",
  tool: string,
  message: string,                     // human-readable
  details?: any,                       // tool-specific structured detail
  retryable: boolean,
  retry_after_s?: number,              // if retryable and tool knows when
  caused_by?: ToolError,               // error chain
}
```

**Semantics.** Tools are responsible for categorising their failures. The runtime enforces the schema at the tool boundary. Agents can branch on `kind` cleanly, check `retryable` before deciding whether to retry, and respect `retry_after_s` when present. Error chains (`caused_by`) preserve diagnostic context without the agent having to parse prose.

**Why this is categorical.** Every retry loop, every escalation decision, every error-handling branch the agent writes today is parsing strings. Structured errors collapse a whole class of brittle code into clean branching on a discriminated union. Self-improvement loops can learn patterns against error kinds, not fragile substring matches.

### Self-validation of outputs

`output_schema` on `AgentSpawn` lets the orchestrator validate a child's output. The agent should be able to validate *its own* outputs symmetrically:

```
ValidateSelf({ output: any, schema: JSONSchema })
  → { valid: true } | { valid: false, errors: [{ path, message }] }
```

**Semantics.** Pure schema validation, no model call. Lets the agent catch "I'm about to return malformed JSON" before committing. Trivial to implement (the runtime already has the JSON-schema validator for `output_schema`); useful as an explicit tool so agents can self-correct.

---

## Structured uncertainty

Not a dedicated tool — a convention plus a schema fragment that can be attached to any output.

```
Uncertainty = {
  confidence: number,                  // 0..1
  reasons?: string[],                  // why the confidence is what it is
  cheap_check?: string,                // a resolvable query that would raise/lower confidence
  fallback?: string,                   // what to do if confidence is insufficient downstream
}
```

Agents are encouraged — possibly required, for certain output types — to attach `Uncertainty` to decisions, classifications, and summaries. Downstream consumers (including the agent's future self) can branch on confidence explicitly.

**Why it matters.** Today uncertainty lives in prose ("I think X but I'm not sure") and is lost at every tool boundary. A structured channel preserves the signal across composition boundaries, enables routing decisions ("if confidence < 0.7, send to `ValidatePlan` before executing"), and lets self-improvement loops notice when an agent's confidence and outcome diverge.

---

## Open questions

- **Automatic checkpointing.** Should the runtime emit checkpoints automatically on budget-fraction boundaries (e.g. every 25% of budget consumed), as a safety net for agents that don't checkpoint themselves? Probably yes, but it interacts with `supersedes_prior` and needs thinking through.
- **`Consult` trace visibility.** If `Consult` is trace-free, it undermines audit. If it's fully traced, it's not really cheaper than `AgentSpawn`. Likely answer: minimal envelope event (who consulted, what model, token count, no body) so it's auditable at the cost level without accumulating trace bulk.
- **`RequireApproval` policy layer.** Running approvals through the bus lets the harness layer in approver-side policies (standing approvals, approval delegation, approval batching). Worth designing once real flows exist, not now.
- **Self-modification.** An agent noticing a tool is broken (via `ToolVersions` or repeated `ToolError`) might want to disable that tool for the rest of its execution. Is that safe? Is it a tool? Open.
- **Context window pressure signals.** `SelfInspect` surfaces tokens in use, but doesn't proactively warn when the agent is approaching its context ceiling. A soft-warning event on the bus at 80% / 95% / 99% of the window would let agents trigger their own compaction or handoff.

---

## Relationship to the orchestration-tools spec

| Concern | Addressed by |
|---|---|
| How a fleet coordinates | [`agent-orchestration-tools.md`](agent-orchestration-tools.md) |
| How an individual agent executes | This doc |
| Durable identification of running work (external) | Durable handles — orchestration-tools |
| Savepoints within running work (internal) | Checkpoints — this doc |
| Result delivery across the runtime boundary | Result sinks — orchestration-tools |
| Reasoning about own execution state | `SelfInspect` — this doc |
| Error propagation across the composition | Composition-level error semantics (future) — orchestration-tools |
| Error handling within a node | Structured `ToolError` — this doc |

The two docs should stay separate going forward. The concerns genuinely differ, and conflating them has already shown up as one-sided coverage in the existing spec.
