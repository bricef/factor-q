# factor-q top-level task runner
# Orchestrates services and infrastructure for the single Cargo workspace
# rooted here (#194). See https://github.com/casey/just

# Enable "$@" in recipe bodies so variadic *args preserve the original
# shell quoting. Without this, `just fq trigger sample-agent "hello world"`
# loses the quotes and fq receives four arguments instead of two.
set positional-arguments

# All Rust crates live in the single workspace at this justfile's directory
# (#194); recipes scope their suite with `-p` filters instead of cd'ing into
# per-service workspaces. The runtime suite is these seven crates — a new
# services/fq-runtime crate joins this list and the root Cargo.toml members.
# `fq-daemon` is here because every gate below is a `-p` filter: a crate
# absent from this line is not linted, not doc-checked and not tested, and
# the gates go green having never compiled it.
runtime_pkgs := "-p fq-agent -p fq-cli -p fq-daemon -p fq-edge -p fq-ops -p fq-runtime -p fq-tools"
infra_dir := "infrastructure"

# Show available recipes
default:
    @just --list

# === Infrastructure ===

# The broker version the test suite spawns, pinned in .nats-version so CI, the
# justfile, and any tooling read one source of truth rather than a literal
# buried in code (#233). Bump the file, not this.
nats_version := trim(read(".nats-version"))
nats_bin := justfile_directory() / ".tools" / "nats-server"

# The address of the shared dev broker (`just infra-up`), for the things
# that use one: `just run`, `just smoke`, `just drill`, and a daemon
# started by hand. NOT the test suite — every NATS-backed test spawns its
# own private broker (#233) and points the code under test at that, so
# `just test` ignores this. Override by exporting FQ_NATS_URL before
# invoking just.
# The dev broker requires token auth (infrastructure/nats/nats.conf). The
# token travels separately from the URL: the URL is host and port only —
# the daemon refuses one with userinfo, because it prints the URL in its
# banner, log and startup event (#540) — and the daemon reads the token
# from the variable its `[nats] token_env` names. The smoke and drill
# configs, and the `fq init` template, name FQ_NATS_TOKEN.
export FQ_NATS_URL := env_var_or_default("FQ_NATS_URL", "nats://127.0.0.1:4222")
export FQ_NATS_TOKEN := env_var_or_default("FQ_NATS_TOKEN", "fq-dev-token")

# Tests that spawn a private broker (#233) find nats-server here — the pinned
# binary `just install-nats` drops in .tools/, so a plain `just test` works
# without a PATH tweak. Override by exporting it yourself.
export FQ_TEST_NATS_SERVER := env_var_or_default("FQ_TEST_NATS_SERVER", nats_bin)

# Tests spawn their own private broker rather than sharing the dev one, so they
# need the binary — NATS is otherwise Docker-only here. Idempotent: re-running
# with the pinned version already installed is a no-op, so it is cheap to call
# from CI and from a dev's first run.
# Install the pinned nats-server into .tools/ (see .nats-version).
install-nats:
    #!/usr/bin/env bash
    set -euo pipefail
    want="{{nats_version}}"
    if [ -x "{{nats_bin}}" ] && "{{nats_bin}}" --version 2>/dev/null | grep -q "v${want}$"; then
        echo "nats-server v${want} already installed ({{nats_bin}})"
        exit 0
    fi
    case "$(uname -s)-$(uname -m)" in
        Linux-x86_64)   plat=linux-amd64  ;;
        Linux-aarch64)  plat=linux-arm64  ;;
        Darwin-x86_64)  plat=darwin-amd64 ;;
        Darwin-arm64)   plat=darwin-arm64 ;;
        *) echo "no nats-server build mapped for $(uname -s)-$(uname -m)" >&2; exit 1 ;;
    esac
    mkdir -p "$(dirname "{{nats_bin}}")"
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT
    file="nats-server-v${want}-${plat}.tar.gz"
    url="https://github.com/nats-io/nats-server/releases/download/v${want}/${file}"
    echo "fetching ${url}"
    curl -sfL "$url" -o "$tmp/nats.tgz"
    # Verify against .nats-checksums (vendored from the release's SHA256SUMS)
    # before anything from the archive can be executed — the version pin alone
    # doesn't protect against a swapped release asset or a corrupt download.
    expected="$(awk -v f="$file" '$2 == f {print $1}' "{{justfile_directory()}}/.nats-checksums")"
    if [ -z "$expected" ]; then
        echo "no pinned checksum for ${file} in .nats-checksums — regenerate it alongside .nats-version (see its header)" >&2
        exit 1
    fi
    if command -v sha256sum >/dev/null 2>&1; then
        actual="$(sha256sum "$tmp/nats.tgz" | awk '{print $1}')"
    else
        actual="$(shasum -a 256 "$tmp/nats.tgz" | awk '{print $1}')"
    fi
    if [ "$actual" != "$expected" ]; then
        echo "checksum mismatch for ${url}" >&2
        echo "  expected ${expected}" >&2
        echo "  got      ${actual}" >&2
        exit 1
    fi
    tar -xzf "$tmp/nats.tgz" --strip-components=1 -C "$(dirname "{{nats_bin}}")" "nats-server-v${want}-${plat}/nats-server"
    "{{nats_bin}}" --version

# Start infrastructure services (NATS, etc.)
infra-up:
    cd {{infra_dir}} && docker compose up -d

# Stop infrastructure services
infra-down:
    cd {{infra_dir}} && docker compose down

# Tail infrastructure logs
infra-logs:
    cd {{infra_dir}} && docker compose logs -f

# Show infrastructure status
infra-status:
    cd {{infra_dir}} && docker compose ps

# CI runs this after `infra-up`; locally NATS is usually already warm so
# you rarely need it.
# Wait until NATS is healthy on its monitoring port.
infra-wait:
    timeout 60 sh -c 'until curl -sf http://127.0.0.1:8222/healthz >/dev/null 2>&1; do sleep 1; done'

