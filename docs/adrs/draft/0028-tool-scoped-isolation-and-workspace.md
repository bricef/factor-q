# ADR-0028: Tool-scoped isolation and a harness-owned workspace

## Status

Draft. Supersedes the agent-scoped framing of
[ADR-0010](../accepted/0010-agent-execution-isolation.md); ratifies the
committed [tool-isolation model](../../design/committed/tool-isolation-model.md)
and extends it with a harness-owned virtual filesystem. It is a single
application of [design principle 3 — safe by construction, not by
restriction](../../design/committed/design-principles.md).

## Context

Phase 1 shipped a process-level sandbox: path canonicalisation for the file
tools, `exec_cwd` for the shell tool, output caps. ADR-0010 accepted
"containers by default" — the whole agent invocation inside one container — as
the next tier.

Two shifts changed the picture:

- [ADR-0014](../accepted/0014-agent-harness-as-reducer.md): the harness is a
  pure function that returns a `NextAction`. It cannot execute anything, so it
  is not an attack surface, and the host has total control over what runs.
- The [tool-isolation model](../../design/committed/tool-isolation-model.md)
  drew the consequence: the security boundary belongs around each *tool*, not
  the agent as a whole. Each tool has its own threat profile and its own
  isolation need; wrapping the whole agent is both overkill for pure tools and
  insufficient for those that execute untrusted code.

