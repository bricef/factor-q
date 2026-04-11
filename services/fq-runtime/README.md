# fq-runtime

The core factor-q runtime service. Provides the agent executor, event bus integration, and CLI.

## Structure

A Cargo workspace with three crates:

| Crate | Purpose |
|---|---|
| [`fq-cli`](crates/fq-cli/) | The `fq` binary — command parsing and dispatch |
| [`fq-runtime`](crates/fq-runtime/) | Core library — agent definition loader, config, event schema, executor |
| [`fq-tools`](crates/fq-tools/) | Built-in tool implementations and the `Tool` trait |

```
services/fq-runtime/
├── Cargo.toml              # workspace manifest
├── justfile                # build tasks
└── crates/
    ├── fq-cli/
    │   └── src/main.rs     # clap command structure
    ├── fq-runtime/
    │   └── src/
    │       ├── lib.rs
    │       ├── config.rs   # runtime configuration
    │       ├── events.rs   # NATS subjects and event schema
    │       └── agent/
    │           └── definition.rs  # Markdown + YAML frontmatter parser
    └── fq-tools/
        └── src/
            ├── lib.rs
            └── tool.rs     # Tool trait and error types
```

## Prerequisites

- Rust toolchain (edition 2024 — see `rust-toolchain.toml` if present)
- [just](https://github.com/casey/just) for running tasks
- A running NATS server for integration (use `just infra-up` from the repo root)

## Development

All tasks run via `just`. Run `just` or `just --list` to see available recipes.

```sh
# Build
just build

# Run tests
just test

# Type-check without building
just check

# Format and lint
just fmt
just lint

# All quality checks (format, lint, test)
just ci

# Run the CLI
just fq -- --help
just fq -- agent list
```

## The CLI

The `fq` binary is the primary interface during phase 1. All commands are currently stubs — they parse arguments but do not yet perform their intended actions.

```
fq init                        # initialise a new factor-q project
fq run                         # run the runtime in the foreground
fq trigger <agent> [payload]   # manually trigger an agent
fq agent list                  # list registered agent definitions
fq agent validate <path>       # validate an agent definition
fq events tail [--subject]     # tail the event stream
fq events query [--agent]      # query event history
fq costs [--agent] [--since]   # show cost breakdown
```

## Design references

This service implements decisions from the repository-level documentation:

- [Project vision](../../VISION.md)
- [Architecture](../../ARCHITECTURE.md)
- [Phase 1 plan](../../docs/plans/active/2026-04-02-phase-1-foundation.md)
- Relevant ADRs:
  - [0005 — Agent definition format](../../docs/adrs/accepted/0005-agent-definition-format.md)
  - [0009 — Rust as host language](../../docs/adrs/accepted/0009-technology-choices.md)
  - [0011 — NATS + JetStream event bus](../../docs/adrs/accepted/0011-event-bus-and-persistence.md)

## Status

Phase 1 foundation — scaffolding in place, implementation in progress. Nothing production-ready yet.