# === Services (one workspace, per-suite package filters) ===

# Build every Rust service.
build: build-runtime build-store build-dashboard

# Build the runtime crates (includes the fq CLI).
build-runtime:
    cargo build {{runtime_pkgs}}

# Build the store. `cli,service` matches the hermetic test path: the fq-cas
# binary and tarpc service, without the NATS-backed `bus` feature.
build-store:
    cargo build -p fq-store --features cli,service

# Build the dashboard.
build-dashboard:
    cargo build -p fq-dashboard

# One workspace (#194), so plain cargo filters work from the root too —
# e.g. `cargo test -p fq-runtime --lib agent::definition`. The per-suite
# recipes below scope the run and carry each suite's feature set.
# Run every Rust service's tests.
test: test-runtime test-store test-dashboard

# Run the runtime tests, or forward cargo args to filter.
test-runtime *args:
    cargo test {{runtime_pkgs}} "$@"

# Run the store tests (hermetic), or forward cargo args to filter.
test-store *args:
    cargo test -p fq-store --features cli,service "$@"

# Run the dashboard tests (hermetic), or forward cargo args to filter.
test-dashboard *args:
    cargo test -p fq-dashboard "$@"

# --all-targets covers tests and examples, matching the workspace lint policy.
# Type-check the whole workspace without building.
check:
    cargo check --workspace --all-targets

# Format the whole workspace.
fmt:
    cargo fmt

# The Rust suites run as independent CI jobs (.github/workflows/ci.yml) so a
# red in one never masks the others (#38); these targets are the local
# equivalents, and `rust-ci` runs them all in one command. `ci` invokes these
# same targets, so the local gate cannot drift from CI (#196). Every suite
# builds into the single workspace target/ (#194), scoped by `-p` filters.
#
# Phases are timed individually (#223) so a slow run says *which* step is
# slow — clippy, rustdoc, and test codegen are separately actionable.
#
# `build` front-loads the test-binary codegen that `cargo test` would do
# anyway, so `test` measures test *execution*. It adds no net work.
#
# `doctest` is split out because it cannot be front-loaded — doctests are
# compiled by rustdoc at run time and `--no-run` does not apply to them, so
# their build cost is irreducible. Left inside `test` it masquerades as test
# execution; its own phase makes it visible instead.
#
# `--tests` selects every target with `test = true` — lib and bin unittests
# plus integration tests — which is `cargo test`'s default set minus doctests.
# NOT --all-targets: that adds --benches, and fq-store's throughput bench is
# harness = false, so cargo would run a benchmark as part of the gate.

# NATS-backed tests spawn their own broker (#233) from the pinned nats-server,
# provisioned by the `install-nats` dependency; the MCP integration tests need
# Node/npx.
# Run the runtime Rust gate (doc, build, test). fmt/clippy: `just quality`.
runtime-ci: install-nats
    #!/usr/bin/env bash
    set -uo pipefail
    # Anchor the phase log on the justfile's own directory, not the caller's
    # cwd. An inherited value wins: nested under the root gate, append to its
    # log rather than start our own (#223).
    export FQ_CI_TIMINGS="${FQ_CI_TIMINGS:-{{justfile_directory()}}/.ci-timings}"
    source {{justfile_directory()}}/scripts/ci-timing.sh
    ci_timing_init
    run_phase "doc"       env RUSTDOCFLAGS="-D warnings" cargo doc --no-deps {{runtime_pkgs}}
    run_phase "build"     cargo build --tests {{runtime_pkgs}}
    run_phase "test"      cargo test --tests {{runtime_pkgs}}
    run_phase "doctest"   cargo test --doc {{runtime_pkgs}}

# No Node needed; the grant-bus test spawns its own private broker (#233) from
# the pinned nats-server, provisioned by the `install-nats` dependency.
# `--all-features` on lint/doc covers cli/service/bus/failpoints; `build` and
# `test` carry `cli,service` — the hermetic path. The final phases compile the
# failpoint seams and the bus feed only where they are actually driven.
# Run the store Rust gate (doc, build, test). fmt/clippy: `just quality`.
store-ci: install-nats
    #!/usr/bin/env bash
    set -uo pipefail
    # Same phase-log anchoring as runtime-ci (#223).
    export FQ_CI_TIMINGS="${FQ_CI_TIMINGS:-{{justfile_directory()}}/.ci-timings}"
    source {{justfile_directory()}}/scripts/ci-timing.sh
    ci_timing_init
    run_phase "doc"       env RUSTDOCFLAGS="-D warnings" cargo doc --no-deps -p fq-store --all-features
    run_phase "build"     cargo build --tests -p fq-store --features cli,service
    run_phase "test"      cargo test --tests -p fq-store --features cli,service
    run_phase "doctest"   cargo test --doc -p fq-store --features cli,service
    run_phase "failpoints" just test-failpoints
    run_phase "test-bus"  just test-bus

# The NATS-backed grant-bus integration test (#233) — spawns its own private
# nats-server from the pinned binary. Part of `store-ci`; kept callable on its
# own for a quick bus-only loop.
test-bus:
    cargo test -p fq-store --features bus --test grant_bus

# Adversarial interleaving tests driven through the fail-crate seams (the
# concurrency verification plan). Hermetic, but separate from `test-store` so
# the `failpoints` feature — which activates the protocol-step seams — is only
# compiled in where they're actually driven.
test-failpoints:
    cargo test -p fq-store --features failpoints --test gc_interleaving

