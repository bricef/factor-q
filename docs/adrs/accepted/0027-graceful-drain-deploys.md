# ADR-0027: Deploys are a graceful drain — suspend and resume, not kill-and-replace

## Status

Accepted (2026-07-07). Prompted by the M0 flywheel: merged improvements to
factor-q's own runtime currently require a manual rebuild + `pkill` +
restart, which will bottleneck the loop as it accelerates and is itself a
human step that breaks the autonomy goal. This ADR scopes the **deploy
model** — how a stateful, durable-execution daemon is replaced without
losing or corrupting in-flight work. The surrounding continuous-delivery
*pipeline* (image build on merge, an auto-deploy watcher, health-gating) is
context here and is captured separately; this ADR fixes the drain/suspend/
resume model those depend on.

## Context

The dogfood daemon is stateful and long-running: it holds in-flight agent
invocations, the reducer write-ahead log and its recovery path, and durable
NATS/JetStream consumers with ack semantics. A naive kill-and-replace
interrupts in-flight invocations mid-step — wasteful at best, and unsafe
where a step has an external side effect (an agent invocation killed between
a `git push` and its next step).

The load-bearing observation is that **factor-q is already a durable-
execution runtime, so a deploy is a *controlled* crash-and-recover.** The
machinery built for crash recovery is exactly the deploy primitive:

- the reducer **WAL** owns in-flight durability;
- **recovery** (`scan_in_flight → categorise → resume`) restores in-flight
  invocations after a restart;
- the per-invocation **`ConfigSnapshot`** (ADR-0020) preserves the config an
  invocation began with, across the swap;
- **ack-on-dispatch** (the redelivery-storm fix) plus JetStream durable
  consumers guarantee no trigger is lost or double-run.

Suspend/resume is therefore not adjacent to continuous delivery here — it
*is* the deploy mechanism.

## Decision

1. **Deploys are a graceful drain, not kill-and-replace.** Replacing the
   running daemon proceeds by draining and suspending in-flight work to a
   clean checkpoint, then resuming it under the new binary — never by
   interrupting it and relying on crash recovery for the common case.

2. **`fq drain` is the mechanism** — a control command in the same family as
   `fq reload` (control message → daemon acts). On receipt the daemon:
   a. stops consuming new triggers (pauses/unsubscribes the trigger
      consumer);
   b. lets each in-flight invocation run to its next **step boundary** and
      **suspend** there — its state already checkpointed to the WAL;
   c. exits cleanly once drained.
   The new binary starts and **recovery resumes** the suspended invocations
   from the WAL, each with the `ConfigSnapshot` it began with.

3. **Bounded wait, with a defined fallback.** A step that wraps a long tool
   call (e.g. a `just ci` build) will not reach a boundary quickly, so
   `fq drain` waits gracefully only up to a deadline *T*; past *T* it hard-
   stops and lets ordinary crash-recovery pick up the remainder. Graceful-
   with-a-deadline — never block forever.

4. **Correctness rests entirely on existing invariants.** A deploy is a
   *planned* recovery cycle and inherits the same guarantees crash recovery
   already provides: no lost work, no double-execution, no dropped triggers.
   No new durability machinery is introduced by this decision.

## Rationale

- **Why graceful over kill-and-replace, when recovery would "work" anyway.**
  A hard kill mid-step can interrupt a tool call whose external side effect
  has already happened but whose result is not yet persisted — the classic
  case being an M0 agent killed between a `git push` and its recording.
  Crash-recovery would then re-run that step, potentially re-doing a
  non-idempotent side effect. Draining to a step boundary suspends *between*
  steps, after a step's result is durably recorded, so resume continues
  *past* completed side effects rather than repeating them. The graceful
  path is strictly safer for exactly the external-side-effect work the M0
  fleet does; kill-and-replace is the fallback, not the default.

- **Why a control command over an external signal.** `fq drain` reuses the
  `fq reload` control-plane pattern; the daemon owns the graceful-shutdown
  logic because only it knows its step boundaries and consumer state. A
  control command is also testable and composable with the deploy
  orchestrator.

- **Deploys validate recovery.** The recovery path is normally exercised
  only by real crashes — rare, hard to test in production. Deploys exercise
  suspend/resume *deliberately and repeatedly*, making continuous delivery
  the best ongoing validation of the durable-execution core.

## Consequences

**Positive.** Closes the flywheel's last manual step. Zero-lost-work, side-
effect-safe deploys. The control-plane pattern generalises cleanly
(`fq reload` → `fq drain`). Suspend/resume gains a continuous, controlled
exerciser.

**Costs and risks.**

- The bounded-wait tradeoff: a deploy either waits out an in-flight build or
  hard-stops it; there is no free lunch when a step is mid-flight.
- **Auto-deploy requires a health-check + rollback safety layer.** A
  CI-green change can still crash the live workload; a merge must not be able
  to take the daemon down irrecoverably. Deploy → health probe → roll back to
  the previous image on failure. This is the deploy-side mirror of the
  trigger-side [cost-control gap](https://github.com/bricef/factor-q/issues/42): *every place
  autonomy replaces a human checkpoint needs a safety layer where the
  checkpoint was.* It is likely its own companion decision.
- Packaging the daemon as a versioned container image (rather than a raw
  binary) is an ops-maturity step — it composes with the existing
  containerised infra and with [ADR-0010](../accepted/0010-agent-execution-isolation.md)
  isolation, but it is real work.

**Interlocks.** Builds directly on the reducer WAL/recovery, `ConfigSnapshot`
(ADR-0020), and ack-on-dispatch; composes with ADR-0010 (containers /
isolation) and reuses the [ADR-0026](../accepted/0026-event-log-system-of-record.md)
event log for the audit of a deploy as a first-class event.

## Alternatives considered

- **Kill-and-replace (naive restart).** Correct via recovery, but interrupts
  in-flight steps and risks re-running non-idempotent side effects. Retained
  only as the post-deadline *fallback*, not the default.
- **Blue-green / two daemons sharing NATS.** Run the new daemon alongside the
  old and shift triggers across. More coordination (two consumers on the same
  streams) than the single-tenant model needs; deferred as a possible future
  once multi-daemon (horizontal) operation exists.
- **External process manager only** (systemd restart on a new binary) with no
  drain. Simple, but no graceful suspend — it *is* kill-and-replace with a
  supervisor. The graceful drain is the whole point.

## Open questions (deferred by decision)

- **Step-boundary reachability.** Is a boundary always reachable in bounded
  time, and what is the precise idempotent resume point for a step that has
  already emitted an external side effect? (The safety argument above assumes
  the result is persisted at the boundary; the mid-step hard-stop case
  inherits crash-recovery's existing categorisation.)
- **The drain deadline *T*** and the exact hard-stop fallback semantics.
- **Health-check definition** (consumers connected? a synthetic trigger
  round-trips?) and the **rollback mechanism** (image pinning, versions
  retained) — likely a companion ADR on the CD pipeline + safety layer.
- Whether the **auto-deploy watcher** is a separate adapter (like the GitHub
  issue watcher) or integrated into the runtime.
- **Build order:** `fq drain` + a manual deploy script first, to exercise the
  graceful handoff under controlled conditions; containerisation and the
  auto-deploy watcher layered on once the roll is proven.

## Addendum (2026-07-18)

Issue #271 folded the `fq drain` CLI verb into `fq down` (drain mode). The drain mechanism specified by this ADR is unchanged.
