# In-executor parallelism — the fast route (fan-out + per-invocation worktrees)

Status: **active plan** (2026-07-10). The interim, "Route A" path to running
agent invocations concurrently on the dogfood fleet. The concurrency lives
**inside a single `fq run` executor** — one daemon runs N jobs in parallel — not
across multiple `fq run` processes. That in-executor concurrency is issue
**#70**; per-invocation git worktrees (**#14**) are the isolation that makes it
safe to do. Deliberately **not** the ADR-0028 VFS (the safe-by-construction
successor); this delivers *functional* parallelism for our own (trusted) agents
on a single-tenant box, and bakes concurrent recovery/drain/shutdown correctness
in as a first-class gate, not a follow-up.

## Goal & scope

A single `fq run` executor runs up to *N* invocations **concurrently within the
one process** — jobs running in parallel inside the daemon, not N daemons — each
in its own git worktree so they don't corrupt each other's checkout, with drain,
shutdown, and crash-recovery proven correct with *N* in flight.

**In scope**
- **In-executor concurrent execution (#70):** semaphore-bounded fan-out so one
  `fq run` runs N invocations at once, *including* making the executor
  (`ReducerRunner`) safe for concurrent `run_invocation` calls. Default off.
- Per-invocation git-worktree isolation of the workspace (closes the shared
  `~/fq-dogfood/workspace/factor-q` clobber — #14).
- **Concurrent recovery / drain / shutdown correctness** — baked in as Phase 2,
  the gate before concurrency is enabled (per the plan's own requirement).

**Explicitly out of scope** (tracked / deferred, not blockers here)
- The harness-owned VFS (**ADR-0028**) — the safe-by-construction successor that
  makes this safe for *untrusted* agents. This plan is the bridge to it.
- **Shell escape is an accepted risk.** The `ToolSandbox` is check-based and
  in-process (`sandbox.rs:30` "does not mount or chroot"; `shell.rs:47` "No
  network isolation"), so an agent's shell can still read a sibling worktree or
  a host secret by absolute path. On a single-tenant box running our own m0
  agents this risk already exists serially; parallelism multiplies its
  concurrency, not its reachability. We accept it here; ADR-0028 closes it
  structurally. Not filing a separate issue for it.
- Fleet-level cost/rate cap (**#42 / ADR-0004**) — the concurrency bound is the
  only spend guardrail in this plan; a real fleet cap is a fast-follow.
- Network proxy, WASM/FUSE exec tiers (ADR-0028 sequencing).

## Current state (grounded)

- **Dispatcher is strictly serial.** `run()` awaits `handle()` to completion
  before pulling the next trigger (`control_plane/dispatcher.rs:145`), and
  `handle` runs `worker.run_invocation(...)` inline — no `tokio::spawn`
  (`dispatcher.rs:280`), selecting to completion (`:304`). It acks early on the
  `durably_started` signal (`:300`) for the redelivery-storm fix, but still
  waits for the run to finish.
- **Workspace path is hardcoded per agent.** The shell tool takes `cwd` as a
  parameter and checks it against the agent's declared `exec_cwd`
  (`fq-tools/src/builtin/shell.rs:194`, `:237`). The m0 agents declare
  `exec_cwd/fs_read/fs_write: [/home/brice/fq-dogfood/workspace/factor-q]` in
  frontmatter. There is **no workspace-root indirection** — the path is fixed,
  so every invocation shares one checkout.
- **Recovery is already concurrent.** Startup recovery spawns one
  `tokio::spawn` resume task per recoverable invocation
  (`fq-cli/src/main.rs:1556-1613`) and a graceful drain joins those
  `resume_handles` up to the deadline (`main.rs:~2005-2101`). So the *recovery*
  path already runs N in parallel — but the *steady-state* dispatcher path does
  not, and drain does **not** track dispatcher-run invocations (they're awaited
  inline). Fan-out must close that tracking gap.
- **Drain already exists** (`fq drain`, ADR-0027): the dispatcher stops
  consuming when `drain_status() == Draining` (`dispatcher.rs:134`) and in-flight
  work suspends at a step boundary via the shared drain signal the reducer polls.

## Design — three coupled pieces

### 1. Workspace indirection + workspace provider (the enabling change)

> **Superseded in part** — see *Decisions taken while building* below:
> the provider provisions plain per-invocation directories; agents
> clone the upstream themselves. The git-worktree mechanics in this
> section are retained for the record but were not built.

Make the workspace path **per-invocation**. Agents stop naming an absolute path;
they reference the workspace through a token the runtime resolves.

- **Agent surface:** agents use a `${workspace}` token in `cwd` params and in
  `sandbox.exec_cwd/fs_read/fs_write` (e.g. `exec_cwd: ["${workspace}"]`). The
  m0 agent defs migrate to it and drop the hardcoded path from their prompts.
- **Resolution:** each invocation's tool-execution context binds `${workspace}`
  to that invocation's worktree path, so `check_exec_cwd` and the shell's
  `current_dir` resolve to the worktree. *Recommended* over prompt-injecting an
  absolute path (deterministic, doesn't depend on the agent echoing a path
  correctly); prompt-injection of the resolved path is the quicker interim if
  token plumbing proves fiddly.
- **Worktree provider** (per invocation, in the worker/dispatch path):
  - **provision:** `git -C <repo> worktree add --detach <wt-path> origin/main`
    (or a per-invocation branch) — a fresh checkout off the latest `main`, which
    *also* fixes #14's original stale-PR bug.
  - **path:** `~/fq-dogfood/workspace/wt/<invocation-id>/…`.
  - **reclaim:** `git worktree remove` on a **terminal** outcome
    (completed/failed) only — see the lifecycle coupling in §3.
  - **prune:** `git worktree prune` + sweep orphaned worktree dirs on startup
    (belongs with the recovery scan).
- **Config:** `agents.workspace_root` (where worktrees live) and the source repo
  path. A `--no-worktree` / disabled mode keeps today's single-shared-workspace
  behavior for a clean rollback.

### 2. In-executor fan-out (this is #70)

The concurrency lives in the one daemon's executor, not in extra processes. The
executor is already *shaped* for it: `Worker::run_invocation` is `&self`
(`worker/mod.rs:207`) and the dispatcher holds the runner as `Arc<dyn Worker>`
(`dispatcher.rs:99`), so N concurrent calls are structurally possible today —
only the serial dispatch loop and unaudited shared state stand in the way.

- Add `worker.max_concurrent_invocations` (Design Principle 8), **default 1** —
  bit-identical to today's serial behavior until Phase 2 proves the concurrent
  paths.
- In the dispatcher `run()` loop: acquire a semaphore permit, then
  `tokio::spawn` the `handle()` work instead of awaiting it inline
  (`dispatcher.rs:280`/`:304`), and continue the loop to pull the next trigger.
  The permit count bounds in-flight invocations.
- **Make the executor concurrency-safe — the core of this work.**
  `run_invocation` will run N times at once through the one shared
  `ReducerRunner`; audit and harden it for concurrency: per-invocation state
  must be local (never mutated on `&self`), the `WorkerStore` connection/pool
  must tolerate concurrent WAL writes across invocations (SQLite write
  serialization / a pool), the event sink must be concurrent-safe, and per-run
  accumulators (`totals`, budget) must stay per-invocation. Two invocations
  interleaving must never cross-contaminate WAL rows, events, or cost.
- **JetStream consumer:** raise `max_ack_pending` to ≥ `max_concurrent` so the
  work-queue consumer delivers up to N un-acked triggers. (Ack-on-durable-start
  already frees a slot before completion — `dispatcher.rs:300`.)
- **Track the spawned tasks** in a join set the daemon owns, so drain/shutdown
  can wait on them (§3) — inline-await today makes drain tracking implicit;
  fan-out must make it explicit.

**Non-goal — multiple processes.** We are *not* running N `fq run` instances.
One executor with in-process concurrency means one thing to deploy
(`redeploy.sh`), drain (`fq drain`), and observe (`fq status`); it is the
concurrency primitive the graph executor reuses; and it is the only shape that
actually exercises the N-in-flight recovery/drain this plan gates on (N serial
processes never would).

### 3. Concurrent recovery / drain / shutdown — the correctness gate (baked in)

This is **not** a follow-up ticket; it is the gate that must pass before
`max_concurrent` is raised above 1 anywhere. The subtlety is the **worktree ×
invocation-lifecycle coupling**:

- **A suspended invocation keeps its worktree.** On drain or crash, an
  invocation suspends with uncommitted work in its worktree; the worktree must
  **persist** across the restart so `resume` continues from it. Worktrees are
  removed only on a *terminal* outcome, never on suspend. Recovery must
  re-associate a resuming invocation with its persisted worktree (store the
  worktree path in the invocation's durable state / `ConfigSnapshot`).
- **Drain must wait for all N in-flight**, not just recovery-resumes. The
  dispatcher's spawned tasks (§2) join into the same deadline-bounded wait the
  recovery `resume_handles` already use (`main.rs:~2005-2101`); past the
  deadline, stragglers hard-stop and recovery reclaims them next start.
- **Shutdown** (`fq down` #63 / SIGTERM) tears down cleanly with N in-flight,
  deregistering the worker once.
- **Orphaned/ambiguous invocations** under concurrency must stay observable
  (#64) — more in-flight means more recovery-limbo cases.

**Verification (DST, in `test_support::sim::SimWorld`):** drive N concurrent
invocations and assert, for each independently:
1. **Happy path:** N complete concurrently; each reads/writes only its own
   worktree (no cross-contamination); per-invocation budget holds.
2. **Drain mid-flight:** `request_drain` with N running → each suspends at a step
   boundary, its worktree intact → next binary resumes each **exactly once**
   from its worktree (budget-across-resume invariant per invocation).
3. **Crash mid-flight:** kill with N running → recovery resumes each from its
   worktree; ambiguous ones surface (don't silently strand).
4. **Shutdown:** `fq down` with N running → clean teardown, single deregister,
   no orphaned worktrees for terminal invocations.
Extend the existing budget-across-resume property test to the N-invocation case.

## Phasing (a focused sprint; each phase verifiable + shippable behind the flag)

- **Phase 0 — worktree provider + workspace indirection.** `${workspace}`
  resolution, provision/reclaim/prune, m0 agent-def migration. *Verify:* one
  invocation runs entirely inside a fresh worktree off `origin/main`; the shell
  `cwd` resolves; `git status` clean at start (fixes #14's stale-PR bug on its
  own). Ship with `max_concurrent = 1` — already an improvement (stale-PR fix).
- **Phase 1 — in-executor fan-out (#70).** Executor concurrency-safety audit +
  hardening (§2), then semaphore + spawn + `max_ack_pending` + `max_concurrent`.
  *Verify:* two triggers published back-to-back run concurrently in **one
  daemon** (no `lag 1` wait), neither clobbers the other, and their WAL rows /
  events / costs stay separate; `fq status` shows both in-flight.
- **Phase 2 — concurrent recovery/drain/shutdown (the gate).** The §3 DST sweep +
  live `fq drain` / `fq down` with N in-flight on a scratch daemon. **Do not**
  raise the dogfood `max_concurrent` until this is green.
- **Phase 3 — enable on the box.** Set `max_concurrent = N` + worktrees on in the
  ops `fq.toml`; redeploy via `redeploy.sh`; watch cost + recovery state.

## Risks & accepted trade-offs

- **Shell escape / cross-worktree reads** — accepted (trusted single-tenant
  fleet); ADR-0028 is the structural fix. See scope.
- **Concurrent spend** — bounded only by `max_concurrent` here; a fleet cap is
  #42 / ADR-0004 (fast-follow).
- **Worktree disk + `.git` contention** — N worktrees share one object store;
  concurrent `git worktree add`/fetch on the same repo can contend. Provision
  from a dedicated bare/mirror clone if lock contention shows up.
- **Recovery re-association bug** would resume an invocation against the wrong
  (or a pruned) worktree — Phase 2's DST is precisely the guard.

## Interlocks

- **#14** — this *is* its preferred (worktree) fix; supersedes its "minimal"
  sync-before-run.
- **#70** — this *is* its in-executor concurrent execution (one instance, N
  jobs); keep the ticket for the cross-cutting notes (cost, ordering).
- **ADR-0027** — reuses `fq drain`; extends its wait to N in-flight.
- **#63 (`fq down`)**, **#64 (loud non-terminal exits)** — exercised under
  concurrency by Phase 2.
- **ADR-0028** — the safe-by-construction successor; this plan is the interim
  bridge and should be retired when the VFS lands.


## Decisions taken while building

- **2026-07-10 — git is out of the runtime; worktrees dropped for
  per-invocation directories.** Building Phase 0 surfaced that a
  `GitWorktreeProvider` makes the runtime shell out to a host binary and
  bakes git vocabulary (`base_ref`, `worktrees_dir`) into the operator
  config — the wrong shape, and a red line. Decision: the runtime
  provisions **directories only** (`[workspace] path` +
  `per_invocation`), pure `std::fs`; **agents clone the upstream into
  `${workspace}` themselves** through their granted tools, which is
  where the M0 loop already does its git (branch, commit, push). A
  fresh clone starts from the latest upstream by construction, which
  fixes #14's stale-base bug more directly than fetch-then-worktree —
  and per-invocation clones share no `.git`, so the worktree-lock
  contention risk in §Risks disappears. The `WorkspaceProvider`
  seam, the `workspace_ref` re-association, and the suspend-keeps /
  terminal-reclaims / startup-prunes lifecycle are unchanged; ADR-0028's
  VFS remains the successor behind the same seam. Cost: a clone per
  invocation instead of a shared object store — acceptable at dogfood
  scale, and shallow clones are available to the agent if it ever
  matters.
