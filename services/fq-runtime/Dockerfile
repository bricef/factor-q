# Container images for factor-q (ADR-0035): the deployable unit is an
# image, one per shipped binary, tagged with the commit it was built from.
#
# The binaries are NOT compiled here. `just build-release <target>` (plus
# `build-watcher` / `build-cron` for the Go adapters) produces them, and
# `just docker-stage <target>` copies them into `dist/bin/`, which is the
# only thing this file reads from the build context. One build therefore
# feeds both the tarball channel and every image, so "same commit" means
# "same binary" in both (#102), and an image build is seconds rather than
# a cold workspace compile. `.dockerignore` admits `dist/bin/` for exactly
# this and nothing else under `dist/`.
#
# Targets (`docker build --target <name> -f services/fq-runtime/Dockerfile .`):
#
#   minimal    fqd + fq on distroless. The bare envelope the daemon is held
#              to (ADR-0035 clause 3): no shell, no init, configuration
#              from one file and the environment, every piece of state
#              under ONE volume. Anything the daemon turns out to need
#              beyond this is a defect in the daemon.
#   dogfood    `minimal`'s binaries on Debian with the toolchain the fleet's
#              agents run inside their invocations (`just ci`: cargo, go,
#              node, just, gh, git, jq, nats-server, sccache). The
#              transitional shape while worker and control plane share a
#              process (clause 4); when they split, only the worker image
#              carries this.
#   watcher, cron, dashboard
#              one static binary each on distroless/static.
#
# Base image tags are pinned in lockstep with the repo's own pins
# (rust-toolchain.toml, adapters/*/go.mod, .nats-version, ci.yml's Node);
# bump them together — the dogfood target's toolchain must match what CI
# and the maintainer build with, or the fleet's `just ci` diverges from
# the real one.

# --- one volume, owned by the runtime user ---------------------------------
#
# The daemon's volume IS the instance (ADR-0035 clause 6): a fixed layout
# under /var/lib/factor-q that mirrors the dogfood host's tree —
#
#   fqd.toml, fq.toml, fq-cron.toml, agents/    config (host-authored)
#   state/       the edge identity + the client's pairing store
#   cache/       the three SQLite stores and the pricing snapshot
#   workspace/   per-invocation working directories
#   build/       cargo/go/sccache caches (dogfood only; prunable)
#   home/        HOME for the dogfood image's gh and git configuration
#
# The container has no other writable path. A named volume mounted here
# for the first time is seeded from this image's copy of the directory,
# ownership included, which is why the tree is created here under the
# runtime uid rather than left for an entrypoint script to fix up: the
# minimal image has no shell to run one. A bind mount is not seeded — the
# host directory must already be owned by uid 65532.
FROM debian:bookworm-slim AS layout
RUN mkdir -p /out/var/lib/factor-q/agents \
             /out/var/lib/factor-q/state/client \
             /out/var/lib/factor-q/cache \
             /out/var/lib/factor-q/workspace \
             /out/var/lib/factor-q/build \
             /out/var/lib/factor-q/home \
    && chown -R 65532:65532 /out/var/lib/factor-q

# --- minimal ----------------------------------------------------------------
#
# distroless `cc` rather than `static` so a glibc-linked local build runs
# here too; the published images carry the static musl binaries, for
# which the C library is unused.
FROM gcr.io/distroless/cc-debian12:nonroot AS minimal

# `fqd` is what this image runs; `fq` rides along so an operator can reach
# the daemon from inside the container — with no shell, `docker exec` of
# the client is the only way to ask a wedged daemon anything — and so the
# health check below has something to run.
COPY --chmod=755 dist/bin/fqd /usr/local/bin/fqd
COPY --chmod=755 dist/bin/fq  /usr/local/bin/fq
COPY --from=layout --chown=65532:65532 /out/var/lib/factor-q /var/lib/factor-q

