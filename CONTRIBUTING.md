# Contributing to factor-q

## Development setup

**Prerequisites:**
- Rust toolchain (edition 2024 — install via [rustup](https://rustup.rs/))
- A Go toolchain, for the trigger adapters in `adapters/` (`just ci`
  gates them)
- Docker and Docker Compose (for NATS)
- [just](https://github.com/casey/just) (task runner)
- [cargo-audit](https://github.com/rustsec/rustsec/tree/main/cargo-audit)
  and [cargo-deny](https://github.com/EmbarkStudios/cargo-deny), for the
  dependency gate — `just install-audit-tools` builds the pinned versions
  (`just ci` runs the gate as its `audit` phase)
- A provider API key for smoke tests — `OPENROUTER_API_KEY` by
  default; not needed for unit tests or the Go gate

**First-time setup:**

```sh
# Clone the repo
git clone https://github.com/bricef/factor-q.git && cd factor-q

# Start NATS with JetStream
just infra-up

# Install the pinned broker binary used by integration tests
just install-nats

# Build
just build

# Run the tests (no API key needed)
just test

# Run the CLI
just fq --help
```

## Repository layout

```
factor-q/
├── Cargo.toml                  the single Cargo workspace (#194) — one Cargo.lock
├── services/fq-runtime/        runtime crates
│   ├── crates/fq-agent/         agent definitions: model, parser, registry
│   ├── crates/fq-cli/           the fq operator client
│   ├── crates/fq-daemon/        the fqd daemon binary
│   ├── crates/fq-edge/          authenticated generic edge
│   ├── crates/fq-ops/           operation-registry contract crate
│   ├── crates/fq-runtime/       core library
│   └── crates/fq-tools/         built-in tools and sandbox
├── services/fq-store/          content-addressed storage (fq-cas)
├── services/fq-dashboard/      operator dashboard
├── services/fq-test-support/   shared test-only helpers
├── adapters/fq-cron/           durable cron scheduler (Go)
├── adapters/github-watcher/    GitHub issue trigger adapter (Go)
├── tools/fq-lint/              source-policy linter — the size ratchets
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

factor-q has four test tiers, each with different prerequisites
and coverage:

### Tier 1: Unit tests

Fast, run in-process. Integration tests that need NATS spawn private brokers
using the pinned binary installed by `just install-nats`; they do not use the
shared broker from `just infra-up`.

```sh
# The runtime suite (including its self-provisioned integration tests)
just test-runtime

# Every Rust suite (runtime + store + dashboard)
just test

# Filter — one workspace, so plain cargo filters work from the root
cargo test -p fq-agent --lib definition
```

Run `just install-nats` once before the Rust suites so tests can provision
their isolated brokers. The shared broker from `just infra-up` is required by
smoke tests, not by these Rust suites.

### Tier 2: Go adapter gate

The trigger adapters (`adapters/fq-cron`, `adapters/github-watcher`)
are standalone Go binaries that reach factor-q only through the
trigger wire contract, never through `fq-runtime` code — so they have
their own gate, and `just ci` runs it as its `go-ci` phase. Skipping
it locally is the usual way a red CI arrives after a green `just
quality`.

```sh
just go-ci        # gofmt -l, go vet, go test, go build — every adapter
```

Needs a Go toolchain and the pinned `nats-server` from `just
install-nats` (the adapters' integration tests spawn their own
broker). No API key, no Docker.

### Tier 3: Smoke tests

End-to-end against a real LLM. Exercises the full stack: agent
loading, executor, tool-call loops, event bus, projection, and the
NATS-triggered dispatch path. Each test creates its own temp
directory and uses a unique agent id.

```sh
# Requires a running NATS and the provider key named by
# SMOKE_API_KEY_ENV — which defaults to OPENROUTER_API_KEY, not
# ANTHROPIC_API_KEY. Override SMOKE_API_KEY_ENV to use another.
just smoke
```

Costs roughly $0.005-0.01 per run. Tests are in `tests/smoke/smoke.sh`.
`just drill` is the sibling live drill (drain / clean shutdown / crash
recovery) and takes its key from `DRILL_API_KEY_ENV`, same default.

Smoke is deliberately *not* part of `just ci`: it needs a provider key
and makes a real, paid call.

### Tier 4: Containerised sandbox tests

The exec tool spawns child processes. Even though the test
battery uses only safe commands (`echo`, `true`, `sleep`, etc.),
we provide a disposable container runner that mounts the workspace
read-only and disables networking. Use this when iterating on the
exec tool's sandbox logic.

```sh
just test-shell-sandbox
```

This builds a Docker image on the pinned toolchain (the
`rust-toolchain.toml` pin travels into the build context) with the
cargo registry pre-populated, then runs the exec tests offline inside
the container. Takes ~30s on the first run (image build), ~5s on
subsequent runs.

### Running everything

```sh
just install-nats                              # once per fresh clone
just test                                      # every Rust suite
just go-ci                                     # the Go adapter gate
just infra-up                                  # shared broker for smoke tests
just smoke                                     # end-to-end (needs a provider key)
just test-shell-sandbox                        # containerised sandbox
```

Or `just ci` for the full local gate in one shot — `lint-docs`,
`check-links`, `quality`, `audit`, the four Rust suites, and `go-ci`,
timed per phase and fail-fast. It covers everything CI runs bar two
carve-outs it names: `smoke` (paid) and `docker-build` (minutes; run
it by hand after changing the Dockerfile, the workspace members, or
the lockfile).

## Code conventions

- **Rust edition 2024**, formatted with `cargo fmt`, linted with
  `cargo clippy -- -D warnings`. Run `just ci` to check both plus
  tests in one shot, or `just quality` for every non-test gate
  without waiting on the suites — that is exactly what the
  "Code quality" CI job runs. Each gate is also its own recipe if
  you want to iterate on one:

  ```sh
  just quality          # all of the below, fail-fast, timed
  just lint-sources     # the include! ban (see AGENTS.md)
  just test-fq-lint     # unit tests for the linter itself
  just lint-sizes       # file + function size ratchets
  just lint-fmt         # cargo fmt --check, workspace-wide
  just lint-clippy      # clippy per crate, with each crate's features
  just lint-creep       # functions approaching the 250-line cap (advisory)
  just lint-coupling    # module fan-in/fan-out and cycles (advisory)
  ```

  Listed in the order `quality` runs them.

- **Dependencies are audited, and the baseline is explicit.** `just
  audit` runs `cargo audit` over the lockfile and `cargo deny check`
  over the resolved graph; `deny.toml` is the reviewed baseline — the
  licence allow-list, the one permitted git source, and one explained
  `ignore` line per advisory that cannot be fixed yet. A red audit on
  your PR is fixed by `cargo update -p <crate>` when a patched release
  exists, and otherwise by one more explained line that the reviewer
  will read — never by a blanket allow. Dependabot opens the weekly
  update PRs (`.github/dependabot.yml`).

- **Size budgets are ratcheted, not advisory.** No file may exceed
  800 production lines and no function may exceed 250 lines;
  pre-existing offenders are pinned in `.file-size-baseline` and
  `.function-size-baseline` and may only ever shrink. `just
  sizes-bless` lowers a budget to match reality but refuses to raise
  one or admit a new entry, so relaxing a budget is always a
  hand-edit a reviewer sees. If a change trips a gate, extract into a
  new module or helper rather than raising the budget. `just
  lint-metrics` reports the underlying numbers. Rationale and the
  measurement rule live in `tools/fq-lint`.
- **No comments explaining what** — only why. Well-named
  identifiers speak for themselves.
- **Module-level doc comments** (`//!`) on every `.rs` file
  explaining the module's purpose and threat model where applicable.
- **Tests live next to the code**, but in a sibling file rather than
  inline: `#[cfg(test)] mod tests;` in `foo.rs`, with the body in
  `foo/tests.rs`. `super::*` gives the same access it had inline, so
  nothing else changes. Small modules may keep an inline
  `#[cfg(test)] mod tests { .. }` — the convention exists so a large
  file's production code is not buried under its own tests, and
  `fq-lint` resolves the declaration and excludes the file from the
  size budgets either way. Integration tests that need NATS spawn an
  isolated broker using the pinned `nats-server` binary.
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
3. Register it in `ToolRegistry::with_builtins_exec()` in
   `services/fq-runtime/crates/fq-runtime/src/tools.rs`
4. Add sandbox tests proving the tool respects sandbox boundaries
5. If the tool spawns processes, add tests to the containerised
   runner (`just test-shell-sandbox`)
