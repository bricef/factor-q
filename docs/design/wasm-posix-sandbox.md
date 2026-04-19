# WASM-native POSIX sandbox

## Status

Exploratory. Captures a specific technical direction for
implementing the shell and file-manipulation tools, distinct
from (and complementary to) the container-based isolation in
the broader tool-isolation model.

Not a commitment. The design needs empirical validation before
adoption — see "What would need to be true" below.

## Context

The tool-isolation model
([`tool-isolation-model.md`](./tool-isolation-model.md))
defines isolation tiers ranging from in-process through
subprocess, container, WASM instance, and microVM. The WASM
tier is listed but thinly described: "Wasmtime sandbox with
hermetic execution, no syscalls by default, bounded memory
and fuel".

This document fleshes out what that tier could actually mean
for one of the most important tool categories — shell and
filesystem operations. The idea is concrete: compile POSIX
utilities (a shell plus a coreutils-equivalent) to WASM, run
them inside a wasmtime instance, and expose a virtual
filesystem backed by the agent's workspace.

The result is a **safe POSIX interface** that models can use
the way they were trained to use a shell, with sandbox
guarantees stronger than subprocess and cheaper than
containers.

## The idea in concrete terms

When an agent invokes `shell` (or any tool that would
traditionally shell out), the tool implementation:

1. Instantiates a fresh wasmtime instance of a pre-compiled
   POSIX-toolkit binary (something like BusyBox compiled to
   `wasm32-wasi-preview2`).
2. Mounts a virtual filesystem into the instance, backed by
   the agent's workspace directory on the host.
3. Runs the requested command inside the instance (`sh -c
   'ls -la | grep foo'`), subject to fuel and memory limits.
4. Captures stdout, stderr, and exit code.
5. Tears down the instance.

From the model's perspective, nothing is different — it
generated a shell command, got back output. From the host's
perspective, no subprocess was spawned, no host binaries
were invoked, no container was started. The entire execution
happened inside a WASM sandbox with explicit capabilities.

## Candidate implementations

### BusyBox-in-WASI

BusyBox bundles hundreds of standard Unix utilities (`sh`,
`ls`, `cat`, `grep`, `find`, `sed`, `awk`, `head`, `tail`,
`sort`, `uniq`, `cut`, `tr`, `wc`, `xargs`, and many more)
into a single binary. There are active efforts to target it
at WASI, producing a single `busybox.wasm` that exposes the
full suite.

Strengths: enormous coverage of what models expect from a
shell; single artefact; mature codebase.

Weaknesses: C codebase, so WASI-portability depends on
upstream keeping pace with the preview 2 interface;
historically has had rough edges where utilities assumed
full POSIX semantics not all of which WASI supports.

### Rust coreutils (uutils) to WASM

`uutils/coreutils` is a Rust reimplementation of GNU
coreutils. Targeting `wasm32-wasi` from Rust is more robust
than from C. Doesn't include a shell — would need to pair
with a WASM-compatible shell.

Strengths: Rust memory safety adds a layer on top of WASM
isolation; active project with regular releases; clean WASI
support likely.

Weaknesses: no bundled shell; narrower coverage than
BusyBox (coreutils only, not the full BusyBox suite).

### Custom minimal shell

A small hand-written shell (Rust, compiled to WASM)
implementing just what's needed: command dispatch, pipes,
redirection, simple globbing, environment variables. Pair
with uutils or BusyBox for the actual commands.

Strengths: full control, minimal surface, can be audited
line by line.

Weaknesses: effort. A shell that handles the range of
things models actually generate is more work than it sounds.
Shell quirks (brace expansion, process substitution, here-
strings) accumulate.

### Existing WASI-native shells

Various projects (`wasi-shell`, `wasix` shell work, research
prototypes) exist at different maturity levels. Worth a
survey before committing to a build.

### WebContainers / Pyodide

Not directly applicable but instructive as existence
proofs:

