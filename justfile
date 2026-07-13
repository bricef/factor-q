# factor-q top-level task runner
# Orchestrates services and infrastructure. Build details live in each service.
# See https://github.com/casey/just

# Enable "$@" in recipe bodies so variadic *args preserve the original
# shell quoting. Without this, `just fq trigger sample-agent "hello world"`
# loses the quotes and fq receives four arguments instead of two.
set positional-arguments

runtime_dir := "services/fq-runtime"
store_dir := "services/fq-store"
infra_dir := "infrastructure"

# Show available recipes
default:
    @just --list

# === Infrastructure ===

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

# Build all services
build:
    cd {{runtime_dir}} && just build

# Run all tests across services, or forward cargo args to filter
# (e.g. `just test -p fq-runtime --lib agent::definition`).
test *args:
    cd {{runtime_dir}} && just test "$@"

# The two Rust suites run as independent CI jobs (.github/workflows/ci.yml)
# so a red in one never masks the other (#38); these targets are the local
# equivalents, and `rust-ci` runs both in one command.

# Bus and MCP integration tests need NATS (`just infra-up`) and Node/npx.
# Run the runtime Rust gate (fmt-check, clippy, doc, test).
runtime-ci:
    cd {{runtime_dir}} && just ci

# Hermetic — no NATS, no Node; the grant-bus test is fq-store's `test-bus`.
# Run the store Rust gate (fmt-check, clippy, doc, test).
store-ci:
    cd {{store_dir}} && just ci

# Run both Rust quality gates locally (fmt-check, clippy, doc, test).
rust-ci: runtime-ci store-ci

# The Go trigger adapters — standalone binaries that talk to factor-q only
# through the trigger wire contract, never fq-runtime code. gofmt + vet +
# test + build.
go-ci:
    cd adapters/github-watcher && test -z "$(gofmt -l .)"
    cd adapters/github-watcher && go vet ./...
    cd adapters/github-watcher && go test ./...
    cd adapters/github-watcher && go build ./...