# Hermetic — the dashboard's router tests spin their own read service over a
# temp DB; no broker needed. No doctest phase: doctests only exist for library
# targets and this crate is bin-only (`cargo test --doc` would hard-error).
# Run the dashboard Rust gate (doc, build, test). fmt/clippy: `just quality`.
dashboard-ci:
    #!/usr/bin/env bash
    set -uo pipefail
    # Same phase-log anchoring as runtime-ci (#223).
    export FQ_CI_TIMINGS="${FQ_CI_TIMINGS:-{{justfile_directory()}}/.ci-timings}"
    source {{justfile_directory()}}/scripts/ci-timing.sh
    ci_timing_init
    run_phase "doc"       env RUSTDOCFLAGS="-D warnings" cargo doc --no-deps -p fq-dashboard
    run_phase "build"     cargo build --tests -p fq-dashboard
    run_phase "test"      cargo test --tests -p fq-dashboard

# The shared test-only crate (#233) — the per-service gates only compile it as
# a dependency; this runs its own fmt/clippy/tests. Its self-tests spawn a
# broker from the pinned nats-server the `install-nats` dependency provisions.
# Run the fq-test-support gate (test). fmt/clippy: `just quality`.
test-support-ci: install-nats
    cargo test -p fq-test-support

# Run every Rust test suite locally (doc, build, test). Linting: `just quality`.
rust-ci: runtime-ci store-ci dashboard-ci test-support-ci