- **WebContainers** (StackBlitz) runs Node.js in the
  browser under WASM with a virtual filesystem and `npm`.
  Demonstrates that substantial runtimes can be made to
  work in this model.
- **Pyodide** runs CPython in WASM with the full scientific
  Python stack. Demonstrates that large language runtimes
  can be WASM-packaged; a similar approach could provide a
  WASM-native Python tool for agents that need code
  execution.

## The virtual filesystem

The guest shell operates on a filesystem that looks like a
normal Unix tree to it but is actually a capability exposed
by the host.

### Options

**WASI preview 2 filesystem mount.** The host calls
`wasi:filesystem`'s `open-at` for a designated host directory
(the agent's workspace) and passes the resulting descriptor
to the guest. The guest's `/` (or a subtree like `/work`) is
this directory. Reads and writes go through the WASI host
implementation, which enforces boundaries.

This is the canonical approach. Standard, well-supported,
composes with the rest of the WASI ecosystem.

**In-memory tmpfs.** The guest gets a RAM-backed filesystem
that starts empty (or from a seed image). Changes are lost
when the instance tears down. Suitable for ephemeral
computations that don't need persistence.

**Overlay (copy-on-write).** A read-only base layer (the
workspace at invocation start) with a writable upper layer
(in memory or scratch file). Changes to the upper layer can
be discarded on failure or committed on success. Gives
transactional semantics for tool calls — either the whole
call succeeds and the workspace is updated, or it fails and
nothing is changed.

**Layered combinations.** Most ergonomic: the agent's
workspace is mounted read-write at `/workspace`, a
pre-populated reference layer (documentation, base code) is
mounted read-only at `/ref`, and `/tmp` is a fresh tmpfs per
invocation. Tools use paths idiomatically; isolation lives in
the mount configuration.

### Boundary enforcement

All guest filesystem operations go through the host's WASI
implementation. The host:

- Canonicalises paths before dispatching
- Rejects paths outside mounted subtrees
- Applies read/write policy per mount
- Logs operations for audit
- Can rewrite or deny operations based on policy (shadow
  mode, sensitive-path blocking, etc.)

This is the sandbox. It is defined by the mount
configuration, not by the guest's behaviour — the guest
cannot escape it because the WASI spec gives it no way to
reference anything outside the mounts.

## Where this fits in the tool-isolation tiers

Inserting into the tier table from the tool-isolation model:

| Tier | Startup | Isolation strength | Resource ceiling | Network | Notes |
|---|---|---|---|---|---|
| In-process | 0 | None | No | N/A | Pure computation only |
| Subprocess | ~ms | Moderate | OS-level | Via proxy | Current `shell` |
| **WASM POSIX** | **~ms** | **Strong (WASM spec)** | **Fuel + memory** | **None by default** | **Sandboxed shell, files, coreutils** |
| Container per call | ~100ms | Strong (Linux ns) | cgroups | Via proxy | Arbitrary code, language runtimes |
| microVM | ~150ms | Strongest (hypervisor) | Per-VM | Via proxy | Untrusted third-party code |

The WASM POSIX tier sits between subprocess and container.
Its distinguishing properties:

- **Startup is comparable to subprocess** — wasmtime instance
  creation is single-digit milliseconds for a pre-compiled
  module. Much cheaper than container startup.
- **Isolation is comparable to container** — for the
  in-process safety properties. An escape requires a bug in
  wasmtime itself, a much smaller trust surface than the
  Linux kernel.
- **No host process is spawned.** Everything happens inside
  the runtime. No PID, no fork bombs, no process-namespace
  games.
- **Bounded deterministically.** Fuel limits (instruction
  count) and memory limits are precise and enforceable,
  unlike wall-clock-based subprocess timeouts.
- **No network by default.** WASI networking is a separate
  capability; not exposing it means the sandboxed shell
  simply cannot make network calls. The agent's
  network-requiring tools (fetch, curl-equivalent) stay in
  other tiers.

## Workspace-state interaction

The agent's workspace is the natural mount point. Shell
commands read and write files in the workspace; the host's
workspace-state machinery (snapshots, base layers) handles
persistence.

This composes cleanly:

- **Base layers as `/ref` mounts.** Pre-loaded agent data
  mounts read-only inside the sandbox. The model does
  `grep -r pattern /ref/docs` and reads from the base image
  without being able to modify it.
- **Overlay semantics for transactional tool calls.** A
  WASM sandbox with an overlay FS gives you all-or-nothing
  tool behaviour — if the command fails or times out, the
  upper layer is discarded and the workspace is untouched.
  This is hard to get cleanly with subprocesses.
- **Snapshotable upper layer.** The upper layer is host
  storage; existing workspace-snapshot machinery works on
  it as on any other directory.

## Tradeoffs

### Strengths

- **Strong isolation per shell call.** Sandbox escape
  requires a WASM runtime vulnerability, not a kernel
  vulnerability. Smaller trust base.
- **Fast startup.** Instantiating a pre-compiled WASM
  module is order-of-milliseconds. Each shell call pays
  this cost, not hundreds of ms for a container.
- **Hermetic execution.** No subprocess, no PATH lookup, no
  host binary compatibility concerns. The tool behaves
  identically on any host that can run wasmtime.
- **Precise resource control.** Fuel and memory limits are
  enforced at the instruction level. An infinite loop
  terminates exactly when fuel runs out, predictably.
- **Bounded capabilities.** The shell has exactly the
  filesystem and network access the host configures — no
  more. Compromised or malicious commands cannot reach
  beyond it.
- **Transactional semantics possible.** Overlay FS enables
  all-or-nothing tool behaviour that subprocess-based
  implementations struggle to provide cleanly.
- **Portable.** The same WASM binary runs on any host
  architecture that has wasmtime. No cross-compilation for
  arm64/x86_64, no per-distro packaging.

### Weaknesses

- **Not everything compiles to WASI cleanly.** Tools that
  assume Unix-specific syscalls (mount, ptrace, cgroups,
  certain ioctls) don't work. Most user-facing utilities are
  fine, but anything touching system administration or
  low-level features is not.
- **Incomplete POSIX surface.** WASI preview 2 covers most
  but not all of POSIX. Shells and utilities sometimes
  depend on features (signals, process control, tty
  handling) that are partial or missing. The gap narrows
  over time but is real today.
- **No subprocess in the guest.** A WASI guest cannot `fork`
  and `exec` like a normal Unix process. Shell pipelines
  (`ls | grep foo`) require either a shell implementation
  that does pipe handling via in-process job control, or
  running each stage in a separate WASM instance with host-
  mediated pipe plumbing. Both are workable but non-trivial.
- **Debugging is harder.** Trap messages vs. Unix signals;
  fewer tools for introspecting a running WASM instance;
  stack traces through WASM frames.
- **Ecosystem maturity.** Projects exist but are not as
  battle-tested as Linux containers. Expect rough edges.
- **Won't replace containers for language runtimes.**
  Running Python or Node.js inside WASM is possible (see
  Pyodide, WebContainers) but has its own performance and
  compatibility tradeoffs. For "run this Python script",
  container is probably still the right tier.

## What this doesn't replace

- **The `container` tier.** WASM POSIX is good for shell and
  file manipulation but not a universal container
  replacement. Arbitrary code execution (especially in
  languages with complex runtimes) still wants containers
  or microVMs.
- **MCP server isolation.** Third-party MCP servers are
  full programs with their own dependencies; most won't
  reasonably compile to WASM. Containers (or microVMs for
  untrusted ones) remain the right tier.
- **The harness boundary.** This is not a revival of
  harness-in-WASM — that was rejected in the reducer
  design. WASM POSIX is a tool-implementation technology,
  used by specific tools to sandbox their execution. The
  harness stays native and trusted.

## What would need to be true

For this to become a real tier, the following need to hold:

- **A production-grade BusyBox-in-WASI (or equivalent)
  exists.** The POSIX utility bundle has to cover the
  commands models actually generate — enough coverage that
  `shell` using WASM POSIX feels like `shell` using a real
  shell.
- **WASI preview 2 filesystem is sufficient.** The mount-
  based capability model must handle the file operations
  agents actually do (read, write, stat, glob, walk).
- **Pipe handling works in-process or with acceptable
  overhead.** Shell pipelines are common. Either the shell
  implementation handles them internally, or the host
  plumbs pipes between per-command WASM instances cheaply.
- **Startup overhead is genuinely ms-scale.** Measured on
  our actual setup, not in vendor benchmarks. If
  instantiation takes 50ms per call, the "cheaper than
  containers" argument softens significantly.

Empirical validation is required. The answer will be
different in six months than it is now as the WASI
ecosystem matures.

## When to pursue this

Not urgent. Current priority order for factor-q phase 2:

1. Reducer-model harness prototype (resolves the much
   bigger architectural question)
2. Network proxy (load-bearing for every isolation tier
   including this one)
3. Container-based shell (the pragmatic next step over the
   phase-1 subprocess sandbox)

WASM POSIX becomes interesting **after** container-based
shell is working, as a stronger-but-not-yet-proven
alternative to it. At that point we have a concrete point
of comparison ("is WASM POSIX actually better than a
container for our shell workload?") and infrastructure to
validate against.

If the WASI ecosystem matures faster than expected, or if a
specific workload (very frequent small shell calls where
container startup dominates) demands it earlier, the
priority could shift.

## Open questions

### Shell choice

BusyBox's `ash`, a custom Rust shell, uutils + separate
shell — need a concrete pick once we're ready to prototype.
Depends partly on what WASI-portability story has the
cleanest upstream.

### Pipe implementation

Shells depend on process pipes. Options:
- Shell runs in one WASM instance and uses internal job
  control / thread-like primitives to implement pipes.
  Simpler but needs a shell implementation that supports it.
- Each pipeline stage runs in its own instance; the host
  plumbs pipes between them using WASI streams. More
  general but more overhead per stage.

Answer depends on the shell chosen.

### Shared resources across calls

Should two consecutive `shell` calls from the same agent
share anything? (Environment variables set by one, visible
to the other? Working directory persistence?) If yes, the
model becomes "persistent shell session in a WASM instance";
if no, each call is fully fresh. Current answer: fresh per
call, matching the current subprocess model. Revisit if
agents need session state.

### Warm pools

Like any sandbox tier with non-zero startup cost, a pool of
pre-instantiated WASM instances would amortise. Design
question: per-agent pools vs. global, how many, how to
lifecycle them. Same shape as for containers.

### Guest signals

SIGKILL has no WASI equivalent. If the host needs to kill a
runaway guest, wasmtime's epoch-based interruption handles
this — but the guest sees a trap, not a signal. Shell
behaviour under fuel exhaustion needs to be defined and
tested.

## Related documents

- [`tool-isolation-model.md`](./tool-isolation-model.md) —
  the broader framework this tier slots into
- [`wasm-boundary-design.md`](./wasm-boundary-design.md) —
  a different use of WASM (rejected for the harness, kept
  as an option for tools like this one)
- [ADR-0010](../adrs/accepted/0010-agent-execution-isolation.md)
  — isolation strategy including WASM as a future tier

## Summary

WASM-native POSIX tools are a plausible strong-isolation
tier for the shell and file-manipulation category
specifically. The appeal is hermetic execution with
strong sandboxing at container-like isolation strength but
subprocess-like startup cost, plus clean composition with
workspace snapshotting via overlay filesystems.

The key unknowns are ecosystem maturity (does a good
BusyBox-in-WASI exist yet?), POSIX completeness (do the
utilities models expect actually work?), and pipe
implementation (can we match shell ergonomics without
excessive complexity?).

Worth investigating — probably after the container-based
shell tier is working and we can benchmark against it.
