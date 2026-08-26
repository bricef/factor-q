# ADR-0001: Internal Task Management

## Status

Accepted

Implementation: partial — factor-q owns its own work engine (invocations,
triggers, the control-plane dispatcher, scheduled triggers via `fq-cron`),
and no external task manager is integrated. The dependency/fan-out/fan-in
half of the task model is not built: it belongs to the graph executor, whose
plan is held (#414, ADR-0007).

## Context

factor-q needs task tracking with dependency management, fan-out/fan-in, and scheduling. An existing project (Taskflow) already provides task management, raising the question of whether to integrate with it or build task management into factor-q.

## Decision

factor-q owns its task engine internally rather than integrating with an external task management system.

## Rationale

The orchestrator must understand task dependencies, ordering, fan-out, and fan-in to do its job. Splitting this across two systems would create sync problems and split state. factor-q's task model requires parallel execution patterns (fan-out/fan-in) that Taskflow was not designed for. The orchestrator and the task engine are the same concern.

## Consequences

- Taskflow becomes a separate, independent project — its lessons and code may be ported where applicable
- factor-q takes on the full complexity of task lifecycle management
- Task state, agent state, and event history are co-located in one system — simpler debugging and auditing
