# factor-q top-level task runner
# Orchestrates services and infrastructure. Build details live in each service.
# See https://github.com/casey/just

# Enable "$@" in recipe bodies so variadic *args preserve the original
# shell quoting. Without this, `just fq trigger sample-agent "hello world"`
# loses the quotes and fq receives four arguments instead of two.
set positional-arguments

runtime_dir := "services/fq-runtime"
store_dir := "services/fq-store"
dashboard_dir := "services/fq-dashboard"
test_support_dir := "services/fq-test-support"
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

# === Services (delegate to per-service justfiles) ===

# Fans out across all three Rust projects rather than delegating to fq-runtime
# alone, so the docstring is true (#196).
# Build every Rust service.
build: build-runtime build-store build-dashboard

# Build the runtime (fq-runtime workspace — includes fq-cli).
build-runtime:
    cd {{runtime_dir}} && just build

# Build the store.
build-store:
    cd {{store_dir}} && just build

# Build the dashboard.
build-dashboard:
    cd {{dashboard_dir}} && just build

# To filter, use the per-suite recipe — e.g.
# `just test-runtime -p fq-runtime --lib agent::definition` — since a cargo
# filter is only meaningful against a single workspace (#196).
# Run every Rust service's tests.
test: test-runtime test-store test-dashboard

# Run the runtime tests, or forward cargo args to filter.
test-runtime *args:
    cd {{runtime_dir}} && just test "$@"

# Run the store tests (hermetic), or forward cargo args to filter.
test-store *args:
    cd {{store_dir}} && just test "$@"

# Run the dashboard tests (hermetic), or forward cargo args to filter.
test-dashboard *args:
    cd {{dashboard_dir}} && just test "$@"

# The three Rust suites run as independent CI jobs (.github/workflows/ci.yml)
# so a red in one never masks the others (#38); these targets are the local
# equivalents, and `rust-ci` runs all three in one command. `ci` invokes these
# same targets, so the local gate cannot drift from CI (#196).

# NATS-backed tests spawn their own broker (#233) from the pinned nats-server,
# provisioned by the `install-nats` dependency; the MCP integration tests need
# Node/npx.
# Run the runtime Rust gate (fmt-check, clippy, doc, test).
runtime-ci: install-nats
    cd {{runtime_dir}} && just ci

# No Node needed; the grant-bus test spawns its own private broker (#233) from
# the pinned nats-server, provisioned by the `install-nats` dependency.
# Run the store Rust gate (fmt-check, clippy, doc, test).
store-ci: install-nats
    cd {{store_dir}} && just ci

# Hermetic — the dashboard's router tests spin their own read service
# over a temp DB; no broker needed.
# Run the dashboard Rust gate (fmt-check, clippy, doc, test).
dashboard-ci:
    cd {{dashboard_dir}} && just ci

# The shared test-only crate (#233) is its own workspace, so the per-service
# gates only compile it as a dependency — this runs its own fmt/clippy/tests.
# Its self-tests spawn a broker; the `install-nats` dependency provisions the
# pinned binary.
# Run the fq-test-support gate (fmt-check, clippy, test).
test-support-ci: install-nats
    cd {{test_support_dir}} && cargo fmt --check
    cd {{test_support_dir}} && cargo clippy --all-targets -- -D warnings
    cd {{test_support_dir}} && FQ_TEST_NATS_SERVER="${FQ_TEST_NATS_SERVER:-{{nats_bin}}}" cargo test

# Run all Rust quality gates locally (fmt-check, clippy, doc, test).
rust-ci: runtime-ci store-ci dashboard-ci test-support-ci

