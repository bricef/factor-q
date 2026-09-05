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

The deployable unit is a container image
([ADR-0035](../../docs/adrs/accepted/0035-container-image-and-compose-supervision.md)):
one per shipped binary, tagged with the commit it was built from, run
under docker compose. The tarball channel (`install.sh`, the
`main-latest` bundle) stays as it is; the images are a second artifact
built from the same binaries.

### Container images

One `Dockerfile` beside this README holds every target. It compiles
nothing: the binaries come from the release build the deploy bundle
already ships, staged into `dist/bin/` by `just docker-stage`, so the
tarball and the image for a commit carry the identical binary and an
image build takes seconds.

| Target | Image | What it is |
|---|---|---|
| `minimal` | `factor-q/fq-runtime` | `fqd` + `fq` on `distroless/cc`. The bare envelope the daemon is held to: no shell, no init, configuration from one file and the environment, every piece of state under one volume. |
| `dogfood` | `factor-q/fq-dogfood` | `minimal`'s binaries (copied, never rebuilt) on Debian with the toolchain the fleet's agents run: cargo 1.95 with rustfmt and clippy, Go, Node 20, `just`, `gh`, `git`, `jq`, `nats-server`, `sccache`. |
| `watcher`, `cron`, `dashboard` | `factor-q/github-watcher`, `factor-q/fq-cron`, `factor-q/fq-dashboard` | one static binary each on `distroless/static`. |

```sh
# From the repository root. Build the binaries for a target first —
# the musl triple is what CI and the deploy bundle use; the gnu triple
# works for a local check.
just build-release x86_64-unknown-linux-musl   # CC_x86_64_unknown_linux_musl=musl-gcc
just build-watcher x86_64-unknown-linux-musl
just build-cron    x86_64-unknown-linux-musl
just docker-build  x86_64-unknown-linux-musl   # stages dist/bin/, builds every target
just docker-check                              # --version through every entrypoint
```

`docker-check` is the half that matters: an image that builds but cannot
start is still broken (a `CMD ["run"]` once outlived the subcommand it
named), and distroless has no shell to find out any other way. For the
dogfood image it also runs every toolchain binary with the `exec` tool's
baseline `PATH` and nothing else in the environment, which is exactly
how an agent's process will see them.

CI runs the same recipes in the `docker` job of
[`ci.yml`](../../.github/workflows/ci.yml), path-filtered to the
Dockerfile, the context filter, the pins the dogfood toolchain tracks
and whatever changes the release build. `just docker-build` is
deliberately **not** part of `just ci` — it needs release binaries and a
docker daemon — so after changing the Dockerfile, run the four commands
above by hand.

### Published images

Every merge to `main` publishes the five images to the repository's
container registry, from the same binaries as the tarball
([`main-artifacts.yml`](../../.github/workflows/main-artifacts.yml)):

```text
ghcr.io/bricef/fq-runtime:<sha>       ghcr.io/bricef/fq-runtime:main-latest
ghcr.io/bricef/fq-dogfood:<sha>       ghcr.io/bricef/fq-dogfood:main-latest
ghcr.io/bricef/github-watcher:<sha>   …
ghcr.io/bricef/fq-cron:<sha>
ghcr.io/bricef/fq-dashboard:<sha>
```

`<sha>` is the twelve-hex commit the binaries inside the image report
from `--version`, and `just docker-publish` refuses to push an image
whose `fq` reports anything else — a dirty build, or staging left over
from another commit — so the tag is a fact about the content, not a
label. It is what a host deploys and rolls back to. `main-latest` moves
with every merge and only ever names the newest build, like the tarball
channel of the same name; nothing should pin to it.

Pulling needs a registry login with `read:packages` if the packages are
private (`docker login ghcr.io`); the first publish of a package under a
personal account creates it private, and its visibility is set once in
the package's settings. Publishing from elsewhere — a fork, a mirror —
is `FQ_DOCKER_REGISTRY=<registry>/<owner> just docker-publish <sha>`
after a `docker login` to it.

**The dogfood image's toolchain is real binaries, not rustup proxies.**
The `exec` tool starts agent processes with
`PATH=/usr/local/bin:/usr/bin:/bin` and no inherited variable, and a
rustup proxy with neither `RUSTUP_HOME` nor `HOME` cannot find a
toolchain. So the image links the toolchain's own `cargo`, `rustc`,
`rustfmt`, `clippy-driver` and friends into `/usr/local/bin`, where they
work in an empty environment. The consequence is that
`rust-toolchain.toml` is not consulted inside the image — only rustup
reads it — and the compiler is whatever the image's base tag pins; keep
`rust:<version>` in the Dockerfile in lockstep with the repo's pin. The
same goes for the Go and Node base tags.

### One volume

The daemon's volume is the instance. Everything the daemon reads or
writes that outlives the container lives under `/var/lib/factor-q`, in
a fixed layout that mirrors the dogfood host's tree:

```text
/var/lib/factor-q/
├── fqd.toml         daemon config (host-authored)
├── fq.toml          client config — `[daemon] addr` for `fq status`
├── fq-cron.toml     the schedule, if fq-cron runs beside it
├── agents/          agent definitions
├── state/           the edge identity; state/client/ is `fq`'s pairing store
├── cache/           the three SQLite stores and the pricing snapshot
├── workspace/       per-invocation working directories
├── build/           cargo, go and sccache caches (dogfood only; prunable)
└── home/            HOME for the dogfood image's gh and git
```

The container has no other writable path. Backing up, migrating or
inspecting the instance is one volume, and the layout is the contract:
a backup may skip `workspace/` and `build/` by path, and a restore is
the tree, nothing else. The secrets file is **not** in it — compose
reads `env_file` before any volume is mounted, and it is the one thing
a volume copy must not carry.

