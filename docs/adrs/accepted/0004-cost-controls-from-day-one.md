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
