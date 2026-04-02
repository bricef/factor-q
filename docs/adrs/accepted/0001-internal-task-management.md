# ADR-0001: Internal Task Management

## Status
Accepted

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