# Every directory the daemon has a setting for points under the volume,
# and these are ENVIRONMENT variables, which outrank `fqd.toml`: an
# operator cannot point the edge identity or the stores outside the
# volume by editing the config, which is the contract. `[workspace] path`
# has no environment form and must be set to /var/lib/factor-q/workspace
# in fqd.toml. `XDG_CONFIG_HOME` is where `fq` keeps its pairing store
# (`fq connect`), so a pairing made inside the container survives a
# recreate. The broker URL carries no credential, ever: the token comes
# from the variable `[nats] token_env` names (FQ_NATS_TOKEN by
# convention), supplied by the environment, never baked in.
ENV FQ_DAEMON_CONFIG=/var/lib/factor-q/fqd.toml \
    FQ_AGENTS_DIR=/var/lib/factor-q/agents \
    FQ_CACHE_DIR=/var/lib/factor-q/cache \
    FQ_STATE_DIR=/var/lib/factor-q/state \
    XDG_CONFIG_HOME=/var/lib/factor-q/state/client \
    FQ_NATS_URL=nats://nats:4222 \
    RUST_LOG=info

# The working directory is the volume root so `fq` finds `fq.toml`
# (its own config, `[daemon] addr`) beside `fqd.toml` without flags.
WORKDIR /var/lib/factor-q
VOLUME ["/var/lib/factor-q"]

USER nonroot:nonroot

# Liveness through the daemon's own front door. `fq status` asks the
# daemon over the authenticated edge and exits non-zero when nothing
# answers, so a healthy container is one whose daemon is serving, not
# merely one whose process exists. It needs a pairing: until `fq connect`
# has been run once inside the container (state/client/ keeps it), the
# probe reports "no daemon paired" and the container shows unhealthy —
# an honest state for an instance nobody has paired with yet. Compose
# may override the timings; the start period covers recovery at boot.
HEALTHCHECK --interval=30s --timeout=10s --start-period=120s --retries=3 \
    CMD ["/usr/local/bin/fq", "status"]

# `fqd` takes no subcommand — it is the daemon. Deliberately no CMD: an
# empty argument list is the whole invocation.
ENTRYPOINT ["/usr/local/bin/fqd"]

# --- toolchains for the dogfood image ----------------------------------------
#
# Each comes from its official image or release rather than a distro
# package, so the version is the one the repo pins. Build stages, not
# layers of the final image: only the installed trees are copied across.
# The versions here are copies of the repository's pins; `just check-pins`
# (scripts/check-pins.sh, in `just quality`) fails when a copy drifts.

# rust-toolchain.toml pins 1.95.0 with rustfmt + clippy; the official
# image installs the minimal profile, so add the two components here.
FROM rust:1.95.0-slim-bookworm AS rust-toolchain
RUN rustup component add rustfmt clippy

# adapters/*/go.mod pin Go.
FROM golang:1.26.3-bookworm AS go-toolchain

# ci.yml's setup-node pins Node 20 (markdownlint via npx, MCP reference
# servers in the smoke tests).
FROM node:20-bookworm-slim AS node-toolchain

# Release binaries fetched with their published checksums. nats-server is
# verified against the repo's own `.nats-checksums`, exactly as
# `just install-nats` does; the others against the checksum file their
# release publishes. Versions are ARGs so a bump is one line.
FROM debian:bookworm-slim AS tools
ARG JUST_VERSION=1.40.0
ARG GH_VERSION=2.74.0
ARG SCCACHE_VERSION=0.10.0
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /dl
COPY .nats-version .nats-checksums ./
RUN set -eu; \
    nats="$(tr -d '[:space:]' < .nats-version)"; \
    file="nats-server-v${nats}-linux-amd64.tar.gz"; \
    curl -sfLO "https://github.com/nats-io/nats-server/releases/download/v${nats}/${file}"; \
    expected="$(awk -v f="$file" '$2 == f {print $1}' .nats-checksums)"; \
    [ -n "$expected" ] || { echo "no checksum for $file in .nats-checksums" >&2; exit 1; }; \
    echo "${expected}  ${file}" | sha256sum -c -; \
    tar -xzf "$file" --strip-components=1 "nats-server-v${nats}-linux-amd64/nats-server"; \
    \
    file="just-${JUST_VERSION}-x86_64-unknown-linux-musl.tar.gz"; \
    base="https://github.com/casey/just/releases/download/${JUST_VERSION}"; \
    curl -sfLO "${base}/${file}"; curl -sfL "${base}/SHA256SUMS" -o just.sums; \
    grep " ${file}\$" just.sums | sha256sum -c -; \
    tar -xzf "$file" just; \
    \
    file="gh_${GH_VERSION}_linux_amd64.tar.gz"; \
    base="https://github.com/cli/cli/releases/download/v${GH_VERSION}"; \
    curl -sfLO "${base}/${file}"; curl -sfL "${base}/gh_${GH_VERSION}_checksums.txt" -o gh.sums; \
    grep " ${file}\$" gh.sums | sha256sum -c -; \
    tar -xzf "$file" --strip-components=2 "gh_${GH_VERSION}_linux_amd64/bin/gh"; \
    \
    file="sccache-v${SCCACHE_VERSION}-x86_64-unknown-linux-musl.tar.gz"; \
    base="https://github.com/mozilla/sccache/releases/download/v${SCCACHE_VERSION}"; \
    curl -sfLO "${base}/${file}"; curl -sfL "${base}/${file}.sha256" -o sccache.sum; \
    echo "$(cut -d' ' -f1 sccache.sum)  ${file}" | sha256sum -c -; \
    tar -xzf "$file" --strip-components=1 "sccache-v${SCCACHE_VERSION}-x86_64-unknown-linux-musl/sccache"; \
    \
    mkdir -p /out && install -m 755 nats-server just gh sccache /out/

