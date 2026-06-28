# ADR-0022: Binary distribution, release pipeline, and BSL 1.1 licensing

## Status

Accepted (2026-06-27). Builds on
[ADR-0011](0011-event-bus-and-persistence.md) (the NATS dependency that
shapes the "getting started" story) and the CI-through-`just` convention
in [AGENTS.md](../../../AGENTS.md).

## Context

A visitor to the GitHub repository should be able to download `fq` and
get started quickly, without cloning the repo or building from source.
That goal has three shaping constraints:

- **`fq` is a single self-contained binary** — templates are
  `include_str!`-embedded, so there is nothing to install alongside it.
- **The runtime needs NATS** ([ADR-0011](0011-event-bus-and-persistence.md)).
  A downloaded binary can `fq init`, `fq agent validate`, and
  `fq version` standalone, but *running* an agent needs a broker. So the
  "quick start" must hand the user a NATS too, not just the binary.
- **Commercialization intent.** factor-q is meant to be commercialized,
  with personal use free and organizational/commercial use paid — which
  is a licensing decision, not just a packaging one.

## Decision

### 1. Tag-triggered release pipeline, through `just`

A `v*.*.*` tag push runs `.github/workflows/release.yml`:
check-version → build matrix → publish. Every build step invokes a
`just` target (`check-version`, `build-release`, `package`,
`publish-release`) so the release path stays isomorphic with local dev.
The release is created as a **draft** for manual review before
publishing.

### 2. Target matrix: musl Linux + Apple Silicon

Three targets, built on native runners (no cross-compilation):

| Target | Runner |
|---|---|
| `x86_64-unknown-linux-musl` | `ubuntu-latest` |
| `aarch64-unknown-linux-musl` | `ubuntu-24.04-arm` |
| `aarch64-apple-darwin` | `macos-latest` |

musl gives a **portable static** Linux binary (no glibc-version
coupling). Intel macOS (`x86_64-apple-darwin`) and Windows are **not**
built — Intel Macs are a shrinking slice, and the scope is Linux + macOS.

### 3. Artifact contract and install channels

- **Artifacts:** `fq-<version>-<target>.tar.gz` plus a `.sha256`,
  attached to the GitHub release. The tarball holds the `fq` binary,
  `LICENSE`, and `README.md`.
- **`install.sh`** (`curl -fsSL .../install.sh | sh`) detects platform,
  downloads the matching artifact, verifies the checksum, and installs to
  `~/.local/bin`. This is the primary "quick start" path.
- **cargo-binstall** metadata in `fq-cli` (`[package.metadata.binstall]`)
  for the Rust-toolchain audience.
- **Homebrew tap — deferred** to a future iteration.

The artifact naming is a **contract** shared by `scripts/package.sh`,
`install.sh`, and the binstall metadata; changing it is a breaking
change to all three.

### 4. macOS: unsigned, with quarantine instructions

Binaries are **not** code-signed or notarized (no Apple Developer
account for now). The `curl | sh` installer sidesteps Gatekeeper —
quarantine is applied by browsers/Finder, not `curl` — so the primary
path is unaffected. Users who download the tarball **in a browser** need
`xattr -d com.apple.quarantine ./fq`; this is documented in the release
notes and README. Signing is revisited if/when there's an Apple Developer
identity.

### 5. Version stamping

A `build.rs` stamps semver + git short SHA (with a `-dirty` marker) +
build date + target triple as compile-time env vars, surfaced by an
`fq version` subcommand (and a richer `fq --version`). The Cargo version
is the source of truth; the release pipeline asserts the `vX.Y.Z` tag
matches it, so the tag, the binary, and `fq version` never disagree.

### 6. `fq init` provisions NATS

`fq init` writes a self-contained `docker-compose.yml` (NATS with
JetStream) alongside the project files, so the binary quick start is:
download → `fq init` → `docker compose up -d` → `fq trigger`, with no
repo clone.

### 7. License: Business Source License 1.1

factor-q is licensed under **BSL 1.1** (`BUSL-1.1`), replacing the
placeholder MIT declaration:

- **Free** for an individual's personal, non-commercial production use.
- **Any organizational or commercial use** requires a commercial license
  (contact `licensing@factorq.top`).
- Each release **converts to Apache-2.0 four years** after its
  publication (the BSL Change Date / Change License).

**Why BSL over the alternatives.** The intent — "personal use free, orgs
pay, commercialize" — is a *hard* commercial gate, which is
source-available, not open source. **AGPL was rejected**: it is OSI open
source and *permits* organizational use under copyleft, so it does not
deliver "orgs must pay" (its role would be a soft, copyleft-friction
funnel in a dual-license, not a literal gate). **PolyForm Noncommercial**
was the other finalist; BSL was chosen for its battle-tested adoption
(HashiCorp, CockroachDB, Sentry) and its delayed-open-source clause,
which preserves community goodwill while protecting the commercial window.

## Consequences

- factor-q is **source-available, not open source** — it will not appear
  on OSI/"open source" listings, and some users/orgs are wary of
  non-OSS licenses. This is an accepted trade for the commercial gate.
- **A contributor licensing agreement (CLA/DCO) is required before
  accepting external contributions** — selling commercial licenses
  depends on owning all contributed code. To be set up before the first
  outside PR.
- **The LICENSE (especially the Additional Use Grant) needs legal review**
  before the first tagged release; the current wording is a sound draft,
  not legal advice.
- factor-q's dependencies are permissive (MIT/Apache), so none conflict
  with BSL distribution; worth a `cargo deny` license pass to confirm.
- The release pipeline is **unproven until the first real `v*.*.*` tag**
  — the musl build in particular proves out then, as CI did on its first
  push.
- Code signing and a Homebrew tap are **deferred**; revisit with demand.

## References

- `.github/workflows/release.yml`, `justfile` (release targets),
  `scripts/package.sh`, `install.sh`, `/LICENSE`.
- [ADR-0011](0011-event-bus-and-persistence.md) — the NATS dependency.