# The Go trigger adapters — standalone binaries that talk to factor-q only
# through the trigger wire contract, never fq-runtime code.
# Run the Go adapter gate (gofmt, vet, test, build).
gate-adapters: install-nats
    # Keep every standalone Go adapter on the same gate.
    for module in adapters/*/go.mod; do dir="${module%/go.mod}"; (cd "$dir" && test -z "$(gofmt -l .)" && go vet ./... && FQ_TEST_NATS_SERVER="{{nats_bin}}" go test ./... && go build -o /dev/null .); done

# Compatibility name used by CI.
go-ci: gate-adapters

# Run all quality checks — docs lint + link check + both Rust gates + the Go
# adapters (the full local gate) — and print a per-phase wall-clock timing
# summary at the end, so an operator can see where `just ci` spent its time.
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
# start_timer/end_timer inside each suite's own justfile, where those phases
# actually live — not re-inlining their builds here.
#
# NATS: no shared broker. Every suite's NATS-backed tests spawn their own
# private nats-server per test (#233, via fq-test-support), so `ci` neither
# brings a broker up nor tears one down. The pinned binary provisions itself:
# the Rust gates depend on `install-nats` (idempotent, a no-op once installed).
#
# smoke is intentionally NOT part of `ci`: it needs ANTHROPIC_API_KEY and makes
# a real, paid LLM call. Run it on its own with `just smoke`.
#
# The full local gate — every target CI runs, timed, fail-fast.
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
    run_phase "runtime"     just runtime-ci
    run_phase "store"       just store-ci
    run_phase "dashboard"   just dashboard-ci
    run_phase "test-support" just test-support-ci
    run_phase "go-ci"       just go-ci

# Build container images for all services
docker-build:
    cd {{runtime_dir}} && just docker-build

# Runs inside a disposable container with networking disabled, for extra
# blast-radius containment while iterating on the sandbox.
# Run the exec tool's test battery in a locked-down container.
test-shell-sandbox:
    cd {{runtime_dir}} && just test-shell-sandbox

# Exercises the full walking skeleton: agent definitions parse, triggers
# run, the tool-call loop drives file_read and shell built-ins against
# Anthropic, events land in the SQLite projection, and the CLI query
# commands read them back.
#
# Requires:
#   - ANTHROPIC_API_KEY in the environment
#   - NATS running (see `just infra-up`)
#   - fq binary built (this recipe builds it first)
#
# Run the end-to-end smoke tests against a real LLM (costs ~$0.005-0.01).
smoke: build-runtime
    {{justfile_directory()}}/tests/smoke/smoke.sh

# N concurrent invocations through drain / clean-shutdown / crash-recovery
# on a scratch daemon (plan §3, the Phase-2 gate's live leg). Needs
# ANTHROPIC_API_KEY and a running broker (`just infra-up`) with no other fq
# daemon on it.
# Run the parallel-workers live drill.
drill: build-runtime
    {{justfile_directory()}}/tests/smoke/drain-drill.sh

# Preserves the user's invocation directory so relative paths in
# arguments resolve against the directory where the user invoked `just`,
# not the workspace or justfile directory.
#
# Uses "$@" (enabled by `set positional-arguments`) so quoted arguments
# are forwarded to fq intact.
#
# Run the fq CLI (e.g. `just fq --agents-dir ./agents agent list`).
[no-cd]
fq *args:
    cargo run --quiet --manifest-path {{justfile_directory()}}/{{runtime_dir}}/Cargo.toml --bin fq -- "$@"

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
# Lint ADR markdown — the zero-error scope mandated by AGENTS.md.
lint-docs *args:
    npx --yes markdownlint-cli2@0.22.1 {{args}} "docs/adrs/**/*.md"

# Links pointing outside the repo (sibling checkouts) are reported but not
# failed.
# Check that relative links in all repo markdown resolve.
check-links:
    python3 scripts/check-links.py

# === Release ===

# Assert the release tag (vX.Y.Z) matches the workspace Cargo version.
check-version tag:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo_version="$(grep -m1 '^version = ' {{runtime_dir}}/Cargo.toml | sed 's/.*"\(.*\)".*/\1/')"
    if [ "{{tag}}" != "v${cargo_version}" ]; then
        echo "release tag {{tag}} != Cargo version v${cargo_version}" >&2
        exit 1
    fi
    echo "release tag {{tag}} matches Cargo version v${cargo_version}"

# Tagged releases still package only fq + fq-cas (`just package`); the
# main-branch deploy bundle takes all of them (`just package-main`).
# Build the release binaries (fq, fq-cas, fq-dashboard) for a target triple.
build-release target:
    cd {{runtime_dir}} && cargo build --release --target {{target}} --bin fq
    cd {{store_dir}} && cargo build --release --target {{target}} --features cli --bin fq-cas
    cd {{dashboard_dir}} && cargo build --release --target {{target}}

# Package the built binaries into a single bundle in dist/ (.tar.gz + .sha256).
package target:
    bash scripts/package.sh {{target}} {{runtime_dir}}:fq {{store_dir}}:fq-cas

# Create a draft GitHub release for a tag from the dist/ artifacts.
publish-release tag:
    gh release create {{tag}} --draft --generate-notes ./dist/*

# === Main-branch deploy artifacts (#102) ===

# Builds into the same target/<triple>/release/ layout the Rust binaries
# use, so scripts/package.sh bundles all three with one spec form. Pure Go
# with CGO_ENABLED=0 — as static as the musl Rust builds; the git SHA is
# embedded by Go's default -buildvcs.
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

# Every deployable plus the dogfood launchers, so a deployed
# releases/<sha>/ dir is self-contained (ops/dogfood/deploy.sh extracts it
# verbatim).
# Package the rolling main-branch deploy bundle into dist/.
package-main target:
    bash scripts/package.sh {{target}} {{runtime_dir}}:fq {{dashboard_dir}}:fq-dashboard {{store_dir}}:fq-cas adapters/github-watcher:github-watcher ops/dogfood/run.sh ops/dogfood/watcher.sh ops/dogfood/dashboard.sh

# Recreates both the release and its tag so tag, assets, and notes always
# point at the same commit. The channel keeps no history by design —
# deploy hosts retain their own releases/<sha>/ dirs for rollback (#102).
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

# Start the runtime in the foreground (brings up infra, builds, runs)
[no-cd]
run: infra-up build-runtime
    cargo run --quiet --manifest-path {{justfile_directory()}}/{{runtime_dir}}/Cargo.toml --bin fq -- run
