# Agent Isolation — Discussion Context

## Status

Handover (2026-09-17). This is **not a plan and not a design**. It captures
the context a design discussion about agent execution isolation needs, so
that the discussion can start in a fresh session without re-deriving where
the sandbox stands, which escapes were found and fixed, which issues carry
the staged options, and which decisions were already taken. It was written
at the end of a long maintenance session (the 2026-09-10 backlog review and
fleet rounds 1–9) in which the sandbox's file-path half was hardened but the
isolation model itself was deliberately left for a dedicated conversation.

The design the discussion should produce lands as an ADR revision or a plan
under `docs/plans/active/`; this file is deleted or folded in when that
happens.

## The question on the table

Two open issues, [#400](https://github.com/bricef/factor-q/issues/400) and
[#208](https://github.com/bricef/factor-q/issues/208), were read as partial
band-aids on a properly designed and verified sandbox. That reading is
mostly right:

- **#208** is explicitly "option B" of a three-step ladder written into
  [#35](https://github.com/bricef/factor-q/issues/35): A, warn that
  `sandbox.network` is unenforced (shipped); B, a CONNECT-filtering forward
  proxy injected into the child environment via `HTTPS_PROXY`; C, the
  container and network namespace of
  [ADR-0010](../../adrs/accepted/0010-agent-execution-isolation.md)
  ([#209](https://github.com/bricef/factor-q/issues/209)). B is bypassable by
  anything that unsets the env or opens a raw socket, and the issue says so.
  What keeps it from being throwaway: ADR-0010 names "a network proxy
  enforces the `sandbox.network` allowlist at the container boundary" as a
  component, so B's proxy is the C component built early.
- **#400** is half band-aid, half design gap. Its cheap fix,
  `prctl(PR_SET_DUMPABLE, 0)` on the daemon, closes one reproduced path (the
  exec'd child reading `/proc/<fqd>/environ`) and was **dispatched to the
  fleet on 2026-09-17** as a one-line change. Its "better" fix, not holding
  provider keys and the GitHub token in the daemon's environment at all, is a
  property of where secrets live, and containers do not give it to you: a
  containerised child with the daemon's env injected is still leaky.

So the discussion is really about ADR-0010 tier 1 plus secret placement,
with #208's proxy as one component, and it deserves a design session rather
than incremental issues.

## Where the sandbox stands today (verified against `main` @ `5bba29b4`)

**Process model.** `exec` is argv-only and process-level
(`services/fq-runtime/crates/fq-tools/src/builtin/exec.rs`). The child runs
as the same uid as `fqd`, as its direct descendant, with `env_clear()` plus a
fixed `PATH` and the agent's `sandbox.env` allowlist. No seccomp, no
namespaces, no cgroups; the module's own "Known gaps" list names PATH,
network, cgroups and seccomp. Yama `ptrace_scope=1` does not help because a
descendant may be traced.

**Filesystem checks.** `ToolSandbox::check_read` / `check_read_dir` /
`check_write` / `check_exec_cwd`
(`services/fq-runtime/crates/fq-tools/src/sandbox.rs`) canonicalise and
check prefixes in-process. Hardened in September 2026:

- the dangling-symlink write escape
  ([#399](https://github.com/bricef/factor-q/issues/399), verified PoC) is
  closed at two layers: `symlink_metadata`-based resolution with a post-join
  re-check, and `O_NOFOLLOW` on the actual open in `file_write` (PR #792);
- writes to non-regular files (FIFO hang, directories, sockets) are refused
  at the check and again after open with `O_NONBLOCK` plus `fstat` (PR #803,
  [#800](https://github.com/bricef/factor-q/issues/800));
- reads already required a regular file
  ([#547](https://github.com/bricef/factor-q/issues/547)).

Documented and **not** closed by any in-process check: a check-then-open
race on an *intermediate* directory (rename a dir, symlink it outside;
only `openat` relative to a held parent handle closes it), and hard links,
which a path-based sandbox cannot see (no privilege gain with
`fs.protected_hardlinks=1`). Property tests over path shapes exist
(`sandbox/proptests.rs`, [#411](https://github.com/bricef/factor-q/issues/411)).

**MCP servers** run as child processes of the daemon with `env_clear()`, a
pinned `PATH`, declared `env:` and a neutral cwd (production-readiness
Phase 0, finding A2). Their outbound traffic is unconstrained.

**The coarse boundary that does exist.** The dogfood instance runs the whole
daemon in one compose container
([ADR-0035](../../adrs/accepted/0035-container-image-and-compose-supervision.md)),
so the *host* is protected. Nothing separates an agent's child from the
daemon: every fleet invocation runs `just ci` on code from a pull request,
as the daemon's uid, inside the process tree that holds the NATS token, the
provider API keys and a GitHub token with push rights to this repository.

**Secrets and tokens.** Provider keys and `GH_TOKEN` reach the daemon through
its environment (`ops/dogfood/env.example`); `[nats] token_env` was
introduced in Phase 0 so the broker credential is no longer in the startup
payload. The fleet's GitHub token has `repo` scope and deliberately not
`workflow` scope (it cannot push `.github/workflows` changes; seen on
PR #802). The backlog groomer shares the fleet's push token while ingesting
attacker-controlled issue text
([#402](https://github.com/bricef/factor-q/issues/402), needs a human to
mint an issues-only PAT). `sandbox.exec` dominates the `env` and `fs` grants
entirely (2026-07-25 review §3.8): an agent with exec never had the
confidentiality the allowlists appear to offer.

## The threat model as discussed

Two inputs are attacker-controlled by design of the M0 loop:

1. **Issue text.** The fleet reads GitHub issues written by anyone with
   access to the tracker; the groomer reads every open issue. Prompt
   injection is the expected attack.
2. **Code under test.** `m0-issue-fix` runs `just ci` on the working copy it
   just changed, and Dependabot PRs pull new dependency versions; build
   scripts and tests execute arbitrary code.

The blast radius today is everything in the daemon's process tree: the
provider keys (spend), the GitHub token (push to `main` behind the merge
gate, PR spam, issue edits), the NATS token (publish triggers, read the
event log), and the instance volume (agent definitions, state). The compose
boundary keeps it off the host.

## The issue and ADR ladder

| Item | Role | State (2026-09-17) |
|---|---|---|
| [#35](https://github.com/bricef/factor-q/issues/35) | Option A: warn that `sandbox.network` is declared but unenforced | shipped |
| [#208](https://github.com/bricef/factor-q/issues/208) | Option B: CONNECT-filtering forward proxy, env-injected; defence in depth, not a boundary | open, `fleet:needs-decision`; the review sequenced it first after Phase 2 |
| [#209](https://github.com/bricef/factor-q/issues/209) | Option C: ADR-0010 containers + network namespace; the epic | open, undecomposed |
| [#400](https://github.com/bricef/factor-q/issues/400) | `/proc` credential read (PoC) | band-aid dispatched 2026-09-17; secret placement open |
| [#402](https://github.com/bricef/factor-q/issues/402) | groomer gets an issues-only PAT | open, human step |
| [#399](https://github.com/bricef/factor-q/issues/399), [#800](https://github.com/bricef/factor-q/issues/800) | write-path escapes | closed (PRs #792, #803) |
| [ADR-0010](../../adrs/accepted/0010-agent-execution-isolation.md) | tier 1 containers (read-only root, bind mounts from `fs_read`/`fs_write`, proxy at the boundary, env injection, cgroups, MCP inside); tier 2 microVMs | accepted 2026, unbuilt |
| [ADR-0028](../../adrs/accepted/0028-tool-scoped-isolation-and-workspace.md), [ADR-0029](../../adrs/accepted/0029-fuse-binding-crate.md) | tool-scoped isolation and the workspace; FUSE binding | accepted; FUSE/VFS parked by the review |
| [ADR-0018](../../adrs/accepted/0018-mcp-server-initiated-execution.md) | server-initiated MCP execution (sampling, elicitation) | built; `includeContext` redaction ([#345](https://github.com/bricef/factor-q/issues/345)) open |
| [Agent identity and attestation](agent-identity-and-attestation.md) | identity model with a hard dependency on isolation | design-ahead |

## Decisions already taken that bound the design

- **Pre-alpha:** no back-compat, rollback or migration obligations; manual
  upgrade steps are acceptable (`STATUS.md` "Maturity").
- **Sequencing:** the 2026-09-03 production-readiness review parks
  containers ([#209](https://github.com/bricef/factor-q/issues/209)) until
  its Phase 2 ("the record is trustworthy") is done, and names the egress
  proxy (#208) as the first item after it. As of this handover, Phase 2's
  untracked items are being filed as refined issues; the multi-node hold
  ([#414](https://github.com/bricef/factor-q/issues/414)) is one issue
  (#400) from lifting.
- **Design method:** domain first, structure carrying semantic weight, no
  codegen, no `include!` splicing, size ratchets (`just lint-sizes`) that
  forbid growing frozen files; the sandbox should get the reducer's
  verification bar (trace oracle, DST, property suites) rather than tests
  written after the fact.
- **Ops shape:** one compose stack per instance, images per binary tagged by
  commit, hourly `deploy --auto` with an idle check, no host crontab
  ([ADR-0036](../../adrs/accepted/0036-ops-image-and-scheduler-service.md)). Any isolation
  substrate has to work from inside that stack.
- **Fleet token stays narrow:** widening it to `workflow` scope was
  considered and not taken; CI wiring is applied by a maintainer session.

## Questions the discussion needs to settle

1. **Substrate.** Per-invocation containers launched from a daemon that is
   itself a compose container: Docker socket access (root-equivalent, the
   thing ADR-0010 is meant to remove), a rootless runtime (podman, or
   `bwrap` with user namespaces), or straight to tier 2 microVMs. The devbox
   already runs a customised `bubblewrap` AppArmor profile for Claude Code's
   sandbox; the same primitive is a candidate.
2. **Filesystem mapping.** How `fs_read` / `fs_write` prefixes become bind
   mounts and a read-only root without losing the symlink semantics just
   fixed; whether the in-process checks stay as defence in depth or go;
   what the workspace ([ADR-0028](../../adrs/accepted/0028-tool-scoped-isolation-and-workspace.md))
   looks like when the container owns it.
3. **Network.** The proxy at the namespace edge (reuse #208's design:
   CONNECT allow-list by host pattern, no TLS interception), DNS inside the
   namespace, and what `sandbox.network` means for MCP servers.
4. **Secrets.** Per-call delivery instead of inheritance: `api_key_file`,
   a credential process, short-lived tokens; which credentials the agent
   child legitimately needs (the GitHub token for `gh pr create`, nothing
   else) and how the groomer/fleet split ([#402](https://github.com/bricef/factor-q/issues/402))
   fits.
5. **Build workloads.** The fleet's real work is `just ci`: cargo target
   directories of 5–25 GB, disk pressure that has killed the daemon before,
   and build times that dominate an invocation. Isolation must not make
   every invocation a cold build; a shared, read-only dependency cache with
   a per-invocation target directory is the obvious shape and needs
   deciding.
6. **Resource limits and observability.** cgroups per invocation; what the
   dashboard and `fq doctor` show; how a killed-for-memory child surfaces.
7. **Verification.** An adversarial suite that *tries the escapes* (every
   variant from the #792 and #803 reviews, `/proc` reads, raw sockets,
   proxy-env unsetting, hard links, TOCTOU swaps) as tests that must fail
   to escape, run in CI against the real substrate.
8. **Sequencing.** Whether to build the proxy first (the review's answer),
   the whole tier 1 at once, or secret placement first because it shrinks
   the blast radius most for the least work.

## Pointers

- Code: `services/fq-runtime/crates/fq-tools/src/builtin/exec.rs`,
  `services/fq-runtime/crates/fq-tools/src/sandbox.rs`,
  `services/fq-runtime/crates/fq-tools/src/builtin/file_write.rs`,
  `services/fq-runtime/crates/fq-runtime/src/mcp/`.
- Reviews: [2026-07-25 cleanroom review](../../reviews/2026-07-25-factor-q-cleanroom-review.md)
  §3.1–3.8 (the PoCs), [2026-09-03 production-readiness review](../../reviews/2026-09-03-production-readiness-review.md)
  (findings A1–A9, the phase plan and "what not to start before Phase 2"),
  [2026-09-10 backlog review](../../reviews/2026-09-10-backlog-review.md).
- The adversarial review comments on PRs #792, #803 and #807 list the
  escape variants tried and the ones that remain documented rather than
  closed.
