# ADR-0002: Self-Hosted Server, Not Local CLI Tool

## Status
Accepted

## Context
factor-q could run as a foreground CLI application (like Claude Code or OpenCode) or as a persistent server process. The target use cases — software development, systems operations, regulatory analysis — all require agents that run continuously and react to events without a human at the terminal.

## Decision
factor-q runs as a persistent, self-hosted server process. CLI, TUI, and other interfaces are clients that connect to the runtime.

## Rationale
All three target use cases require persistence that outlasts a user session. Agents responding to operational alerts or monitoring regulatory changes must keep running when no human is connected. The runtime must survive disconnections, restarts of client interfaces, and periods of no human interaction.

## Consequences
- The system needs a daemon/service architecture from day one
- An API layer is required for clients to connect (see ADR on API design)
- State must be durably persisted — in-memory-only state is not acceptable
- Deployment becomes a server operations concern (process management, logging, monitoring)
