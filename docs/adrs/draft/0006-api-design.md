# ADR-0006: Runtime API Design

## Status
Draft

## Context
factor-q runs as a persistent server (ADR-0002). All clients — CLI, TUI, web interfaces, external integrations — connect through an API. The API is the sole interface to the runtime; no client has privileged access that bypasses it.

## Options

### Option A: REST/HTTP
Well-understood, wide tooling support, easy to debug with curl. Poor fit for real-time streaming without augmentation (SSE or polling).

### Option B: gRPC
Strong typing via protobuf, efficient binary protocol, native streaming support. Harder to debug manually, requires code generation, less accessible for quick integrations.

### Option C: WebSocket
Native bidirectional streaming. Good for real-time event tailing and interactive sessions. Less natural for CRUD operations. Connection management adds complexity.

### Option D: Combination
E.g. REST for CRUD and administration, WebSocket or SSE for real-time streaming. Pragmatic but increases API surface area.

## Decision
Not yet taken.

## Considerations
- The API must support real-time streaming (watching agent activity, event tailing)
- CRUD operations on agents, tasks, and graphs
- Event log queries with filtering
- Administrative controls (start, stop, pause agents and the runtime)
- Multiple client types will connect — the API should be accessible enough for a CLI to consume without heavy client libraries
- Integration with external systems (webhooks in, notifications out) may influence the choice
