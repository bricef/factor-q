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

Only the runtime suite needs NATS; the store and dashboard suites are
hermetic.

```sh
# The runtime suite without NATS — skips the integration tests
just test-runtime

# The runtime suite with NATS — runs the integration tests too
FQ_NATS_URL=nats://localhost:4222 just test-runtime

# Every Rust suite (runtime + store + dashboard)
FQ_NATS_URL=nats://localhost:4222 just test

# Filter, by forwarding cargo args to one suite
just test-runtime -p fq-runtime --lib agent::definition
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

### Tier 3: Containerised sandbox tests

The exec tool spawns child processes. Even though the test
battery uses only safe commands (`echo`, `true`, `sleep`, etc.),
we provide a disposable container runner that mounts the workspace
read-only and disables networking. Use this when iterating on the
exec tool's sandbox logic.

```sh
just test-shell-sandbox
```

This builds a `rust:1.85-slim` Docker image with the cargo
registry pre-populated, then runs the exec tests offline inside
the container. Takes ~30s on the first run (image build), ~5s on
subsequent runs.

### Running everything

```sh
just infra-up
FQ_NATS_URL=nats://localhost:4222 just test    # all three Rust suites
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

## Design sessions

Design work that affects LLM-facing surfaces — tool shapes, agent
interfaces, orchestration primitives, worker-side affordances — is
conducted as co-design sessions between a human collaborator and an
LLM, with both participants' contributions treated as primary
material. This is a deliberate practice, grounded in the principle
that [LLMs are first-class users and a source of requirements](docs/design/committed/design-principles.md#1-llms-are-first-class-users-and-a-source-of-requirements).
The shape of the orchestration-tools and worker-side-ergonomics
specs was materially informed by an LLM surfacing friction in its
own execution that a human working top-down would not have found.

### When to run a co-design session

A session is warranted when any of the following apply:

- Specifying a new tool surface an LLM will call
- Revising how agents express uncertainty, errors, or self-state
- Designing composition or orchestration primitives
- Debugging why agents repeatedly misuse or misunderstand a surface
- Exploring what new capabilities would unlock, as distinct from
  fixing existing ones

Routine implementation, bug fixes, infrastructure plumbing, and
documentation polish do not require a session, though nothing stops
a collaborator from using one if it feels useful.

### What a good session looks like

- The LLM is asked about its felt experience of the current
  surface, not just asked to review a spec that has already been
  drafted.
- Disagreements are worked through on their merits — neither
  participant defers by default.
- The session's output is the design document itself (or revisions
  to one), not a transcript, summary, or checklist of follow-ups.
- Open questions the session cannot resolve are captured in the
  doc rather than forgotten.

### Preserving the practice

This collaboration is a first-class part of how factor-q is built,
not a transitional phase. Future collaborators — human or
otherwise — are expected to continue it. When design output ages
and needs revision, revisions follow the same model.

## Architecture and design

Start with these docs to understand the system:

1. [VISION.md](VISION.md) — what factor-q is and why it exists
2. [ARCHITECTURE.md](ARCHITECTURE.md) — subsystems and concerns
3. [docs/design/committed/design-principles.md](docs/design/committed/design-principles.md)
   — cross-cutting rules that guide design decisions
4. [docs/design/committed/event-schema.md](docs/design/committed/event-schema.md) — the
   event model that everything else is built around
5. [docs/adrs/](docs/adrs/) — every significant design decision
   with rationale

The [phase 1 closing summary](docs/plans/closed/2026-04-02-phase-1-foundation.md)
has a detailed inventory of what shipped and what was deferred.

## Adding a new tool

1. Create `services/fq-runtime/crates/fq-tools/src/builtin/<name>.rs`
2. Implement `Tool` for your struct (see `file_read.rs` for a
   minimal example, `exec.rs` for a complex one)
3. Register it in `ToolRegistry::with_builtins()` in
   `services/fq-runtime/crates/fq-runtime/src/tools.rs`
4. Add sandbox tests proving the tool respects sandbox boundaries
5. If the tool spawns processes, add tests to the containerised
   runner (`just test-shell-sandbox`)
