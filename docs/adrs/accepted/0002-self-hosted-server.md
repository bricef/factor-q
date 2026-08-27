# ADR-0002: Self-Hosted Server, Not Local CLI Tool

## Status

Accepted

Implementation: complete — `fqd` is the persistent daemon and `fq` a thin
client that connects to it (#498, ADR-0031); the client API layer is the
authenticated edge over the typed operation registry (ADR-0006).

## Context

factor-q could run as a foreground CLI application (like Claude Code or OpenCode) or as a persistent server process. The target use cases — software development, systems operations, regulatory analysis — all require agents that run continuously and react to events without a human at the terminal.

## Decision

factor-q runs as a persistent, self-hosted server process. CLI, TUI, and other interfaces are clients that connect to the runtime.

## Rationale

All three target use cases require persistence that outlasts a user session. Agents responding to operational alerts or monitoring regulatory changes must keep running when no human is connected. The runtime must survive disconnections, restarts of client interfaces, and periods of no human interaction.

## Consequences

- The system needs a daemon/service architecture from day one
- An API layer is required for clients to connect (unnumbered at the time;
  since answered by [ADR-0006](0006-registry-first-api.md) and
  [ADR-0031](0031-daemon-cli-split.md))
- State must be durably persisted — in-memory-only state is not acceptable
- Deployment becomes a server operations concern (process management, logging, monitoring)
