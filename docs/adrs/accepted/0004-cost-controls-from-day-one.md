# ADR-0004: Cost Controls From Day One

## Status
Accepted

## Context
Autonomous agents call LLMs and consume tokens without direct human oversight for each invocation. A misconfigured agent, an infinite loop, or an unexpectedly large task can generate significant costs before anyone notices.

## Decision
Cost tracking and budget limits are built into the core runtime from the start, not added later.

## Rationale
Autonomous agents spending money without human oversight is a first-order risk. This must be a design constraint from the start, not a feature bolted on after an incident. Retrofitting cost controls into a system that wasn't designed for them leads to gaps and inconsistencies.

## Consequences
- The agent executor tracks token usage and cost for every LLM call
- Cost data is emitted as events on the bus
- Per-agent and aggregate budget limits are enforced, with hard ceilings that halt execution
- The cost model must account for different pricing across models and providers

## Budget conservation under delegation (added 2026-05-28)

When agents spawn sub-agents (see `docs/plans/backlog.md` →
Agent concurrency primitives), the cost model extends with one
invariant, stated here as the authoritative rule:

> **A parent's budget bounds the total spend of its entire
> subtree.** The sum of a parent's own spend plus all of its
> descendants' spend never exceeds the parent's budget. Applied
> recursively, no spawn tree can spend more than its root
> agent's budget.

This makes recursive fan-out safe: an `AgentMap` or a chain of
spawns cannot escalate cost invisibly. It is the budget half of
the broader capability-attenuation rule — a child's capabilities
*and* budget are both subsets of its parent's.

The *enforcement mechanism* is an open implementation choice,
deferred until sub-agent spawning is built:

- **Reservation / escrow** — deduct the child's budget from the
  parent's remaining at spawn; return the unspent remainder on
  completion. Guarantees a spawned child its full budget;
  pessimistic (idle reservations block siblings).
- **Aggregate-and-halt** — children draw from a shared pool; stop
  spawning when the running total hits the cap (the "Inheritance
  rule" already described in
  `docs/design/agent-orchestration-tools.md`). Optimistic; a
  child can be starved mid-flight.

Both satisfy the invariant above.

## Cost attribution (added 2026-05-28)

Cost-bearing events carry a typed **origin** so spend is traceable
to its cause, not just its total. The origin distinguishes at
least the agent's own turn, a sampling request from a named MCP
server, and an elicitation answer for a named MCP server (see
[ADR-0017](./0017-mcp-human-in-the-loop.md)), and is extensible as
new spend sources appear (e.g. sub-agent edges). `fq costs` and
the invocation trace break spend down by origin — so a shared
budget never becomes an opaque blob; when budget is consumed,
where it went and on whose behalf is always visible.
