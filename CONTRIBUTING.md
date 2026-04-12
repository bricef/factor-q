# Contributing to factor-q

## Development setup

**Prerequisites:**
- Rust toolchain (edition 2024 — install via [rustup](https://rustup.rs/))
- Docker and Docker Compose (for NATS)
- [just](https://github.com/casey/just) (task runner)
- An Anthropic API key for smoke tests (optional for unit tests)

**First-time setup:**

```sh
# Clone the repo
git clone https://github.com/bricef/factor-q.git && cd factor-q

# Start NATS with JetStream
just infra-up

# Build
just build

# Run the unit tests (no API key needed, but NATS must be running)
FQ_NATS_URL=nats://localhost:4222 just test

# Run the CLI
just fq --help
```

## Repository layout

```
factor-q/
├── services/fq-runtime/       Rust workspace (CLI + runtime library + tools)
│   ├── crates/fq-cli/           fq binary
│   ├── crates/fq-runtime/       core library
│   └── crates/fq-tools/         built-in tools and sandbox
├── infrastructure/             docker-compose + NATS config
├── agents/examples/            sample agent definitions
├── tests/smoke/                end-to-end smoke tests (bash)
├── docs/
│   ├── adrs/                   architectural decision records
│   ├── design/                 technical design specs
│   └── plans/                  phase plans and backlog
├── VISION.md                   what and why
├── ARCHITECTURE.md             subsystems and concerns
└── CONTRIBUTING.md             this file
```

## Running tests

factor-q has three test tiers, each with different prerequisites
and coverage:

### Tier 1: Unit tests

Fast, run in-process. Most don't need NATS, but the integration
tests (bus round-trips, executor event sequences, dispatcher,
projection consumer) do.

```sh
# Without NATS — runs ~110 tests, skips integration tests
just test

# With NATS — runs all ~155 tests
FQ_NATS_URL=nats://localhost:4222 just test
```

If a test says "skipping: FQ_NATS_URL not set", it's gated on
NATS. Start NATS with `just infra-up`.

### Tier 2: Smoke tests

End-to-end against a real LLM (Anthropic). Exercises the full
stack: agent loading, executor, tool-call loops, event bus,
projection, and the NATS-triggered dispatch path. Each test
creates its own temp directory and uses a unique agent id.

```sh
# Requires ANTHROPIC_API_KEY and a running NATS
just smoke
```

Costs roughly $0.005-0.01 per run. Tests are in `tests/smoke/smoke.sh`.

### Tier 3: Containerised shell sandbox tests

The shell tool spawns child processes. Even though the test
battery uses only safe commands (`echo`, `true`, `sleep`, etc.),
we provide a disposable container runner that mounts the workspace
read-only and disables networking. Use this when iterating on the
shell tool's sandbox logic.

```sh
just test-shell-sandbox
```

This builds a `rust:1.85-slim` Docker image with the cargo
registry pre-populated, then runs the shell tests offline inside
the container. Takes ~30s on the first run (image build), ~5s on
subsequent runs.

### Running everything

```sh
just infra-up
FQ_NATS_URL=nats://localhost:4222 just test    # unit + integration
just smoke                                       # end-to-end (needs API key)
just test-shell-sandbox                          # containerised sandbox
```

## Code conventions

- **Rust edition 2024**, formatted with `cargo fmt`, linted with
  `cargo clippy -- -D warnings`. Run `just ci` to check both plus
  tests in one shot.
- **No comments explaining what** — only why. Well-named
  identifiers speak for themselves.
- **Module-level doc comments** (`//!`) on every `.rs` file
  explaining the module's purpose and threat model where applicable.
- **Tests live next to the code** (`#[cfg(test)] mod tests`) rather
  than in a separate `tests/` crate. Integration tests that need
  NATS are gated on `FQ_NATS_URL`.
- **Commits** follow conventional style: imperative mood, short
  first line, body explains the "why" and links to ADRs where
  relevant.

## Architecture and design

Start with these docs to understand the system:

1. [VISION.md](VISION.md) — what factor-q is and why it exists
2. [ARCHITECTURE.md](ARCHITECTURE.md) — subsystems and concerns
3. [docs/design/event-schema.md](docs/design/event-schema.md) — the
   event model that everything else is built around
4. [docs/adrs/](docs/adrs/) — every significant design decision
   with rationale

The [phase 1 closing summary](docs/plans/closed/2026-04-02-phase-1-foundation.md)
has a detailed inventory of what shipped and what was deferred.

## Adding a new tool

1. Create `services/fq-runtime/crates/fq-tools/src/builtin/<name>.rs`
2. Implement `Tool` for your struct (see `file_read.rs` for a
   minimal example, `shell.rs` for a complex one)
3. Register it in `ToolRegistry::with_builtins()` in
   `services/fq-runtime/crates/fq-runtime/src/tools.rs`
4. Add sandbox tests proving the tool respects sandbox boundaries
5. If the tool spawns processes, add tests to the containerised
   runner (`just test-shell-sandbox`)