# --- dogfood ------------------------------------------------------------------
#
# Debian rather than distroless because a toolchain is a userland: cargo
# links through `cc`, `just ci` shells out, `npx` needs a shell. The two
# factor-q binaries are copied from the `minimal` stage, never rebuilt,
# so the daemon that runs here is byte-for-byte the one held to the bare
# envelope above.
FROM debian:bookworm-slim AS dogfood

RUN apt-get update && apt-get install -y --no-install-recommends \
       ca-certificates curl git jq bash openssh-client python3 \
       build-essential pkg-config \
    && rm -rf /var/lib/apt/lists/*

# The runtime user is uid 65532 — distroless's `nonroot` — so a volume
# seeded by either image is owned identically and the two are
# interchangeable on the same volume. Home is on the volume too: `gh`
# and `git` keep their configuration under it.
RUN groupadd -g 65532 nonroot \
    && useradd -u 65532 -g 65532 -d /var/lib/factor-q/home -M -s /usr/sbin/nologin nonroot

COPY --from=minimal /usr/local/bin/fqd /usr/local/bin/fqd
COPY --from=minimal /usr/local/bin/fq  /usr/local/bin/fq
COPY --from=layout --chown=65532:65532 /out/var/lib/factor-q /var/lib/factor-q
COPY --from=tools /out/ /usr/local/bin/

# Rust: the toolchain's REAL binaries are linked into /usr/local/bin, not
# rustup's proxies. The `exec` tool starts agents' processes with a fixed
# `PATH=/usr/local/bin:/usr/bin:/bin` and no other inherited variable, and
# a rustup proxy without RUSTUP_HOME (or HOME) in its environment cannot
# find a toolchain at all. The real `cargo` locates `rustc` through PATH
# and `rustc` its sysroot through its own path, so they work in an empty
# environment. Consequence: `rust-toolchain.toml` is not consulted here
# (only rustup reads it) — the version is this image's, which is why the
# base tag above must track the pin. rustup itself stays at
# /usr/local/cargo/bin, off the baseline PATH, for an operator shell.
COPY --from=rust-toolchain /usr/local/rustup /usr/local/rustup
COPY --from=rust-toolchain /usr/local/cargo /usr/local/cargo
RUN set -eu; tc="$(ls -d /usr/local/rustup/toolchains/*/)"; \
    for b in cargo rustc rustdoc rustfmt cargo-fmt cargo-clippy clippy-driver; do \
        ln -s "${tc}bin/${b}" "/usr/local/bin/${b}"; \
    done

# Go and Node likewise: real binaries, reachable on the baseline PATH.
COPY --from=go-toolchain /usr/local/go /usr/local/go
RUN ln -s /usr/local/go/bin/go /usr/local/bin/go && ln -s /usr/local/go/bin/gofmt /usr/local/bin/gofmt
COPY --from=node-toolchain /usr/local/bin/node /usr/local/bin/node
COPY --from=node-toolchain /usr/local/lib/node_modules /usr/local/lib/node_modules
RUN ln -s ../lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
    && ln -s ../lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx

