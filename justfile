# factor-q top-level task runner
# Orchestrates services and infrastructure. Build details live in each service.
# See https://github.com/casey/just

# Enable "$@" in recipe bodies so variadic *args preserve the original
# shell quoting. Without this, `just fq trigger sample-agent "hello world"`
# loses the quotes and fq receives four arguments instead of two.
set positional-arguments

runtime_dir := "services/fq-runtime"
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

# The bus and MCP integration tests need NATS (`just infra-up`) and
# Node/npx on PATH.
# Run the Rust quality gate (fmt-check, clippy, test).
rust-ci:
    cd {{runtime_dir}} && just ci

# CI runs the two halves as separate jobs that each invoke a just target
# (see .github/workflows/ci.yml).
# Run all quality checks — docs lint + the Rust gate (the full local gate).
ci: lint-docs rust-ci

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

# === Docs ===

# Uses markdownlint-cli2 (pinned) via npx; rules in .markdownlint.jsonc.
# Auto-fix the mechanical rules with `just lint-docs --fix`.
# Lint ADR markdown — the zero-error scope mandated by AGENTS.md.
lint-docs *args:
    npx --yes markdownlint-cli2@0.22.1 {{args}} "docs/adrs/**/*.md"

# === Full workflows ===

# Start infrastructure and build all services
up: infra-up build

# Stop infrastructure
down: infra-down

# Start the runtime in the foreground (brings up infra, builds, runs)
[no-cd]
run: infra-up build
    cargo run --quiet --manifest-path {{justfile_directory()}}/{{runtime_dir}}/Cargo.toml --bin fq -- run