# The Go trigger adapters — standalone binaries that talk to factor-q only
# through the trigger wire contract, never fq-runtime code.
# Run the Go adapter gate (gofmt, vet, test, build).
gate-adapters: install-nats
    # Keep every standalone Go adapter on the same gate.
    for module in adapters/*/go.mod; do dir="${module%/go.mod}"; (cd "$dir" && test -z "$(gofmt -l .)" && go vet ./... && FQ_TEST_NATS_SERVER="{{nats_bin}}" go test ./... && go build -o /dev/null .); done

# Compatibility name used by CI.
go-ci: gate-adapters

# Run all quality checks — docs lint + link check + dependency audit + both
# Rust gates + the Go adapters (the full local gate) — and print a per-phase
# wall-clock timing summary at the end, so an operator can see where
# `just ci` spent its time.
#
# Why a script body instead of `ci: lint-docs check-links rust-ci go-ci`:
# recipe *dependencies* run before the body, so a dependency chain cannot be
# timed phase-by-phase — and worse, a failing dependency aborts the run before
# the body ever executes, so the summary would never print on exactly the runs
# that need it most. The body sources the small timing framework in
# scripts/ci-timing.sh and invokes each phase explicitly through its
# `run_phase`, wrapped in a stopwatch, preserving the original checks, their
# order, and fail-fast (the first failing phase stops the run and sets the exit
# code). The summary is printed on success AND on failure, via an EXIT trap.
#
# Every phase delegates to the same `just` target .github/workflows/ci.yml
# invokes, rather than re-implementing it here (#196). That is what keeps
# AGENTS.md's promise true — what passes `just ci` locally passes in CI —
# because there is exactly one definition of each suite's gate, not two that
# can drift. Adding a suite to CI means adding its target here, and nothing
# else. The trade is granularity: one timer per suite, where this recipe used
# to hand-roll a compile-vs-test split. Reclaiming that means putting
# start_timer/end_timer inside each suite's own gate, where those phases
# actually live — not re-inlining their builds here.
#
# NATS: no shared broker. Every suite's NATS-backed tests spawn their own
# private nats-server per test (#233, via fq-test-support), so `ci` neither
# brings a broker up nor tears one down. The pinned binary provisions itself:
# the Rust gates depend on `install-nats` (idempotent, a no-op once installed).
#
# smoke is intentionally NOT part of `ci`: it needs a provider key and makes
# a real, paid LLM call. Run it on its own with `just smoke`.
#
# `docker-build` is not part of `ci` either: it needs release binaries for a
# target (a full `build-release`, minutes) and a docker daemon. CI runs it in
# its own path-filtered job. So the promise below has one carve-out — after
# changing the Dockerfile, run `just build-release <target>`, `build-watcher`,
# `build-cron`, then `just docker-build <target>` and `just docker-check` by
# hand, because a green `just ci` does not cover the images.
#
# The full local gate — every target CI runs bar the two carve-outs above,
# timed, fail-fast.
ci:
    #!/usr/bin/env bash
    set -uo pipefail
    # Anchor the phase log on the justfile's own directory, not the caller's
    # cwd, so it lands in the same place whichever gate you enter through. An
    # inherited value wins: when a parent gate is already writing a log, this
    # run appends to that one rather than starting its own (#223).
    export FQ_CI_TIMINGS="${FQ_CI_TIMINGS:-{{justfile_directory()}}/.ci-timings}"
    source {{justfile_directory()}}/scripts/ci-timing.sh
    ci_timing_init
    # -- the gate, in order, fail-fast. Each phase is the same target CI runs.
    #    No NATS lifecycle: every suite spawns its own broker per test (#233),
    #    so there is no shared broker to bring up, wait for, or tear down. --
    run_phase "lint-docs"   just lint-docs
    run_phase "check-links" just check-links
    run_phase "quality"     just quality
    run_phase "audit"       just audit
    run_phase "runtime"     just runtime-ci
    run_phase "store"       just store-ci
    run_phase "dashboard"   just dashboard-ci
    run_phase "test-support" just test-support-ci
    run_phase "go-ci"       just go-ci

# === Container images (ADR-0035) ===
#
# The images are built from the binaries `build-release` / `build-watcher` /
# `build-cron` already produced — never compiled in-image — so the tarball
# channel and every image carry the identical binary for a commit. Root build
# context (`.dockerignore` admits `dist/bin/` and nothing else built); the
# Dockerfile stays with its service and holds every target.

# Copy a target's built binaries into dist/bin/, the image build's one input.
docker-stage target:
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p dist/bin
    for b in fq fqd fq-dashboard; do
        src="target/{{target}}/release/$b"
        [ -x "$src" ] || { echo "missing $src — run 'just build-release {{target}}' first" >&2; exit 1; }
        cp "$src" dist/bin/
    done
    for spec in adapters/github-watcher:github-watcher adapters/fq-cron:fq-cron; do
        src="${spec%%:*}/target/{{target}}/release/${spec##*:}"
        [ -x "$src" ] || { echo "missing $src — run 'just build-watcher {{target}}' / 'just build-cron {{target}}' first" >&2; exit 1; }
        cp "$src" dist/bin/
    done
    echo "staged $(ls dist/bin | tr '\n' ' ')into dist/bin/"

# Every image target, tagged factor-q/<name>:<tag> (default `latest`).
# `minimal` is the daemon held to the bare envelope; `dogfood` is minimal's
# binaries plus the fleet's toolchain; the rest are one static binary each.
# Build every container image from the staged binaries.
docker-build target tag="latest": (docker-stage target)
    docker build --target minimal   -t factor-q/fq-runtime:{{tag}}      -f services/fq-runtime/Dockerfile .
    docker build --target dogfood   -t factor-q/fq-dogfood:{{tag}}      -f services/fq-runtime/Dockerfile .
    docker build --target watcher   -t factor-q/github-watcher:{{tag}}  -f services/fq-runtime/Dockerfile .
    docker build --target cron      -t factor-q/fq-cron:{{tag}}         -f services/fq-runtime/Dockerfile .
    docker build --target dashboard -t factor-q/fq-dashboard:{{tag}}    -f services/fq-runtime/Dockerfile .

# An image that builds but cannot start is still broken (a leftover
# `CMD ["run"]` once outlived the subcommand it named), and distroless has
# no shell to check any other way: run each image's binary with --version.
# Prove every built image starts: --version through each entrypoint.
docker-check tag="latest":
    #!/usr/bin/env bash
    set -euo pipefail
    echo "fq-runtime (minimal) — the daemon:";  docker run --rm factor-q/fq-runtime:{{tag}} --version
    echo "fq-runtime (minimal) — the client:";  docker run --rm --entrypoint /usr/local/bin/fq factor-q/fq-runtime:{{tag}} --version
    echo "fq-dogfood — the daemon:";            docker run --rm factor-q/fq-dogfood:{{tag}} --version
    echo "fq-dogfood — the toolchain on the exec baseline PATH:"
    docker run --rm --entrypoint /usr/bin/env factor-q/fq-dogfood:{{tag}} -i PATH=/usr/local/bin:/usr/bin:/bin \
        sh -c 'cargo --version && rustc --version && cargo fmt --version && cargo clippy --version && go version && node --version && npx --version && just --version && gh --version | head -1 && git --version && jq --version && nats-server --version && sccache --version'
    echo "github-watcher:";                     docker run --rm factor-q/github-watcher:{{tag}} --version
    echo "fq-cron:";                            docker run --rm factor-q/fq-cron:{{tag}} --version
    echo "fq-dashboard:";                       docker run --rm factor-q/fq-dashboard:{{tag}} --version

# Where `docker-publish` pushes: <registry>/<image>:<tag>. The default is
# the repository's own container registry; override for a fork or a mirror.
docker_registry := env("FQ_DOCKER_REGISTRY", "ghcr.io/bricef")
# Every image `docker-build` produces, by name; publish iterates this list.
docker_images := "fq-runtime fq-dogfood github-watcher fq-cron fq-dashboard"

# Publish every built image to the registry under two tags: the twelve-hex
# commit the binaries inside it report, and the moving `main-latest`, the
# image-side twin of the tarball channel. The commit tag is what a host
# deploys and rolls back to; `main-latest` only names the newest build.
#
# The tag is checked, not trusted: it must be the checkout's HEAD, and it
# must be the commit `dist/bin/fq --version` was stamped with, with no
# `-dirty` suffix — the same coherence check ops/dogfood/deploy.sh applies
# to a bundle, moved to the publishing side so a mismatch never reaches
# the registry. Run `docker-build <target> <sha>` and `docker-check <sha>`
# first; this pushes what they built and proved.
# Push every image as <registry>/<name>:<sha> and :main-latest.
docker-publish sha:
    #!/usr/bin/env bash
    set -euo pipefail
    sha="{{sha}}"
    [[ "$sha" =~ ^[0-9a-f]{12}$ ]] || { echo "docker-publish: tag must be the twelve-hex commit, got '$sha'" >&2; exit 1; }
    head="$(git rev-parse --short=12 HEAD)"
    [ "$sha" = "$head" ] || { echo "docker-publish: tag $sha is not HEAD ($head) — publish only what this checkout built" >&2; exit 1; }
    [ -x dist/bin/fq ] || { echo "docker-publish: no dist/bin/fq — run 'just docker-build <target> $sha' first" >&2; exit 1; }
    stamped="$(dist/bin/fq --version | sed -nE 's/.*\(([0-9a-f]+(-dirty)?) .*/\1/p')"
    [ "$stamped" = "$sha" ] || { echo "docker-publish: dist/bin/fq reports '$stamped', not $sha — stale staging or a dirty build; refusing" >&2; exit 1; }
    for name in {{docker_images}}; do
        local_ref="factor-q/${name}:${sha}"
        docker image inspect "$local_ref" >/dev/null 2>&1 || { echo "docker-publish: $local_ref not built — run 'just docker-build <target> $sha' first" >&2; exit 1; }
        for tag in "$sha" main-latest; do
            ref="{{docker_registry}}/${name}:${tag}"
            docker tag "$local_ref" "$ref"
            docker push --quiet "$ref"
            echo "pushed $ref"
        done
    done

