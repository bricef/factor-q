# ADR-0007: Inter-Agent Communication Patterns

## Status
Draft

## Context
Agents in a factor-q graph need to communicate — passing work, sharing results, coordinating on tasks. The event bus decouples agents, but the system must define what communication patterns are supported, as these constrain what kinds of agent graphs are expressible.

## Messaging Patterns

### Fire-and-forget
Agent emits an event and continues without waiting. Simplest pattern. Good for notifications and logging. Makes coordination and error handling difficult.

### Request/response
Agent emits an event and waits for a corresponding reply event. Enables tight collaboration. Couples agents temporally (one blocks waiting for the other).

### Streaming
One agent produces a stream of intermediate results that another consumes progressively. Useful for long-running analysis or incremental processing.

### Broadcast
One event consumed by many agents. Useful for system-wide notifications, configuration changes, or fan-out patterns.

### Pub/sub with topic filtering
Agents subscribe to event topics or patterns. More targeted than broadcast, more flexible than point-to-point.

## Invocation Semantics

### Spawn
Parent creates a child agent and continues running. The parent retains its context and can coordinate multiple children, optionally awaiting their results. Enables supervisor/worker patterns and parallel delegation.

### Exec
Parent is replaced by the child agent. The parent's context is handed over and the parent does not resume. Useful for pipeline stages where one agent transforms work and passes it forward without the overhead of keeping the parent alive.

## Decision
Not yet taken.

## Considerations
- The patterns chosen determine what agent topologies are possible
- Spawn vs exec determines whether graphs are trees of supervisors or linear pipelines — both are needed
- The right answer is likely multiple patterns, but which are first-class primitives and which are composed?
- Error propagation differs across patterns (who handles a failure in fire-and-forget vs request/response?)
- Backpressure: what happens when a consumer can't keep up with a producer?
