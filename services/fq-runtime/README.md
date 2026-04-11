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

## Deployment

### Container image

A multi-stage `Dockerfile` lives alongside this README. It builds a
release binary with the official Rust image and copies it into a
[distroless](https://github.com/GoogleContainerTools/distroless) runtime
stage (`gcr.io/distroless/cc-debian12:nonroot`) for minimal surface
area and a non-root user by default.

```sh
# From this directory
docker build -t factor-q/fq-runtime .
```

The image is aspirational for now — the runtime doesn't yet have enough
functionality to warrant a real deploy — but it establishes the shape
the runtime is authored against.

### Environment variables

Every runtime path is configurable via an environment variable. The
defaults baked into the container image are conventional Linux paths
that operators can mount volumes at; on a fresh host they all fall
through to safe locations.

| Variable          | CLI flag          | Default (container)          | Notes                                     |
|-------------------|-------------------|------------------------------|-------------------------------------------|
| `FQ_CONFIG`       | `--config`        | `/etc/factor-q/fq.toml`       | Optional — defaults apply if unset        |
| `FQ_AGENTS_DIR`   | `--agents-dir`    | `/var/lib/factor-q/agents`    | Mount a volume with your agent definitions |
| `FQ_CACHE_DIR`    | `--cache-dir`     | `/var/cache/factor-q`         | Pricing cache and other runtime caches    |
| `FQ_NATS_URL`     | `--nats-url`      | `nats://nats:4222`            | Points at a NATS service on the same network |
| `RUST_LOG`        | (n/a)             | `info`                        | Log level / filter                        |

Precedence remains CLI flag > env var > config file > default. On a
host without any of these set, factor-q falls back to:
- `agents/` in cwd
- `$XDG_CACHE_HOME/factor-q` → `$HOME/.cache/factor-q` → `/tmp/factor-q`
- `nats://localhost:4222`

### Mounted volumes

The image declares volumes at `/var/lib/factor-q` (agent definitions,
skills, future state) and `/var/cache/factor-q` (pricing JSON and other
caches). Mount persistent volumes at these paths for anything that
needs to survive container restarts.

### Example compose stanza

```yaml
services:
  nats:
    image: nats:latest
    command: ["--config", "/etc/nats/nats.conf"]
    volumes:
      - ./nats/nats.conf:/etc/nats/nats.conf:ro
      - nats-data:/data/nats

  fq-runtime:
    image: factor-q/fq-runtime
    depends_on:
      - nats
    volumes:
      - ./agents:/var/lib/factor-q/agents:ro
      - fq-cache:/var/cache/factor-q
    environment:
      ANTHROPIC_API_KEY: ${ANTHROPIC_API_KEY}

volumes:
  nats-data:
  fq-cache:
```

## Status

Phase 1 foundation — scaffolding in place, implementation in progress. Nothing production-ready yet.