# Exercises the full walking skeleton: agent definitions parse, triggers
# run, the tool-call loop drives file_read and shell built-ins against
# Anthropic, events land in the SQLite projection, and the CLI query
# commands read them back.
#
# Requires:
#   - the provider key named by SMOKE_API_KEY_ENV (default OPENROUTER_API_KEY)
#   - NATS running (see `just infra-up`)
#   - fq binary built (this recipe builds it first)
#
# Run the end-to-end smoke tests against a real LLM (costs ~$0.005-0.01).
smoke: build-runtime
    {{justfile_directory()}}/tests/smoke/smoke.sh

# N concurrent invocations through drain / clean-shutdown / crash-recovery
# on a scratch daemon (plan §3, the Phase-2 gate's live leg). Needs
# the key named by DRILL_API_KEY_ENV (default OPENROUTER_API_KEY) and a running
# broker (`just infra-up`) with no other fq
# daemon on it.
# Run the parallel-workers live drill.
drill: build-runtime
    {{justfile_directory()}}/tests/smoke/drain-drill.sh

# Drift detector against the live Anthropic API. Sends one short
# Haiku call (~fractions of a cent) and asserts the response
# parses through our genai adapter. Use this when the mock-server
# tests pass but you want to confirm the real wire contract is
# unchanged — typically after Anthropic ships a model or API
# update. Requires ANTHROPIC_API_KEY in the environment.
acceptance-drift:
    cargo test {{runtime_pkgs}} -- --ignored anthropic_real_api

# Deep verification soak (reducer verification plan, slice 7): the
# randomised lifecycle driver with every oracle on. CI runs 48
# scenarios inside the normal test suite; this recipe scales the
# iteration count for local bug-hunting (~3 min per 1000).
soak iters="1000":
    FQ_SOAK_ITERS={{iters}} cargo test -p fq-runtime --lib soak_fixed -- --nocapture

# Preserves the user's invocation directory so relative paths in
# arguments resolve against the directory where the user invoked `just`,
# not the workspace or justfile directory.
#
# Uses "$@" (enabled by `set positional-arguments`) so quoted arguments
# are forwarded to fq intact.
#
# Run the fq client (e.g. `just fq --addr 127.0.0.1:9472 agent list`).
# Note `--agents-dir` is the daemon's flag, not the client's.
[no-cd]
fq *args:
    cargo run --quiet --manifest-path {{justfile_directory()}}/Cargo.toml --bin fq -- "$@"

# Renders from deterministic fixtures (headless chromium over file:// — no
# daemon, no broker). CI runs this when dashboard code changes and uploads
# the PNGs as an artifact. An artifact job, not a correctness gate — hence
# not part of `just ci` (#196).
# Screenshot every fq-dashboard page into dist/dashboard-screenshots/.
dashboard-screenshots out="dist/dashboard-screenshots":
    bash scripts/dashboard-screenshots.sh {{out}}

# === Docs ===

# Uses markdownlint-cli2 (pinned) via npx; rules in .markdownlint.jsonc.
# Auto-fix the mechanical rules with `just lint-docs --fix`.
# Lint every markdown file under docs/ — zero errors, per AGENTS.md.
lint-docs *args:
    npx --yes markdownlint-cli2@0.22.1 {{args}} "docs/**/*.md"

# Links pointing outside the repo (sibling checkouts) are reported but not
# failed.
#
# The self-test runs as a dependency, so it runs everywhere the check runs
# (CI, `just ci`, a bare `just check-links`) without a second CI job or a
# non-stdlib dependency. It guards the checker's skip logic, which is the
# part that can fail *green*: a skip rule that eats the whole tree reports
# success having scanned nothing, and running the real gate cannot tell
# that apart from a clean tree.
# Check that relative links in all repo markdown resolve.
check-links: test-check-links
    python3 scripts/check-links.py

# stdlib unittest — no pytest, nothing to install; CI already has python3
# for the checker itself.
# Run check-links.py's own tests.
test-check-links:
    python3 -m unittest discover -s scripts -p 'test_check_links.py'

# === Dependency audit ===

# The audit tools are pinned here and read back by the CI job with
# `just --evaluate`, so a version bump is one edit. Bump both together and
# re-run `just audit`: a new cargo-deny can change what deny.toml means.
cargo_audit_version := "0.22.2"
cargo_deny_version := "0.20.2"

# `cargo install` of an already-installed version is a no-op, so this is
# cheap to repeat. CI fetches prebuilt binaries of the same versions rather
# than compiling them (the Dependency audit job in .github/workflows/ci.yml).
# Install the pinned cargo-audit and cargo-deny.
install-audit-tools:
    cargo install --locked cargo-audit@{{cargo_audit_version}} cargo-deny@{{cargo_deny_version}}

