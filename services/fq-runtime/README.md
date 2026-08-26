# fq-runtime

The core factor-q runtime service: the agent executor, event bus
integration, the daemon, and the CLI. It ships two binaries — `fqd` (the
daemon) and `fq` (the client that talks to it over the authenticated
edge).

## Structure

Seven crates, all members of the single workspace rooted at the
[repository root](../../Cargo.toml) (#194) — there is no per-service
workspace manifest and no per-service justfile; every recipe is a `-p`
filter from the root.

| Crate | Purpose |
|---|---|
| [`fq-daemon`](crates/fq-daemon/) | The `fqd` binary — daemon assembly, hosted tasks, the operator surface's handlers |
| [`fq-cli`](crates/fq-cli/) | The `fq` binary — command parsing, and the verb modules that dial the edge |
| [`fq-runtime`](crates/fq-runtime/) | Core library — config, event schema, the bus, the worker and its reducer harness, the control plane and its stores |
| [`fq-agent`](crates/fq-agent/) | Agent definitions: the Markdown + YAML frontmatter parser, the registry, the config snapshot |
| [`fq-ops`](crates/fq-ops/) | The operator surface as data — operation identities, declared shapes, view DTOs. Transport-free, store-free |
| [`fq-edge`](crates/fq-edge/) | The edge transport — TLS with certificate pinning, biscuit tokens, the invoke/stream envelope, the registry a server binds handlers into |
| [`fq-tools`](crates/fq-tools/) | Built-in tool implementations and the `Tool` trait |

`fq-ops` and `fq-edge` are the seam: the daemon declares its surface in
terms of them and the client (and [`fq-dashboard`](../fq-dashboard/))
consumes it, so neither reader links the daemon or the storage under it.
Gate tests hold that shape — `fq-cli/tests/edge_migration_gate.rs` and
`thin_client_gate.rs`, `fq-edge/tests/thin_client_gate.rs`,
`fq-daemon/tests/store_open_gate.rs`.

## Prerequisites

- Rust toolchain (edition 2024 — pinned in `rust-toolchain.toml` at the
  repository root)
- [just](https://github.com/casey/just) for running tasks
- `nats-server` for the tests, which spawn a private broker per test
  (#233): `just install-nats` drops the pinned build in `.tools/` and the
  Rust gates depend on it, so a plain `just test` provisions itself. A
  shared broker is *not* a prerequisite — `just infra-up` is for running
  a daemon by hand (`just run`, `just smoke`, `just drill`), not for the
  suite.

## Development

Every task runs via `just` **from the repository root** — this directory
has no justfile of its own. Run `just --list` to see the recipes.

```sh
# Build every Rust service, or just this one
just build
just build-runtime

# Run every suite, or just this one
just test
just test-runtime

# Type-check the whole workspace without building
just check

# Format; then every non-test quality gate (source policy, sizes, fmt,
# clippy, creep, coupling)
just fmt
just quality

# This suite's doc/build/test gate; `just rust-ci` for all of them
just runtime-ci

# The full local gate — every target CI runs bar `docker-build` and
# `smoke`, timed, fail-fast
just ci

# Run the client (note: no `--` separator; `just fq -- agent list`
# passes `--` to clap and fails)
just fq --help
just fq agent list
just fq --addr 127.0.0.1:9472 agent list
```

## The binaries

`fqd` is the daemon and takes no subcommand. `fq` is a thin client:
almost every verb reaches a paired daemon over the authenticated edge,
so `fqd` has to be running and `fq connect` has to have paired with it.
The exceptions are marked *local* — they touch no daemon.

```
fqd                                         # start the daemon (edge, projection, dispatcher,
                                            #   worker, recovery, retention sweep)

fq init [-f|--force]                        # local: scaffold a project — fq.toml, fqd.toml,
                                            #   README.md, docker-compose.yml,
                                            #   agents/sample-agent.md
fq connect                                  # pair with a daemon: pin its certificate
                                            #   fingerprint (TOFU) and store the token
fq trigger <agent> [payload]                # queue work on the daemon's durable trigger stream
                                            #   (--via-nats is accepted and ignored: the
                                            #    in-process runner is retired)
fq agent list                               # the daemon's live registry, as `fq reload` left it
fq agent validate <path>                    # local: validate one definition file
fq events tail | query | get <id>           # the live stream, the projection, one whole event
fq costs [--agent] [--since]                # per-agent cost totals
fq invocation list | show | drop | resume   # triage: the ambiguous and the stuck
fq invocation transcript <id> [--follow]    # turns and tool calls, payloads included
fq workers list | show                      # worker liveness
fq dead-letters list | requeue              # dead-lettered triggers (#49/#169)
fq status                                   # build, streams, consumers, registry, projection,
                                            #   recovery
fq doctor [--json] [--fail-on-issues]       # durable-execution health, composed from the above
fq reload                                   # hot-swap the agent registry, no restart
fq down [--now]                             # drain and stop the daemon, confirmed
fq ops list                                 # the surface describing itself
fq token attenuate --grant <verb:domain>    # local: narrow a token, no round trip
fq version                                  # version and build information
```

Every trigger runs through the reducer runner documented in
[`docs/guide/reducer-harness.md`](../../docs/guide/reducer-harness.md):
a pure synchronous `step(StepInput) -> StepOutput` driven by a
host loop, with suspend/resume as a structural property of the
boundary.

**The two binaries' global flags are different, and so are their
configs.** `fq` takes `--config` (`FQ_CLI_CONFIG`, default `fq.toml`),
`--addr` (`FQ_ADDR`) and `--log-format`. `fqd` takes `--config`
(`FQ_DAEMON_CONFIG`, default `fqd.toml`), `--agents-dir`, `--nats-url`,
`--cache-dir`, `--state-dir` and `--log-format` — and no subcommands, so
"available on every subcommand" is only ever a statement about `fq`. The
agents directory, the broker and the cache are the daemon's; a client
that configured them would be describing a machine it may not be on. See
`fq --help` and `fqd --help`.

## Testing

Test tiers, each with different prerequisites:

| Tier | Command | Prerequisites |
|---|---|---|
| Unit + integration | `just test-runtime` | `nats-server` in `.tools/` (`just install-nats`; the gates depend on it) |
| Binary smoke | `just test-runtime` (covered) | — |
| Smoke (real LLM) | `just smoke` (repo root) | a running broker + the key named by `SMOKE_API_KEY_ENV` (default `OPENROUTER_API_KEY`) |
| Parallel-workers drill (real LLM) | `just drill` (repo root) | a running broker with no other daemon on it + `DRILL_API_KEY_ENV` (default `OPENROUTER_API_KEY`) |
| Drift detector (real LLM) | `just acceptance-drift` | `ANTHROPIC_API_KEY` |
| Shell sandbox (container) | `just test-shell-sandbox` | Docker |

**No shared broker for the suite.** Every NATS-backed test spawns its own
private `nats-server` (#233, via `fq-test-support`) and points the code
under test at it, so `just test` neither needs `just infra-up` nor an
`FQ_NATS_URL` in the environment. The live tiers are the exception: they
run a real daemon against a real broker, which is what `just infra-up`
is for.

The unit-and-integration tier includes the in-process acceptance
harness (`test_support::runtime::TestRuntime`) that boots the full
`fqd` runtime against a private broker and the mock Anthropic server,
plus four end-to-end scenarios (drop-ambiguous, stale-worker,
retry-sweeper-recovers-from-CP-outage, drop-vs-late-archived race).
New acceptance tests for future plans should reach for the harness
rather than building inline component wiring.

The binary smoke tier (`fq-cli/tests/smoke.rs`) invokes the `fq`
binary as a subprocess for CLI-level regressions that in-process
tests can't catch.

`just acceptance-drift` makes one short Haiku call (~fractions of a
cent) against the live Anthropic API and asserts the response parses
through our genai adapter. The full end-to-end pipeline (worker →
control-plane archive hand-off) is verified deterministically in
every `just test` run via `MockAnthropicServer`; run
`acceptance-drift` separately when you want a real-API sanity check —
for example after Anthropic ships a model or API update, or before
cutting a release. Failure usually means a wire-format change; update
the mock's response builders to match and re-run.

See [CONTRIBUTING.md](../../CONTRIBUTING.md) for the full testing
guide.

## Design references

- [Project vision](../../VISION.md)
- [Architecture](../../ARCHITECTURE.md)
- [Phase 1 closing summary](../../docs/plans/closed/2026-04-02-phase-1-foundation.md)
- [Agent authoring guide](../../docs/guide/agent-definitions.md)
- Relevant ADRs:
  - [0005 — Agent definition format](../../docs/adrs/accepted/0005-agent-definition-format.md)
  - [0009 — Rust as host language](../../docs/adrs/accepted/0009-technology-choices.md)
  - [0011 — NATS + JetStream event bus](../../docs/adrs/accepted/0011-event-bus-and-persistence.md)

## Deployment

### Container image

A multi-stage `Dockerfile` lives alongside this README. It builds with
the official Rust image and copies the binaries into a
[distroless](https://github.com/GoogleContainerTools/distroless) runtime
stage (`gcr.io/distroless/cc-debian12:nonroot`) for minimal surface
area and a non-root user by default.

It ships **both** binaries. `fqd` is the entrypoint and takes no
subcommand — it is the daemon. `fq` rides along because a distroless
image has no shell, so `docker exec`-ing the client is the only way to
ask a daemon in a container anything.

```sh
# From the repository root — the build context is the workspace, not
# this directory, because cargo needs every member manifest.
just docker-build
```

CI builds it, but only on a Docker-relevant change. The `docker` job in
[`ci.yml`](../../.github/workflows/ci.yml) is path-filtered to this
Dockerfile, the workspace manifest, the lockfile, the justfile and the
toolchain pin — nothing else — because a cold build compiles the whole
workspace again inside the image, minutes for a check only those files
can break. It builds the real image rather than
linting it (the failure mode is a manifest cargo cannot resolve, which
only a build finds) and then runs `fqd --version` inside it, because an
image that builds but cannot start is still broken.

That job exists because both halves had happened. Until 2026-08-25 the
image was in no workflow at all: its member-manifest list had been
missing `fq-agent` and `fq-daemon` since they were added, so `cargo
build` failed on the first line it reached, and its `CMD` still named a
subcommand the fq/fqd split had deleted. Note the filter's edge:
`just docker-build` is deliberately **not** part of `just ci`, so a
green local gate does not cover the image — after changing the
Dockerfile, the root `Cargo.toml`'s members, or the lockfile, run it by
hand.

### Environment variables

Every runtime path is configurable via an environment variable. These
are the **daemon's** — `fqd`'s flags, not `fq`'s. The defaults baked
into the container image are conventional Linux paths that operators can
mount volumes at; on a fresh host they all fall through to safe
locations.

| Variable          | `fqd` flag        | Default (container)          | Notes                                     |
|-------------------|-------------------|------------------------------|-------------------------------------------|
| `FQ_DAEMON_CONFIG`| `--config`        | `/etc/factor-q/fqd.toml`       | Optional — defaults apply if unset        |
| `FQ_AGENTS_DIR`   | `--agents-dir`    | `/var/lib/factor-q/agents`    | Mount a volume with your agent definitions |
| `FQ_CACHE_DIR`    | `--cache-dir`     | `/var/cache/factor-q`         | Pricing snapshot and the SQLite stores    |
| `FQ_STATE_DIR`    | `--state-dir`     | (unset — resolves as below)   | Durable state: the edge identity          |
| `FQ_NATS_URL`     | `--nats-url`      | `nats://nats:4222`            | Points at a NATS service on the same network |
| `RUST_LOG`        | (n/a)             | `info`                        | Log level / filter                        |

Precedence remains CLI flag > env var > config file > default. On a
host without any of these set, factor-q falls back to:

- `agents/` in cwd
- cache: `$XDG_CACHE_HOME/factor-q` → `$HOME/.cache/factor-q` → `/tmp/factor-q`
- state: `$XDG_STATE_HOME/factor-q` → `$HOME/.local/state/factor-q` → `/var/lib/factor-q`
- `nats://localhost:4222`

### Cache vs state

Two directories, two lifetimes (#362):

- **cache** (`FQ_CACHE_DIR`, `[cache] directory`) — the LiteLLM pricing
  snapshot, plus (for now) the daemon's SQLite stores. Its fallback is
  temp-dir shaped because FHS §5.5 and the XDG spec both license a
  cleaner to empty it.
- **state** (`FQ_STATE_DIR`, `[state] directory`) — data factor-q must
  never regenerate. Today that is the **edge identity**: the daemon's
  self-signed certificate and its biscuit token root. Losing it orphans
  every client pinned to the old fingerprint and invalidates every
  issued token, so its fallback is deliberately durable
  (`/var/lib/factor-q`), never `/tmp`.

A daemon whose state directory is empty but whose cache directory still
holds an `edge/` identity — every deployment that predates the split —
**adopts** the old one and says so on startup. The legacy copy is left
in place; delete it once you are satisfied. A fresh identity is minted
only when neither location has one.

The SQLite stores are the obvious next tenant of the state directory;
moving them is a separate migration.

### Mounted volumes

The image declares volumes at `/var/lib/factor-q` (agent definitions)
and `/var/cache/factor-q` (pricing JSON and the SQLite stores). Mount
persistent volumes at these paths for anything that needs to survive
container restarts.

The image sets no `FQ_STATE_DIR`, so the edge identity — the one thing
that must never be regenerated — lands wherever the fallback chain above
resolves, which may be neither volume. Set it explicitly to a path under
`/var/lib/factor-q`.

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

Pre-alpha, and the phase this crate is in moves faster than a paragraph
here can. [`STATUS.md`](../../STATUS.md) at the repository root is the
one place that says where the project is; it is maintained, this line is
not.
