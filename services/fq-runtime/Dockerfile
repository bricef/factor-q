# Multi-stage container build for the fq-runtime service.
#
# Builder stage uses the official Rust image to produce a release binary.
# Runtime stage uses a distroless image for minimal surface area.
#
# This Dockerfile is aspirational for now — the runtime doesn't yet do
# enough to warrant a deploy, but we want the container shape established
# early so the runtime is authored with distroless constraints in mind.

# --- Builder ---------------------------------------------------------------

FROM rust:1.85-slim-bookworm AS builder

WORKDIR /build

# Install only the system deps we actually need at build time.
# rustls (not native TLS) keeps the runtime dependency list small.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
       pkg-config \
    && rm -rf /var/lib/apt/lists/*

# Copy the workspace manifest and crate manifests first so cargo can
# cache dependency resolution independently of source changes. This is
# a cheap form of caching without reaching for cargo-chef.
COPY Cargo.toml Cargo.lock ./
COPY crates/fq-cli/Cargo.toml crates/fq-cli/
COPY crates/fq-runtime/Cargo.toml crates/fq-runtime/
COPY crates/fq-tools/Cargo.toml crates/fq-tools/

# Populate the crates with empty stubs so cargo can build and cache the
# dependency graph before any real source is present.
RUN mkdir -p crates/fq-cli/src crates/fq-runtime/src crates/fq-tools/src \
    && echo 'fn main() {}' > crates/fq-cli/src/main.rs \
    && echo '' > crates/fq-runtime/src/lib.rs \
    && echo '' > crates/fq-tools/src/lib.rs \
    && cargo build --release --bin fq \
    && rm -rf crates/fq-cli/src crates/fq-runtime/src crates/fq-tools/src

# Copy the real source and build.
COPY crates/ crates/
RUN cargo build --release --bin fq \
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