# Two views of one Cargo.lock, one reviewed baseline (deny.toml, #406):
#
#   * `cargo audit` scans the flat lockfile — every crate cargo could ever
#     build, including ones no feature of ours reaches. `--deny warnings`
#     makes unmaintained, unsound and yanked findings fail alongside
#     vulnerabilities: a warning nobody is made to read is a finding nobody
#     reviews.
#   * `cargo deny check` resolves the workspace's real dependency graph
#     (every feature on — deny.toml [graph]) and fails on any advisory not
#     explicitly ignored there, on a yanked crate, on a licence outside the
#     allow-list, and on a source outside crates.io and the one pinned git
#     fork. `--locked`: an audit that rewrote Cargo.lock would have audited
#     something other than what gets built.
#
# deny.toml is the ONLY ignore list. Its advisory ids are handed to
# `cargo audit` verbatim, so one explained line accepts a finding in both
# tools, and an id the grep misses fails closed (audit goes red), never
# open. Each tool covers the other's blind spot: audit sees lockfile-only
# crates deny cannot reach (and, in cargo-deny 0.20, unsound advisories
# that carry an `unaffected` range, which deny skips); deny gates licences
# and sources audit knows nothing about.
# Audit dependencies: RustSec advisories, licences, sources (deny.toml).
audit:
    #!/usr/bin/env bash
    set -euo pipefail
    # The gate is the pinned versions, which CI installs exactly; whatever
    # else is on PATH is not it — a pre-0.16 cargo-deny cannot even parse a
    # version-2 deny.toml, and a different advisory engine returns a
    # different verdict on the same lockfile. A missing tool reads as an
    # empty version and fails the same way.
    have_audit="$(cargo audit --version 2>/dev/null | awk '{print $NF}' || true)"
    have_deny="$(cargo deny --version 2>/dev/null | awk '{print $NF}' || true)"
    if [ "$have_audit" != "{{cargo_audit_version}}" ] || [ "$have_deny" != "{{cargo_deny_version}}" ]; then
        echo "audit tools are not the pinned versions — cargo-audit ${have_audit:-missing} (want {{cargo_audit_version}}), cargo-deny ${have_deny:-missing} (want {{cargo_deny_version}}); run \`just install-audit-tools\`" >&2
        exit 1
    fi
    # Comment lines come off before the id grep: a `#`-disabled ignore must
    # stop reaching cargo audit the moment deny stops honouring it, or a
    # lockfile-only crate deny never sees (rsa today) would pass fail-open.
    ignore_flags=()
    while IFS= read -r id; do
        ignore_flags+=(--ignore "$id")
    done < <(grep -vE '^\s*#' deny.toml | grep -oE 'id *= *"RUSTSEC-[0-9]{4}-[0-9]{4}"' | grep -oE 'RUSTSEC-[0-9]{4}-[0-9]{4}')
    cargo audit --deny warnings ${ignore_flags[@]+"${ignore_flags[@]}"}
    cargo deny --locked check

# === Code quality ===

# No include!-based code splicing (postmortem of
# https://github.com/bricef/factor-q/pull/322): include! splices source
# files into one translation unit behind the tooling's back — rustfmt
# only discovers files through `mod` declarations, so a spliced file is
# silently never formatted again, and clippy's `disallowed-macros`
# cannot see include! at all (verified empirically; include_str! it can
# see, include! it cannot). Real module trees only. The same gate
# rejects include_str!/include_bytes! aimed at .rs files — embedding
# source as data is the same splice one step removed; scanners read the
# tree at runtime instead (see fq-cli tests/store_open_gate.rs). Data
# embeds (templates, web assets) stay fine. No allow-marker: matching
# lines are hard failures. The absolute ban is include! code splicing;
# if a future include_str! use is genuinely valid and trips this gate,
# allow it case by case — narrow the pattern or exempt that path here,
# in a reviewed change — never contort the code around the lint.
# The toolchain and broker pins are copied by hand into the container
# image's base images, every workflow's rust-toolchain step and every
# compose file's broker image (scripts/check-pins.sh lists them). A bump
# that misses a copy builds the image with a different compiler from the
# tarball's; this fails instead (#102).
# Check every hand-copied toolchain/broker version against its pin.
check-pins:
    scripts/check-pins.sh

# Reject include!-family macros that splice Rust source (tracked *.rs).
lint-sources:
    #!/usr/bin/env bash
    set -uo pipefail
    fail=0
    if git grep -nE '(^|[^[:alnum:]_])include!\(' -- '*.rs'; then
        echo "error: include! splices source outside the module tree; use real modules (justfile: lint-sources)" >&2
        fail=1
    fi
    if git grep -nE '(^|[^[:alnum:]_])include_(str|bytes)!\([[:space:]]*"[^"]*\.rs"' -- '*.rs'; then
        echo "error: embedding .rs files via include_str!/include_bytes!; read the tree at runtime instead (justfile: lint-sources)" >&2
        fail=1
    fi
    exit "$fail"

# The size ratchets (2026-07-25 cleanroom review, Part 2). Three split issues
# (#78 runner.rs, #189 fq-cli/src/lib.rs, #191 mcp.rs) stayed open across two
# reviews while every file they named grew — runner.rs 5.6k to 7.4k, lib.rs
# 4.4k to 6.3k. Reviews reliably land work that decomposes into issues and
# reliably do not land structural work, so this is a gate rather than a
# preference: it converts "should refactor" into "cannot merge".
#
# Two dimensions, one mechanism. FILES may not exceed 800 production lines;
# FUNCTIONS may not exceed 250 lines. Pre-existing offenders are pinned in
# .file-size-baseline and .function-size-baseline and may only ever shrink.
#
# Files count PRODUCTION lines — total minus #[cfg(test)] items — because Rust
# puts unit tests inline and a total-lines budget would tax the test suite,
# which is the strongest thing in this repo. It also aims the gate correctly:
# by total lines runner.rs (7,441) looks worse than lib.rs (6,274), but 3,193
# of runner.rs's lines are tests, so lib.rs is the bigger file in production
# terms. Test targets (tests/, benches/, test_support/, *_test.go) are out of
# scope, and so are test functions.
#
# Functions are measured from the `fn` keyword, not the start of the item, so
# a function is never charged for its own doc comment — this codebase puts
# incident and ADR rationale there and the gate must not discourage it.
#
# Why not clippy for the function half: clippy CAN say "no function over N"
# (too_many_lines) but cannot say "these functions must shrink" — its
# thresholds are global with no per-item baseline, so the only way to exempt
# known debt is an #[allow] at the site, which is a permanent pass rather than
# a shrinking budget. The threshold gate is complementary and tracked in #392.
#
# Measurement runs off a real syn AST (tools/fq-lint), not a text scan. The
# first version was a hand-rolled line scanner and it was wrong on three of
# the tree's 140 files — cfg(any(test, ..)), indented #[cfg(test)] items, and
# doc comments on test-only items. syn either parses the file exactly or fails
# loudly; there is no third outcome where it guesses.
#
# Budgets may only go down (`just sizes-bless`). Raising one, or admitting a
# new entry, means hand-editing the baseline — it shows in the diff and needs
# a human at the merge gate.
# Enforce the file and function size ratchets.
lint-sizes:
    cargo run -q -p fq-lint

