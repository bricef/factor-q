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

# === Services (delegate to per-service justfiles) ===

# Build all services
build:
    cd {{runtime_dir}} && just build

# Run all tests across services
test:
    cd {{runtime_dir}} && just test

# Run quality checks across services
ci:
    cd {{runtime_dir}} && just ci

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

# === Full workflows ===

# Start infrastructure and build all services
up: infra-up build

# Stop infrastructure
down: infra-down

# Start the runtime in the foreground (brings up infra, builds, runs)
[no-cd]
run: infra-up build
    cargo run --quiet --manifest-path {{justfile_directory()}}/{{runtime_dir}}/Cargo.toml --bin fq -- run