The dogfood loop then surfaced phase-1's gaps as concrete issues: the
`sandbox.env` allowlist is not plumbed to the shell child (#34);
`sandbox.network` is declared but never enforced — ambient egress (#35); the
workspace is a single shared directory, not per-invocation (#14); and a shell
tool with a path allow-list let an agent read `~/.config/gh` via `gh`, because
the allow-list guards the *file tool*, not a spawned subprocess. These are not
new design — they are the gap between phase-1 and the tool-isolation model, and
the shell leak is the signature failure mode of *restriction*.

The graph executor (the next milestone) turns these from tech-debt into
blockers: concurrent nodes cannot share one serial workspace, and fan-out
multiplies every un-bounded surface. The isolation contract must be settled
first.

## Decision

Adopt **tool-scoped isolation, safe by construction**, with a **harness-owned
virtual filesystem** as the shared workspace-and-sandbox substrate.

1. **The agent has no ambient authority.** Its entire capability is the
   declared-tools list ([ADR-0005](../accepted/0005-agent-definition-format.md));
   the host validates every `NextAction::CallTool` against it. There is no
   agent-visible general shell to restrict.
2. **Isolation is per tool, by tier.** Each tool declares the tier its work
   needs: **in-process** (pure/native, no sandbox), **WASM** (wasmtime,
   hermetic, fuel/memory-bounded — the
   [WASM-POSIX](../../design/aspirational/wasm-posix-sandbox.md) shell/file
   direction), **host-binary** (a real process, used only where a tool cannot
   be ported), and — for untrusted third-party or user tools —
   **container/microVM**. Overhead is paid only where the work needs it.
3. **The workspace is a virtual filesystem the harness owns**, behind a
   `FileSystem` trait (a scoped jail + a read-only base layer + a writable
   upper — the shape the `virtual-filesystem` crate's `ScopedFS`/`RocFS`
   already provide), **backed by fq-store's CAS**. Each invocation gets its own
   instance — closing #14 and making graph concurrency safe by construction (no
   shared directory to clobber). The agent's *entire* filesystem reality is
   this mount; there is no host path to escape to, so the shell-leak class is
   closed structurally, not by a check.
4. **The VFS binds to each tool by the cheapest mechanism that fits, over one
   backing store:** the in-process `FileSystem` trait for native tools (sect, a
   git-library tool), a **WASI mount** for WASM tools, and a **FUSE mount** for
   a host binary that cannot be ported (`cargo`/`rustc`) — all the same
   fq-store-backed VFS, so a file sect edits in-process and a build reads over
   FUSE see one consistent store, with no copy-and-fold step.
5. **State is reconciled natively.** The VFS backend, sect's content hashes,
   git's object model, and fq-store's CAS are one content-addressed store:
   files are blobs, directories trees, a workspace snapshot a commit.
   Workspace persistence across a drain-suspend, per-file undo (sect
   `restore`), optimistic locking (sect `--expect HASH`), git operations, and
   the WAL therefore share one object model instead of three reconciliation
   systems.
6. **Narrow typed tools replace the general shell**
   ([ADR-0016](../accepted/0016-typed-operations-no-free-form-apis.md)). Our
   own agents get file editing via `sect-core` (in-process, native — sect's own
   design targets Factor-Q integration), a `Git` tool scoped to known-safe
   subcommands, a build/test tool, and `WebFetch` — not an open shell.
   Narrowing the surface removes most of the isolation burden *before* any
   heavy tier, and may let us skip the container tier entirely for first-party
   agents; the residual open-ended-exec need is the WASM tier.
7. **The network proxy is the single egress choke-point.** No tool has ambient
   sockets; every network-touching tool reaches the network only through the
   proxy, which enforces a per-tool allow-list, injects credentials the tool
   never sees, rate-limits, and audits. (This keeps ADR-0010's "the network is
   the trust-enforcement point".)

**Sequencing:** VFS-on-fq-store (fs + workspace + state, one seam) → network
proxy (egress + credential injection) → code-exec tiers (WASM-mount-the-VFS;
FUSE for host binaries). The VFS is the single biggest lever — it takes the
filesystem half of #14, #34, and #35 in one move.

## Rationale

- **Safe by construction (principle 3).** A path allow-list is a restriction
  the shell bypassed; a VFS whose only mount *is* the workspace is construction
  — the unsafe path does not exist. This is strictly stronger (nothing to leave
  a gap in) and shrinks the trusted base to a few primitives (the VFS backend,
  wasmtime, the tool set).
- **Tool-scoped follows from the pure-function harness.** With the harness
  unable to execute, the only meaningful boundary is each side-effectful tool.
  One container around the whole agent over-isolates the pure parts and
  under-isolates the untrusted ones.
- **The VFS unifies four concerns** — workspace isolation, filesystem
  sandboxing, base layers, and state — into one seam, and reusing fq-store,
  built as the persistence layer for agent execution, makes git, snapshots, and
  the WAL one store.
- **Narrowing beats hardening.** A narrow tool needs only cheap isolation;
  retiring the general shell removes the surface that would otherwise force the
  container tier.

## Consequences

- **#14 / #34 / #35 become slices of this model**, not separate patches: #14 is
  the per-invocation VFS; #34 is credential injection via the proxy; #35 is the
  proxy's egress enforcement.
- **`sect-core` needs a backend trait.** It binds `std::fs` + an XDG snapshot
  store today; taking a harness backend means its I/O and snapshots go behind a
  trait factor-q implements over the CAS-VFS — the sibling of its existing
  "core is format-blind" seam.
- **A tool-result ABI opportunity.** sect's structured result
  (`old_hash`/`new_hash`/diff/notes) is a candidate shape for every in-process
  tool's result, giving the WAL and the transcript command (#47) uniform, rich
  payloads.
- **Composes with drain and graph.** A drained invocation's workspace persists
  as a CAS snapshot and resumes exactly; concurrent graph nodes each hold their
  own VFS instance.
- **Costs.** Building the CAS-backed `FileSystem`, the network proxy, the FUSE
  binding, and the `sect-core` backend refactor is real work, sequenced above.
  The container/microVM tiers stay unbuilt until an untrusted-agent workload
  needs them.

## Relationship to ADR-0010

ADR-0010's still-valid core is kept: containers/microVMs as the isolation tier
for *untrusted* code, and the network proxy as the trust-enforcement point.
What this ADR supersedes is its **unit of isolation** — not the agent
invocation wrapped in a container, but the individual tool (and the workspace,
for state). Per-agent containers may still appear, for workspace encapsulation
or aggregate resource limits, but no longer as the primary security boundary.

## Alternatives considered

- **Agent-scoped OS container (ADR-0010 as written).** Over-isolates the pure
  harness, under-isolates per-tool, and — as the shell leak shows — still
  relies on in-container restriction. Retained only as the tier for genuinely
  untrusted tools.
- **Per-invocation git worktrees** (for #14). Rejected: a shared `.git` object
  store is leakier isolation, couples the workspace to git, and gives no
  filesystem sandboxing. The VFS's read-only base-layer-from-a-ref achieves the
  "seed a checkout" goal without worktrees.
- **Real-FS stable-path workspaces** (the tool-isolation model's phase-2
  starting point). A reasonable first rung, superseded here because the VFS
  delivers isolation *and* native state in the same seam, and the drain work
  (ADR-0027) already makes workspace-persistence load-bearing sooner than that
  doc anticipated.

## Open questions

- **Content-addressed `FileSystem` on fq-store** — the exact blob/tree/commit
  shaping and how snapshots map to CAS objects (a dedicated design pass,
  extending the tool-isolation model's workspace-snapshot open question).
- **Host builds: FUSE now, WASM later?** — the `cargo`/`rustc` residual gets
  its filesystem by construction over FUSE but retains OS-level ambient
  authority on other axes (sockets, subprocess); how far to supplement with a
  namespace versus push builds toward WASM.
- **The `sect-core` backend-trait shape** — where it lives (in sect, with
  factor-q providing the implementation) so sect stays independently useful.
- **Warm-pool lifecycle** for the WASM/FUSE tiers (per-agent vs global),
  deferred until prototype performance data exists.
- **The tool-result ABI** — whether sect's result shape becomes the model for
  all in-process tools.

## References

- Principle: [safe by construction, not by restriction](../../design/committed/design-principles.md)
  (design principle 3).
- Supersedes/updates [ADR-0010](../accepted/0010-agent-execution-isolation.md);
  builds on [ADR-0014](../accepted/0014-agent-harness-as-reducer.md),
  [ADR-0016](../accepted/0016-typed-operations-no-free-form-apis.md),
  [ADR-0005](../accepted/0005-agent-definition-format.md).
- Design: [tool-isolation-model](../../design/committed/tool-isolation-model.md),
  [wasm-posix-sandbox](../../design/aspirational/wasm-posix-sandbox.md).
- Substrate: fq-store (CAS); `sect` (`sect-core` + its DESIGN.md); the
  `virtual-filesystem` crate (trait + `ScopedFS`/`RocFS`/`MountableFS`).
- Issues: #14 (workspace isolation), #34 (env plumbing), #35 (network
  enforcement), #47 (transcript / tool-result), #49 (bounded redelivery).
