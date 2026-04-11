# factor-q top-level task runner
# Orchestrates services and infrastructure. Build details live in each service.
# See https://github.com/casey/just

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

# Run the fq CLI (e.g. `just fq --agents-dir ./agents agent list`)
#
# Preserves the user's invocation directory so relative paths in
# arguments resolve against the directory where the user invoked `just`,
# not the workspace or justfile directory.
[no-cd]
fq *args:
    cargo run --quiet --manifest-path {{justfile_directory()}}/{{runtime_dir}}/Cargo.toml --bin fq -- {{args}}

# === Full workflows ===

# Start infrastructure and build all services
up: infra-up build

# Stop infrastructure
down: infra-down

# Start the runtime in the foreground (brings up infra, builds, runs)
[no-cd]
run: infra-up build
    cargo run --quiet --manifest-path {{justfile_directory()}}/{{runtime_dir}}/Cargo.toml --bin fq -- run
