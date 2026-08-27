# ADR-0004: Cost Controls From Day One

## Status

Accepted

Implementation: partial — per-call cost tracking, cost events, per-agent and
per-invocation budgets with a halting ceiling, and `fq costs` / the dashboard
cost views all ship (#216, #218, #230, #484). Two halves are not built: the
delegation enforcement mechanism below (sub-agent spawning does not exist),
and the per-origin breakdown in the operator surface — the typed origin is
carried on the events but no reader renders it (see § Cost attribution).

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

When agents spawn sub-agents (see [§ Decided spawn
semantics](../../design/aspirational/agent-orchestration-tools.md)), the cost model extends with one
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
  `docs/design/aspirational/agent-orchestration-tools.md`). Optimistic; a
  child can be starved mid-flight.

Both satisfy the invariant above.

Still open for the spawn case, which remains unbuilt. For the *graph*
case, [ADR-0007](./0007-inter-agent-communication.md) (2026-07-05) has
since taken a position: a per-traversal budget with an ε cost floor.

## Cost attribution (added 2026-05-28)

Cost-bearing events carry a typed **origin** so spend is traceable
to its cause, not just its total. The origin distinguishes at
least the agent's own turn, a sampling request from a named MCP
server, and an elicitation answer for a named MCP server (see
[ADR-0017](./0017-mcp-human-in-the-loop.md)), and is extensible as
new spend sources appear (e.g. sub-agent edges). The intent is that a
shared budget never becomes an opaque blob: when budget is consumed,
where it went and on whose behalf should be visible.

**Built as of 2026-08-26:** the typed origin (`LlmCallOrigin::{AgentTurn,
Sampling, Elicitation}`) is stamped on cost-bearing events, so the
breakdown is recoverable from the event stream. The operator-facing half
is not built — `fq costs` and the cost reports break spend down by agent,
invocation and model, never by origin, so separating a server's sampling
spend from the agent's own reasoning today means reading raw event JSON.
[ADR-0017](./0017-mcp-human-in-the-loop.md) §4 restates the same
unrealised promise.