# Same contract as `minimal`, plus where the build caches live: under the
# volume's `build/` subtree, so an image update does not cold-build every
# invocation that follows it, and so a prune job has one directory to
# sweep. These reach an agent's processes only if its definition
# allowlists them (`sandbox.env`): the runtime clears the child
# environment by design. The tool list above, and this variable list, are
# the compatibility contract every live agent definition is read against
# before this image replaces the host (#587).
ENV FQ_DAEMON_CONFIG=/var/lib/factor-q/fqd.toml \
    FQ_AGENTS_DIR=/var/lib/factor-q/agents \
    FQ_CACHE_DIR=/var/lib/factor-q/cache \
    FQ_STATE_DIR=/var/lib/factor-q/state \
    XDG_CONFIG_HOME=/var/lib/factor-q/state/client \
    FQ_NATS_URL=nats://nats:4222 \
    RUST_LOG=info \
    HOME=/var/lib/factor-q/home \
    CARGO_HOME=/var/lib/factor-q/build/cargo \
    CARGO_TARGET_DIR=/var/lib/factor-q/build/target \
    RUSTC_WRAPPER=sccache \
    SCCACHE_DIR=/var/lib/factor-q/build/sccache \
    GOCACHE=/var/lib/factor-q/build/go-cache \
    GOMODCACHE=/var/lib/factor-q/build/go-mod \
    GOFLAGS=-buildvcs=false

WORKDIR /var/lib/factor-q
VOLUME ["/var/lib/factor-q"]
USER nonroot:nonroot
HEALTHCHECK --interval=30s --timeout=10s --start-period=120s --retries=3 \
    CMD ["/usr/local/bin/fq", "status"]
ENTRYPOINT ["/usr/local/bin/fqd"]

# --- the adapters and the dashboard --------------------------------------------
#
# Static Go binaries and a static musl dashboard: distroless/static is
# enough. Each binary is its own probe (ADR-0035 clause 8): the process
# serves GET /healthz on a loopback address and `--probe` (the adapters)
# or `probe` (the dashboard) asks it from the same environment, so the
# HEALTHCHECK needs no shell, no curl and no credential. The adapters
# report healthy while attached to the broker (the watcher also while its
# poll loop is cycling); the dashboard while it is serving — none of them
# fails on the daemon's account, which is the daemon's own probe's job.
# Configuration is the environment (GHW_*, FQCRON_*, FQ_DASHBOARD_*,
# FQ_EDGE*), supplied by compose; see each binary's README.

FROM gcr.io/distroless/static-debian12:nonroot AS watcher
COPY --chmod=755 dist/bin/github-watcher /usr/local/bin/github-watcher
USER nonroot:nonroot
HEALTHCHECK --interval=30s --timeout=5s --start-period=30s --retries=3 \
    CMD ["/usr/local/bin/github-watcher", "--probe"]
ENTRYPOINT ["/usr/local/bin/github-watcher"]

FROM gcr.io/distroless/static-debian12:nonroot AS cron
COPY --chmod=755 dist/bin/fq-cron /usr/local/bin/fq-cron
USER nonroot:nonroot
HEALTHCHECK --interval=30s --timeout=5s --start-period=30s --retries=3 \
    CMD ["/usr/local/bin/fq-cron", "--probe"]
ENTRYPOINT ["/usr/local/bin/fq-cron"]

# The dashboard is the one web-facing process and holds an attenuated
# token only (#545); nothing else from the instance's environment
# belongs in this container.
FROM gcr.io/distroless/static-debian12:nonroot AS dashboard
COPY --chmod=755 dist/bin/fq-dashboard /usr/local/bin/fq-dashboard
USER nonroot:nonroot
HEALTHCHECK --interval=30s --timeout=5s --start-period=30s --retries=3 \
    CMD ["/usr/local/bin/fq-dashboard", "probe"]
ENTRYPOINT ["/usr/local/bin/fq-dashboard"]