# Run all quality checks — docs lint + link check + both Rust gates + the Go
# adapters (the full local gate) — and print a per-phase wall-clock timing
# summary at the end, so an operator can see where `just ci` spent its time.
#
# Why a script body instead of `ci: lint-docs check-links rust-ci go-ci`:
# recipe *dependencies* run before the body, so a dependency chain cannot be
# timed phase-by-phase. The body below invokes each phase explicitly, wrapped
# in a stopwatch, preserving the original checks, their order, and fail-fast
# (the first failing phase stops the run and sets the exit code). The summary
# is printed on success AND on failure, via an EXIT trap.
#
# compile vs. test: each Rust suite is split so the operator's main question —
# "is a slow run compile-bound or test-bound?" — is answered. "compile
# (<suite>)" is all the compile / static-check work (fmt-check + clippy +
# rustdoc + a `cargo build --tests` that pre-builds the test binaries);
# "test (<suite>)" is then the cache-warm `cargo test` run (its time is test
# execution plus any doctest compilation). The extra build step only front-
# loads work `cargo test` would do anyway, so the checks that run are unchanged.
#
# NATS: only the runtime suite needs the broker. If it is already warm (the dev
# default) `ci` uses it and leaves it running; if it is cold, `ci` brings it up
# and — even on failure, via the trap — tears it down again, so a run never
# leaks a broker it started. store-ci and go-ci are hermetic.
#
# smoke is intentionally NOT part of `ci`: it needs ANTHROPIC_API_KEY and makes
# a real, paid LLM call. Run it on its own with `just smoke`.
ci:
    #!/usr/bin/env bash
    set -uo pipefail
    t_ci_start=$(date +%s)
    declare -a T_LABEL=() T_VAL=()
    nats_owned=0
    failed_label=""
    phase_no=0
    # -- helpers --
    record() { T_LABEL+=("$1"); T_VAL+=("$2"); }
    repeat() { local n=$1 ch=$2 out= i; for (( i=0; i<n; i++ )); do out="$out$ch"; done; printf '%s' "$out"; }
    human() {
        local s=$1
        if   [ "$s" -ge 3600 ]; then printf '%dh %dm %ds' $((s/3600)) $(((s%3600)/60)) $((s%60))
        elif [ "$s" -ge 60   ]; then printf '%dm %ds' $((s/60)) $((s%60))
        else                         printf '%ds' "$s"; fi
    }
    print_summary() {
        local total=$(( $(date +%s) - t_ci_start )) w=5 i n
        if [ "${#T_LABEL[@]}" -gt 0 ]; then
            for i in "${T_LABEL[@]}"; do [ "${#i}" -gt "$w" ] && w=${#i}; done
        fi
        printf '\n── CI timing summary %s\n' "$(repeat $((w + 2)) '─')"
        n=${#T_LABEL[@]}
        for (( i=0; i<n; i++ )); do
            printf '  %s %s %s\n' "${T_LABEL[$i]}" "$(repeat $(( w - ${#T_LABEL[$i]} + 3 )) '.')" "${T_VAL[$i]}"
        done
        printf '  %s\n' "$(repeat $((w + 5)) '─')"
        printf '  TOTAL %s %s\n' "$(repeat $(( w - 5 + 3 )) '.')" "$(human "$total")"
        [ -n "$failed_label" ] && printf '\n  x FAILED at phase: %s (exit non-zero)\n' "$failed_label"
        return 0
    }
    on_exit() {
        local rc=$?
        # safety net: a phase failed while we still held a broker we started —
        # tear it down (timed) so a failed run never leaks it.
        if [ "$nats_owned" = "1" ]; then
            local t0=$(date +%s)
            just infra-down >/dev/null 2>&1 || true
            record "NATS down" "$(human $(( $(date +%s) - t0 )))"
            nats_owned=0
        fi
        print_summary
        exit "$rc"
    }
    trap on_exit EXIT
    run_phase() {
        local label="$1"; shift
        phase_no=$((phase_no + 1))
        printf '\n==> [%d] %s\n' "$phase_no" "$label"
        local t0=$(date +%s) rc=0
        "$@" || rc=$?
        record "$label" "$(human $(( $(date +%s) - t0 )))"
        if [ "$rc" -ne 0 ]; then failed_label="$label"; exit "$rc"; fi
    }
    # -- phase bodies the generic runner cannot take as argv --
    compile_runtime() { ( cd {{runtime_dir}} && just fmt-check && just lint && just doc && cargo build --tests ); }
    test_runtime()    { ( cd {{runtime_dir}} && just test ); }
    compile_store()   { ( cd {{store_dir}}   && just fmt-check && just lint && just doc && cargo build --tests --features cli,service ); }
    test_store()      { ( cd {{store_dir}}   && just test ); }
    nats_up() {
        phase_no=$((phase_no + 1))
        printf '\n==> [%d] NATS up\n' "$phase_no"
        local t0=$(date +%s)
        if curl -sf http://127.0.0.1:8222/healthz >/dev/null 2>&1; then
            record "NATS up" "$(human $(( $(date +%s) - t0 ))) (already warm)"
            return 0
        fi
        nats_owned=1
        if ! ( just infra-up && just infra-wait ); then
            record "NATS up" "$(human $(( $(date +%s) - t0 )))"
            failed_label="NATS up"; exit 1
        fi
        record "NATS up" "$(human $(( $(date +%s) - t0 )))"
    }
    nats_down() {
        phase_no=$((phase_no + 1))
        printf '\n==> [%d] NATS down\n' "$phase_no"
        local t0=$(date +%s)
        if [ "$nats_owned" = "1" ]; then
            just infra-down >/dev/null 2>&1 || true
            record "NATS down" "$(human $(( $(date +%s) - t0 )))"
            nats_owned=0
        else
            record "NATS down" "skipped (left warm)"
        fi
    }
    # -- the gate, in order, fail-fast --
    run_phase "lint-docs"         just lint-docs
    run_phase "check-links"       just check-links
    nats_up
    run_phase "compile (runtime)" compile_runtime
    run_phase "test (runtime)"    test_runtime
    nats_down
    run_phase "compile (store)"   compile_store
    run_phase "test (store)"      test_store
    run_phase "go-ci"             just go-ci

# Build container images for all services
docker-build:
    cd {{runtime_dir}} && just docker-build

# Run the shell tool test battery inside a disposable container
# with networking disabled, for extra blast-radius containment
# while iterating on the sandbox.
test-shell-sandbox:
    cd {{runtime_dir}} && just test-shell-sandbox

# Run end-to-end smoke tests against a real LLM. Exercises the
# full walking skeleton: agent definitions parse, triggers run,
# the tool-call loop drives file_read and shell built-ins against
# Anthropic, events land in the SQLite projection, and the CLI
# query commands read them back.
#
# Requires:
#   - ANTHROPIC_API_KEY in the environment
#   - NATS running (see `just infra-up`)
#   - fq binary built (this recipe builds it first)
smoke: build
    {{justfile_directory()}}/tests/smoke/smoke.sh

# The parallel-workers live drill: N concurrent invocations through
# drain / clean-shutdown / crash-recovery on a scratch daemon (plan §3,
# the Phase-2 gate's live leg). Needs ANTHROPIC_API_KEY and a running
# broker (`just infra-up`) with no other fq daemon on it.
drill: build
    {{justfile_directory()}}/tests/smoke/drain-drill.sh

# Run the fq CLI (e.g. `just fq --agents-dir ./agents agent list`)
#
# Preserves the user's invocation directory so relative paths in
# arguments resolve against the directory where the user invoked `just`,
# not the workspace or justfile directory.
#
# Uses "$@" (enabled by `set positional-arguments`) so quoted arguments
# are forwarded to fq intact.
[no-cd]
fq *args:
    cargo run --quiet --manifest-path {{justfile_directory()}}/{{runtime_dir}}/Cargo.toml --bin fq -- "$@"

# Screenshot every fq-dashboard page from deterministic fixtures into
# dist/dashboard-screenshots/ (headless chromium over file:// — no
# daemon, no broker). CI runs this when dashboard code changes and
# uploads the PNGs as an artifact.
dashboard-screenshots out="dist/dashboard-screenshots":
    bash scripts/dashboard-screenshots.sh {{out}}

# === Docs ===

# Uses markdownlint-cli2 (pinned) via npx; rules in .markdownlint.jsonc.
# Auto-fix the mechanical rules with `just lint-docs --fix`.
# Lint ADR markdown — the zero-error scope mandated by AGENTS.md.
lint-docs *args:
    npx --yes markdownlint-cli2@0.22.1 {{args}} "docs/adrs/**/*.md"

# Check that relative links in all repo markdown resolve. Links pointing
# outside the repo (sibling checkouts) are reported but not failed.
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

# Build the release binaries (fq, fq-cas, fq-dashboard) for a target
# triple. The dashboard shares the runtime workspace, so its release
# build is incremental on top of fq's; tagged releases still package
# only fq + fq-cas (`just package`), while the main-branch deploy
# bundle takes all of them (`just package-main`).
build-release target:
    cd {{runtime_dir}} && cargo build --release --target {{target}} --bin fq --bin fq-dashboard
    cd {{store_dir}} && cargo build --release --target {{target}} --features cli --bin fq-cas

# Package the built binaries into a single bundle in dist/ (.tar.gz + .sha256).
package target:
    bash scripts/package.sh {{target}} {{runtime_dir}}:fq {{store_dir}}:fq-cas

# Create a draft GitHub release for a tag from the dist/ artifacts.
publish-release tag:
    gh release create {{tag}} --draft --generate-notes ./dist/*

# === Main-branch deploy artifacts (#102) ===

# Build the github-watcher for a target triple, into the same
# target/<triple>/release/ layout the Rust binaries use so
# scripts/package.sh bundles all three with one spec form. Pure Go with
# CGO_ENABLED=0 — as static as the musl Rust builds; the git SHA is
# embedded by Go's default -buildvcs.
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

# Package the rolling main-branch bundle: every deployable plus the
# dogfood launchers, so a deployed releases/<sha>/ dir is self-contained
# (ops/dogfood/deploy.sh extracts it verbatim).
package-main target:
    bash scripts/package.sh {{target}} {{runtime_dir}}:fq {{runtime_dir}}:fq-dashboard {{store_dir}}:fq-cas adapters/github-watcher:github-watcher ops/dogfood/run.sh ops/dogfood/watcher.sh ops/dogfood/dashboard.sh

# Publish/refresh the rolling `main-latest` pre-release from dist/.
# Recreates both the release and its tag so tag, assets, and notes always
# point at the same commit. The channel keeps no history by design —
# deploy hosts retain their own releases/<sha>/ dirs for rollback (#102).
publish-main sha:
    -gh release delete main-latest --yes
    -git push origin :refs/tags/main-latest
    gh release create main-latest --prerelease --target {{sha}} \
        --title "main @ {{sha}}" \
        --notes "Rolling deploy artifacts from main @ {{sha}} — not a versioned release. Fetched by ops/dogfood/deploy.sh; use the tagged releases for versioned installs." \
        ./dist/*

# === Full workflows ===

# Start infrastructure and build all services
up: infra-up build

# Stop infrastructure
down: infra-down

# Start the runtime in the foreground (brings up infra, builds, runs)
[no-cd]
run: infra-up build
    cargo run --quiet --manifest-path {{justfile_directory()}}/{{runtime_dir}}/Cargo.toml --bin fq -- run
