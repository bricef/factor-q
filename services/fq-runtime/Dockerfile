# Multi-stage container build for the fq-runtime service.
#
# Builder stage uses the official Rust image to produce a release binary.
# Runtime stage uses a distroless image for minimal surface area.
#
# Build context is the REPO ROOT (`just docker-build` passes `-f` with this
# path): the unified workspace (#194) needs the root Cargo.toml/Cargo.lock
# and every member manifest to resolve. rust-toolchain.toml rides along so
# the container compiles with the same pinned toolchain as CI and local
# builds — "same commit" implies "same binary" (#102); keep the base image
# tag in lockstep with the pin.
#
# This Dockerfile is aspirational for now — the runtime doesn't yet do
# enough to warrant a deploy, but we want the container shape established
# early so the runtime is authored with distroless constraints in mind.

# --- Builder ---------------------------------------------------------------

FROM rust:1.95-slim-bookworm AS builder

WORKDIR /build

# Install only the system deps we actually need at build time.
# rustls (not native TLS) keeps the runtime dependency list small.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
       pkg-config \
    && rm -rf /var/lib/apt/lists/*

# Copy the workspace manifest, lockfile, toolchain pin, and every member
# manifest first so cargo can cache dependency resolution independently of
# source changes. This is a cheap form of caching without reaching for
# cargo-chef. Cargo refuses to load a workspace with a member manifest
# missing, so ALL members are listed — not just the ones this image builds.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY services/fq-runtime/crates/fq-cli/Cargo.toml services/fq-runtime/crates/fq-cli/
COPY services/fq-runtime/crates/fq-ops/Cargo.toml services/fq-runtime/crates/fq-ops/
COPY services/fq-runtime/crates/fq-runtime/Cargo.toml services/fq-runtime/crates/fq-runtime/
COPY services/fq-runtime/crates/fq-tools/Cargo.toml services/fq-runtime/crates/fq-tools/
COPY services/fq-dashboard/Cargo.toml services/fq-dashboard/
COPY services/fq-store/Cargo.toml services/fq-store/
COPY services/fq-test-support/Cargo.toml services/fq-test-support/

# Populate the members with empty stubs so cargo can build and cache the
# dependency graph before any real source is present. Every declared target
# needs a file behind it or cargo rejects the manifest — hence the store's
# bin and bench stubs.
RUN mkdir -p services/fq-runtime/crates/fq-cli/src/bin \
             services/fq-runtime/crates/fq-ops/src \
             services/fq-runtime/crates/fq-runtime/src \
             services/fq-runtime/crates/fq-tools/src \
             services/fq-dashboard/src \
             services/fq-store/src/bin services/fq-store/benches \
             services/fq-test-support/src \
    && echo '' > services/fq-runtime/crates/fq-cli/src/lib.rs \
    && echo 'fn main() {}' > services/fq-runtime/crates/fq-cli/src/bin/fq.rs \
    && echo 'fn main() {}' > services/fq-runtime/crates/fq-cli/src/bin/fqd.rs \
    && echo '' > services/fq-runtime/crates/fq-ops/src/lib.rs \
    && echo '' > services/fq-runtime/crates/fq-runtime/src/lib.rs \
    && echo '' > services/fq-runtime/crates/fq-tools/src/lib.rs \
    && echo 'fn main() {}' > services/fq-dashboard/src/main.rs \
    && echo '' > services/fq-store/src/lib.rs \
    && echo 'fn main() {}' > services/fq-store/src/bin/fq-cas.rs \
    && echo 'fn main() {}' > services/fq-store/benches/throughput.rs \
    && echo '' > services/fq-test-support/src/lib.rs \
    && cargo build --release -p fq-cli --bin fq \
    && rm -rf services/fq-runtime/crates/*/src services/fq-dashboard/src \
              services/fq-store/src services/fq-store/benches \
              services/fq-test-support/src

# Copy the real source and build.
COPY services/ services/
RUN cargo build --release -p fq-cli --bin fq \
    && strip target/release/fq

# --- Runtime ---------------------------------------------------------------

FROM gcr.io/distroless/cc-debian12:nonroot AS runtime

COPY --from=builder /build/target/release/fq /usr/local/bin/fq

# Defaults for container deployments. Operators should override paths to
# point at mounted volumes for persistence; the defaults fall through to
# /tmp so a fresh container runs without any explicit configuration.
ENV FQ_CONFIG=/etc/factor-q/fq.toml \
    FQ_AGENTS_DIR=/var/lib/factor-q/agents \
    FQ_CACHE_DIR=/var/cache/factor-q \
    FQ_NATS_URL=nats://nats:4222 \
    RUST_LOG=info

# Mount points for persistent state.
VOLUME ["/var/lib/factor-q", "/var/cache/factor-q"]

# Run as the distroless non-root user (uid 65532).
USER nonroot:nonroot

ENTRYPOINT ["/usr/local/bin/fq"]
CMD ["run"]