# `--all` covers every workspace member, including tools/fq-lint. This only
# reports; `just fmt` is the one that rewrites files.
# Check workspace formatting without modifying anything.
lint-fmt:
    cargo fmt --check --all

# Per crate rather than one `--workspace` pass, because the feature sets are
# load-bearing: fq-store lints under --all-features (cli/service/bus/
# failpoints), and a workspace pass resolves default features, which would
# silently drop that coverage.
# Run clippy over every crate with its own feature set.
lint-clippy:
    #!/usr/bin/env bash
    set -uo pipefail
    export FQ_CI_TIMINGS="${FQ_CI_TIMINGS:-{{justfile_directory()}}/.ci-timings}"
    source {{justfile_directory()}}/scripts/ci-timing.sh
    ci_timing_init
    run_phase "runtime"      cargo clippy --all-targets {{runtime_pkgs}} -- -D warnings
    run_phase "store"        cargo clippy -p fq-store --all-targets --all-features
    run_phase "dashboard"    cargo clippy -p fq-dashboard --all-targets
    run_phase "test-support" cargo clippy -p fq-test-support --all-targets -- -D warnings
    run_phase "fq-lint"      cargo clippy -p fq-lint --all-targets -- -D warnings

# The measurement rule both ratchets depend on. Runs inside `just quality` so
# the linter proves itself before it gates anything else.
# Run fq-lint's own unit tests.
test-fq-lint:
    cargo test -q -p fq-lint

# ADVISORY — always exits 0, never gates. The 250-line function ratchet stops a
# merge, which makes it a cliff; this is the ramp, so growth is legible while
# there is still runway.
#
# Reports CODE lines (comments and blanks excluded) alongside physical span, at
# a threshold below the cap: the two measures differ by roughly 0.7 on this
# tree, so the 250-line cap lands near 175 code lines and a warning at 175
# would fire as the gate hit rather than before it.
#
# Deliberately NOT `cargo clippy --force-warn clippy::too_many_lines`, which
# measures the same idea: `cargo clippy -- <args>` does not invalidate cargo's
# fingerprint, so cached units never re-emit. Measured on this tree that
# reported 13 functions where a full rebuild found 35, and which 13 depended on
# what happened to be stale — an advisory that silently under-reports is worse
# than none. Deriving it from the AST fq-lint already builds is exact, instant,
# and needs no compile at all.
# Report function-length creep (advisory — never fails the gate).
lint-creep:
    cargo run -q -p fq-lint -- --creep

# Refuses to raise any budget or admit a new entry — the ratchets only ever
# tighten, so a budget can be lowered automatically but never relaxed.
# Lower the file and function size budgets to match reality.
sizes-bless:
    cargo run -q -p fq-lint -- --bless

# Non-enforcing: a read on the structural facts the AST layer makes cheap.
# Report function arity and physical span across the tree.
lint-metrics:
    cargo run -q -p fq-lint -- --metrics

# ADVISORY — always exits 0, never gates. The counterweight to the size
# ratchets: a file-size budget on its own rewards the wrong refactor, because
# splitting a god-file into two halves that import each other heavily passes
# `lint-sizes` and leaves the tree worse. Nothing else in `just quality` can
# tell that split apart from a real one.
#
# Reports per-crate module fan-in/fan-out and import cycles (Rust forbids crate
# cycles but permits module cycles silently, so no other tool here sees them).
# Edges are `crate::`/`super::` paths in production code between a crate's
# top-level modules — see tools/fq-lint/src/coupling.rs for what is deliberately
# not counted, and why every number is a floor rather than an estimate.
#
# `--json` is the same data for scripts/coupling-pr-comment.sh, which diffs a
# PR against its merge base and keeps one self-renewing comment on the PR.
# Gates and verification come later; this is the reporting layer first
# (docs/reviews/2026-07-27-code-quality-metrics.md).
# Report module coupling: fan-in/fan-out and cycles (advisory — never fails).
lint-coupling:
    cargo run -q -p fq-lint -- --coupling

# One command for every quality gate that is not a test, mirrored exactly by
# the "Code quality" CI job. Before this, the structural gates lived in the
# source-policy job while formatting and clippy were scattered across the four
# per-suite gates, so there was no single answer to "is this branch clean?"
# without running the test suites too. The per-suite gates keep doc/build/test;
# everything that reports on code *quality* is here.
#
# Clippy stays per-crate rather than one `--workspace` pass on purpose:
# fq-store lints under --all-features (cli/service/bus/failpoints), and a
# workspace pass resolves default features, which would silently drop that
# coverage. Same reason the invocations are not collapsed — the feature sets
# are load-bearing, not incidental.
#
# Needs no NATS and no Node: nothing here runs a test that wants a broker.
#
# Every phase is its own recipe, so any single gate can be run on its own
# while iterating — `just lint-sources`, `just test-fq-lint`, `just
# lint-sizes`, `just lint-fmt`, `just lint-clippy`. Phases nest in the timing
# summary, so `just lint-clippy` alone still reports its per-crate breakdown.
# Run every non-test quality gate: source policy, sizes, fmt, clippy.
quality:
    #!/usr/bin/env bash
    set -uo pipefail
    # Same phase-log anchoring as the per-suite gates (#223).
    export FQ_CI_TIMINGS="${FQ_CI_TIMINGS:-{{justfile_directory()}}/.ci-timings}"
    source {{justfile_directory()}}/scripts/ci-timing.sh
    ci_timing_init
    run_phase "check-pins"   just check-pins
    run_phase "lint-sources" just lint-sources
    run_phase "test-fq-lint" just test-fq-lint
    run_phase "lint-sizes"   just lint-sizes
    run_phase "lint-fmt"     just lint-fmt
    run_phase "lint-clippy"  just lint-clippy
    run_phase "lint-creep"   just lint-creep
    run_phase "lint-coupling" just lint-coupling