A named volume mounted at `/var/lib/factor-q` for the first time is
seeded from the image's copy of the tree, ownership included: the
directories exist and belong to uid 65532 (`nonroot` in both images)
before the daemon starts. A bind mount is not seeded; the host directory
must already be owned by 65532.

### Environment variables

The image sets every directory the daemon has a setting for to a path
under the volume, and these are environment variables, which outrank
`fqd.toml` — an operator cannot point the edge identity or the stores
outside the volume by editing the config. That is the point. These are
the **daemon's** — `fqd`'s flags, not `fq`'s.

| Variable | `fqd` flag | Set in the image to | Notes |
|---|---|---|---|
| `FQ_DAEMON_CONFIG` | `--config` | `/var/lib/factor-q/fqd.toml` | Host-authored; the daemon refuses to start without one |
| `FQ_AGENTS_DIR` | `--agents-dir` | `/var/lib/factor-q/agents` | |
| `FQ_CACHE_DIR` | `--cache-dir` | `/var/lib/factor-q/cache` | The three SQLite stores and the pricing snapshot |
| `FQ_STATE_DIR` | `--state-dir` | `/var/lib/factor-q/state` | The edge identity — never regenerate it |
| `FQ_NATS_URL` | `--nats-url` | `nats://nats:4222` | The broker on the compose network; no credential, ever |
| `RUST_LOG` | (n/a) | `info` | Log level / filter |
| `XDG_CONFIG_HOME` | (n/a — `fq`'s) | `/var/lib/factor-q/state/client` | Where `fq connect` keeps the pairing, so it survives a recreate |

`[workspace] path` has no environment form: set it to
`/var/lib/factor-q/workspace` in `fqd.toml`.

The broker token is deliberately not on this list. `[nats] token_env`
in `fqd.toml` names the environment variable the daemon reads it from
(the `fq init` template and the dev tooling use `FQ_NATS_TOKEN`), and
`FQ_NATS_URL` / `--nats-url` is refused if it carries userinfo: the URL
is printed in the banner, the log and the `system.startup` event, so a
credential must never be part of it
([#540](https://github.com/bricef/factor-q/issues/540)). `RUST_LOG`
cannot put it there either: the daemon caps the NATS client's own
output below `trace`, because the client traces every protocol
operation it writes — the `CONNECT` that carries the token included —
so `RUST_LOG=trace` costs wire-level NATS debugging, never the
credential.

The dogfood image adds the build-cache variables — `CARGO_HOME`,
`CARGO_TARGET_DIR`, `RUSTC_WRAPPER=sccache`, `SCCACHE_DIR`, `GOCACHE`,
`GOMODCACHE` — all under `build/`, plus `HOME` under `home/`. They reach
an agent's processes only if its definition allowlists them
(`sandbox.env`): the runtime clears the child environment by design.
The image's tool list and this variable list are the compatibility
contract every live agent definition is read against before the image
replaces a host ([#587](https://github.com/bricef/factor-q/issues/587)).

Precedence remains CLI flag > env var > config file > default. Outside
a container, with none of these set, factor-q falls back to:

- `agents/` in cwd
- cache: `$XDG_CACHE_HOME/factor-q` → `$HOME/.cache/factor-q` → `/tmp/factor-q`
- state: `$XDG_STATE_HOME/factor-q` → `$HOME/.local/state/factor-q` → `/var/lib/factor-q`
- `nats://localhost:4222`

### Cache vs state

Two directories, two lifetimes (#362):

- **cache** (`FQ_CACHE_DIR`, `[cache] directory`) — the LiteLLM pricing
  snapshot, plus (for now) the daemon's SQLite stores. Its fallback is
  temp-dir shaped because FHS §5.5 and the XDG spec both license a
  cleaner to empty it — which is why the image pins it under the volume
  rather than trusting the fallback.
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

### Health

Both daemon images carry a `HEALTHCHECK` that runs `fq status`. It asks
the daemon over the authenticated edge and exits non-zero when nothing
answers, so a healthy container is one whose daemon is *serving*, not
merely one whose process exists. It needs a pairing: until `fq connect`
has been run once inside the container — `state/client/` keeps it — the
probe reports "no daemon paired" and the container shows unhealthy,
which is an honest state for an instance nobody has paired with yet.
The start period is generous (two minutes) so recovery at boot is not
mistaken for a hang; compose may override the timings. The adapter and
dashboard images have no probe yet — none of the three exposes one a
shell-less image can run; that is a follow-up on
[#587](https://github.com/bricef/factor-q/issues/587).

### Running it

The stack that runs the images is
[`ops/dogfood/compose.yml`](../../ops/dogfood/compose.yml): the broker,
the proxy, the daemon, the watcher, the dashboard and the scheduler,
with restart policy, health ordering on the broker, a stop grace period
longer than the drain deadline, resource limits on the daemon and
rotated logs. [`ops/dogfood/README.md`](../../ops/dogfood/README.md) is
the operator's guide to it — bootstrap of a dedicated host, the
tag-bump `deploy.sh` and its hourly `--auto` mode, hygiene, backups and
the restore drill, and the migration runbook. In short:

```sh
sudo ops/dogfood/bootstrap.sh          # a fresh Debian/Ubuntu host: docker, the deploy user, the tree, the crontab
~/fq-dogfood/deploy.sh                 # pull main-latest, verify, drain, up, verify — or deploy.sh <sha> to roll back
docker compose exec fqd fq status      # ask the daemon, through the container's own client
```

## Status

Pre-alpha, and the phase this crate is in moves faster than a paragraph
here can. [`STATUS.md`](../../STATUS.md) at the repository root is the
one place that says where the project is; it is maintained, this line is
not.
