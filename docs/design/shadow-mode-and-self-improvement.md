# Shadow Mode and Self-Improvement

## Overview

Factor-q's path to Q200 depends on continuous, automated
self-improvement of agent workflows, prompts, and strategies.
This document describes two tightly coupled mechanisms:

1. **Shadow mode** — a safe evaluation environment where proposed
   workflow changes run against real traffic without affecting
   outputs
2. **The self-improvement loop** — an automated cycle where agents
   propose improvements, shadow mode evaluates them, and validated
   changes are promoted to production

## Shadow mode

### The problem

Agent workflows are non-deterministic. Traditional testing
(unit tests, synthetic benchmarks) cannot fully predict how a
changed prompt or workflow will behave against real-world inputs.
A/B testing compares two versions on *different* inputs, which
conflates input variation with workflow quality. What we need is
a way to compare two versions on the *same* input, under real
conditions, without risk.

### How it works

When a workflow change is proposed, the system enters a
time-boxed shadow evaluation window:

1. A trigger arrives and is dispatched to the **live agent**
   (current workflow) as normal.
2. The same trigger is simultaneously dispatched to a **shadow
   agent** running the candidate workflow.
3. The live agent's output reaches the user/external system. The
   shadow agent's output is captured but discarded.
4. Both invocations produce full event streams (tool calls, LLM
   turns, costs, final output).
5. An **evaluator** compares the paired event streams and produces
   a verdict.

### Side-effect isolation via the network proxy

The critical challenge is that agents have side effects — they
send Slack messages, create PRs, write to databases. Shadow
agents must not produce real side effects.

The network proxy (described in ADR-0010) is the control point.
All outbound traffic from containerised agents passes through it.
The proxy operates in different modes per agent:

- **Live agent:** record and forward. Every outbound request and
  response is captured.
- **Shadow agent:** record and replay. When the shadow agent makes
  an outbound call, the proxy serves the recorded response from
  the live agent's corresponding call. The shadow agent sees the
  same external world as the live agent without making real calls.

This has several consequences:

- No duplicate calls to external systems (no doubled API costs,
  no duplicate Slack messages)
- The shadow agent's behaviour is evaluated against the same
  external responses the live agent saw, isolating the comparison
  to workflow quality alone
- No external state divergence between live and shadow

For stdio-transport MCP servers (which make outbound HTTP calls
to the services they wrap), the proxy captures traffic at the
container's network boundary regardless of the MCP transport
mechanism. The event bus also captures all tool-call-level
interactions via `tool.call` / `tool.result` events.

### Evaluation windows

Shadow mode is not always-on. It runs in time-boxed windows
triggered by a proposed change:

- **Duration:** N invocations or T hours, whichever comes first
- **Budget:** shadow invocations have an independent budget cap
  (LLM token cost is the main expense; external API costs are
  near-zero due to replay)
- **Scope:** shadow can run on all traffic or a sampled subset
- **Teardown:** shadow infrastructure is torn down after the
  window closes

This avoids unbounded cost and the state divergence problem that
would accumulate with permanent shadow execution.

### Memory and state divergence

If the live agent stores something in persistent memory during
invocation N, the shadow agent does not see that stored memory on
invocation N+1 (its memory namespace is isolated). Over time, the
two agents diverge in internal state even though they see the same
external world.

Short evaluation windows mitigate this. For longer evaluations,
the shadow's memory could be periodically snapshotted from the
live agent's state, but this adds complexity. The pragmatic
starting point is short windows (tens of invocations) where
divergence is minimal.

## The self-improvement loop

### The cycle

```
Propose  -->  Evaluate (shadow mode)  -->  Promote or Reject
   ^                                            |
   |____________________________________________|
                  (learn from outcome)
```

1. **Propose:** An improvement agent analyses historical event
   streams and proposes a change — a modified prompt, a different
   tool selection strategy, a restructured workflow, a new agent
   in the graph.

2. **Evaluate:** The proposed change is deployed as a shadow
   workflow. Real traffic is dual-routed for the evaluation
   window. The evaluator compares paired outputs.

3. **Promote or reject:** If the evaluator determines the shadow
   version is better (or at least not worse), the change is
   promoted. If it's worse, it's rejected. Either way, the
   outcome feeds back into the proposal agent's context for future
   iterations.

### The evaluator

The evaluator is itself an agent. Given a pair of (live, shadow)
event streams for the same input, it assesses:

**Mechanically measurable dimensions:**
- Cost (token usage, external API calls)
- Latency (time to completion)
- Tool call efficiency (fewer calls for the same result)
- Error rate (tool failures, sandbox violations)

**Judgment-requiring dimensions:**
- Output quality (does the shadow output address the original
  request as well or better?)
- Reasoning quality (is the shadow agent's chain of thought
  more coherent?)
- Appropriateness of actions (did the shadow agent take
  unnecessary or risky actions?)

The judgment dimensions require an LLM-as-judge pattern: the
evaluator feeds the original request, both outputs, and both
event traces into a frontier model and asks for a comparative
assessment. This is the most sophisticated component of the
loop — the evaluator must be more capable than the agents it
evaluates, or at least capable of recognising quality differences.

### Promotion path

Changes don't go from shadow to full production in one step:

1. **Shadow** — candidate runs alongside live, output discarded,
   evaluator compares
2. **Canary** — candidate handles a small percentage of real
   traffic (e.g., low-stakes triggers), output delivered
3. **Promote** — candidate becomes the live workflow
4. **Regression shadow** — the *old* workflow continues running
   as a shadow after promotion, providing continuous regression
   detection

Step 4 is important: keeping the old version as a shadow after
promoting the new one means regressions are detected
automatically, not just during the pre-promotion evaluation
window.

### Versioning

The self-improvement loop requires versioned agent definitions,
prompts, and workflows. Git is the natural backing store. Each
proposed change is a branch or commit. Promotion is a merge.
Rollback is a revert.

Agents should have access to their own version history (via tools)
so they can understand what changed and why. This is opt-in
context, not loaded by default — version history is useful for
debugging and learning but would waste context window space in
normal operation.

## Relationship to other subsystems

- **Event bus:** provides the raw material (full invocation
  traces) that both the improvement agent and evaluator consume
- **Network proxy (ADR-0010):** provides the side-effect
  isolation mechanism (record/replay) that makes shadow mode safe
- **Container isolation (ADR-0010):** shadow agents run in their
  own containers with isolated filesystems and memory
- **Cost controls:** shadow invocations have independent budgets;
  the proxy eliminates most external API cost duplication
- **Memory MCP service (phase 2):** shadow agents use an isolated
  memory namespace to prevent state contamination

## Open questions

- **Evaluator bootstrapping:** the evaluator agent needs to exist
  before the self-improvement loop can run. Its own prompts and
  evaluation criteria are initially human-authored. Can the
  evaluator eventually improve itself, or is that a
  self-referential problem that requires a human-in-the-loop
  permanently?
- **Improvement scope:** should the loop be constrained to prompt
  changes initially, or should it also propose structural changes
  (adding/removing agents from the graph, changing tool sets)?
  Structural changes are higher-risk and harder to evaluate.
- **Evaluation sample size:** how many paired invocations are
  needed for a statistically meaningful comparison? This depends
  on the variance of the workload and the magnitude of the
  proposed change.
- **Multi-step workflows:** if a workflow spans multiple agents in
  sequence, where does the shadow fork happen? At the entry point
  (shadow the entire graph) or at the changed agent only? The
  former is cleaner but more expensive.