# === Release ===

# Reads [workspace.package] in the root Cargo.toml.
# Assert the release tag (vX.Y.Z) matches the workspace Cargo version.
check-version tag:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo_version="$(grep -m1 '^version = ' {{justfile_directory()}}/Cargo.toml | sed 's/.*"\(.*\)".*/\1/')"
    if [ "{{tag}}" != "v${cargo_version}" ]; then
        echo "release tag {{tag}} != Cargo version v${cargo_version}" >&2
        exit 1
    fi
    echo "release tag {{tag}} matches Cargo version v${cargo_version}"

# This builds all four; the two packaging recipes then differ in what
# they ship. `just package` (tagged releases) takes fq, fqd and fq-cas —
# the daemon has to be in a versioned install, and has been since the
# fq/fqd split. `just package-main` (the deploy bundle) adds fq-dashboard
# and the Go adapters.
# Build the release binaries (fq, fqd, fq-cas, fq-dashboard) for a target triple.
build-release target:
    cargo build --release --target {{target}} -p fq-cli --bin fq
    cargo build --release --target {{target}} -p fq-daemon --bin fqd
    cargo build --release --target {{target}} -p fq-store --features cli --bin fq-cas
    cargo build --release --target {{target}} -p fq-dashboard

# Rust binaries build into the workspace root target/ (#194), so their specs
# use `.` as the crate dir; the Go adapters keep per-adapter target dirs.
# Package the built binaries into a single bundle in dist/ (.tar.gz + .sha256).
package target:
    bash scripts/package.sh {{target}} .:fq .:fqd .:fq-cas

# Create a draft GitHub release for a tag from the dist/ artifacts.
publish-release tag:
    gh release create {{tag}} --draft --generate-notes ./dist/*

# === Main-branch deploy artifacts (#102) ===

# Builds into the same target/<triple>/release/ layout the Rust binaries
# use (per-adapter, so the package.sh spec form stays uniform). Pure Go with
# CGO_ENABLED=0 — as static as the musl Rust builds; the git SHA is embedded
# by Go's default -buildvcs.
# Build the github-watcher for a target triple.
build-watcher target:
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{target}}" in
        x86_64-unknown-linux-*)  export GOOS=linux  GOARCH=amd64 ;;
        aarch64-unknown-linux-*) export GOOS=linux  GOARCH=arm64 ;;
        aarch64-apple-darwin)    export GOOS=darwin GOARCH=arm64 ;;
        *) echo "no GOOS/GOARCH mapping for target {{target}}" >&2; exit 1 ;;
    esac
    cd adapters/github-watcher
    CGO_ENABLED=0 go build -o "target/{{target}}/release/github-watcher" .

# Build fq-cron for the target triple.
build-cron target:
    cd adapters/fq-cron && CGO_ENABLED=0 go build -o "target/{{target}}/release/fq-cron" .

# Every deployable, as the binary-only channel for hosts that are not
# containerised (install.sh, a bare VM). The dogfood host deploys the
# images instead (ADR-0035; ops/dogfood/deploy.sh), so the bundle no
# longer carries launchers.
# Package the rolling main-branch deploy bundle into dist/.
package-main target:
    bash scripts/package.sh {{target}} .:fq .:fqd .:fq-dashboard .:fq-cas adapters/github-watcher:github-watcher adapters/fq-cron:fq-cron

# Recreates both the release and its tag so tag, assets, and notes always
# point at the same commit. The channel keeps no history by design — the
# image registry keeps every commit tag, and that is the rollback history
# (ADR-0035; before it, hosts kept releases/<sha>/ dirs, #102).
# Publish/refresh the rolling `main-latest` pre-release from dist/.
publish-main sha:
    -gh release delete main-latest --yes
    -git push origin :refs/tags/main-latest
    gh release create main-latest --prerelease --target {{sha}} \
        --title "main @ {{sha}}" \
        --notes "Rolling deploy artifacts from main @ {{sha}} — not a versioned release. Fetched by ops/dogfood/deploy.sh; use the tagged releases for versioned installs." \
        ./dist/*

# === Full workflows ===

# Builds the runtime only — that is what `just fq` needs. `just build` fans out
# across all three Rust services if you want everything (#196).
# Start infrastructure and build the runtime (gives you `just fq`).
up: infra-up build-runtime

# Stop infrastructure
down: infra-down

# Start the runtime in the foreground (brings up infra, builds, runs).
# Run it from a project directory whose fqd.toml names
# `[nats] token_env = "FQ_NATS_TOKEN"` (what `fq init` writes): the dev
# broker requires the token, and the justfile exports the value but a
# daemon only reads the variable its config names (#540).
[no-cd]
run: infra-up build-runtime
    cargo run --quiet --manifest-path {{justfile_directory()}}/Cargo.toml --bin fqd --

# Deliberately spares .tools/ (pinned tooling, not a build product — see
# .gitignore; `just install-nats` reprovisions it anyway) and .ci-timings
# (the only record of where a killed CI run spent its time, #223).
# Remove all build artefacts: the workspace target dir, Go adapter binaries, dist/, TLC scratch.
clean:
    cargo clean
    # Keep every standalone Go adapter on the same sweep (mirrors gate-adapters):
    # `go clean` drops the dev binary, target/ holds `build-watcher`/`build-cron` output.
    for module in adapters/*/go.mod; do dir="${module%/go.mod}"; (cd "$dir" && go clean && rm -rf target); done
    rm -rf dist
    rm -rf docs/design/states docs/design/committed/states
