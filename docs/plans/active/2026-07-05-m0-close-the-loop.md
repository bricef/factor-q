# M0 — Close the loop: implementation plan

**Status:** proposed (2026-07-05). Drives the **M0** milestone named in
[VISION.md](../../../VISION.md) ("factor-q can be used to work on factor-q
itself") — which the
[2026-07-05 project assessment](../../design/2026-07-05-project-assessment.md)
§1 flagged as having no active plan. This is that plan. It is deliberately
*loop-first* rather than platform-first: it turns one of the
[reference workloads](../../design/committed/reference-workloads.md) from a
touchstone into a running load, and lets the load's gaps reorder priorities
— the same way the v0 dogfood loop already reordered them (caching, SIGPIPE,
broker separation).

## What M0 is — and isn't

M0 is a **capability** bar, not a leverage ratio: its Q entry in VISION is
literally "—". The bar (from VISION): the system is capable of *complex code
comprehension, multi-file changes, test validation, and git workflows*.

Concretely: **factor-q's own loop — running autonomously, not an interactive
session — lands accepted, test-validated, non-trivial changes to this
repository.** That is the whole of M0. Measuring Q is explicitly *not* part
of it (see "Not M0"); the proxy instrumentation below rides alongside as
cheap scaffolding for the milestones that follow, but M0 succeeds or fails
on the capability alone.

## What already exists

- **The runtime**, hardened over the 2026-07-05 verification arc: a
  reducer/WAL/budget/recovery path now covered by an oracle, a crash DST,
  and budget properties (see the
  [reducer verification plan](../closed/2026-07-05-reducer-verification.md)). Crash
  recovery is WAL-based and independent of the event bus.
- **The building blocks the loop needs**: MCP, shell/file tools, a
  nothing-by-default sandbox, per-invocation budgets, triggers, and cron.
- **The v0 dogfood loop** (`doc-drift`, live daily against this repo from
  `~/fq-dogfood`). It has `file_write` and `exec` in its sandbox but is
  scoped to **read-and-report** — it comprehends commits and writes a dated
  report. It is necessary substrate and a proof the daemon runs real
  workloads, but it does **not** clear M0's bar: it makes no change,
  validates no tests, opens no PR.

So the gap to M0 is **loop shape, not raw tool capability**: going from "an
agent that reviews and reports" to "an agent that lands a validated change."

## The loop (the primary deliverable)

This loop is tractable *because* of [design principle 6](../../design/committed/design-principles.md)
— the codebase's core capabilities sit behind verified, swappable seams, so
an agent can work on a module with a bounded blast radius and a contract the
verification net keeps it from silently breaking. The discipline that lets a
human swap a component is the same one that lets the loop.

A scoped-task change loop:

1. **Intake** — a scoped task (a backlog item, a failing test, a
   doc-fix-requiring-code-understanding). Low blast radius to start.
2. **Change** — the agent comprehends the relevant code and makes the
   multi-file change in a **sandboxed working copy**.
3. **Validate** — the agent runs `just ci` (the same gate every change in
   this repo passes) and iterates until green.
4. **Land** — the change is opened as a **pull request**, never pushed
   direct to `main`.
5. **Gate** — a human accepts (merges) or rejects (closes). This is both the
   safety boundary and, conveniently, the acceptance signal the proxies
   read.

### Safety (non-negotiable)

An agent editing factor-q's *own source* is exactly where a mistake is
costly, so the loop is constrained from the start:

- **PR-only**, never direct push to `main`; a human merge gate on every
  change.
- **Per-invocation budget** (the loop cannot spend unboundedly).
- **Sandboxed edits** against a working copy, not the live tree.
- **Low-ambition task selection** first; ambition grows only as demonstrated
  trust grows — and that growth is visible on the proxies, not asserted.

## Instrumentation (rides on the loop)

The measurement is secondary to the capability, but it is nearly free
*because* the loop is PR-shaped, and it is what makes the post-M0 milestones
decidable (see Baseline). Five proxies, **read as pairs and trends, never as
targets, never composited into a single score**:

| Proxy | Crude stand-in for | Captured from | Its lie |
|---|---|---|---|
| **Acceptance rate** | value / "was it useful" | PR merged vs closed-unmerged | accept ≠ good; you may accept-then-fix — pair with correction ratio |
| **Rework rate** (lagging) | *durable* correctness | reverts / churn on merged lines within N days | low volume, slow — but catches "looked good, was wrong" |
| **Autonomy rate** | self-sufficiency / trust | loop events: reached a landable candidate with no human intervention | autonomous ≠ correct — only meaningful against acceptance |
| **Human-correction ratio** | human-effort-in (the denominator) | diff attribution: human-authored fraction of the merged change | a rewrite and a one-liner both count as "touched" unless measured as a ratio |
| **Cost per accepted change** | compute-input efficiency | `fq costs` spend ÷ accepted (trend only) | changes aren't fungible — comparable across similar task types, not absolute |

The signal is in the **pairings**, because every single metric games toward
trivial-safe work:

- **Autonomy × Acceptance** — the trust quadrant. High/high = real leverage;
  **high-autonomy + low-acceptance = confidently wrong** (the danger cell);
  low-autonomy + high-acceptance = works but needs hand-holding, i.e. not
  yet leverage.
- **Acceptance × Rework** — looked-good vs was-good.

Honest caveats, to be held in view:

- **N is tiny.** One loop, a handful of changes — these are qualitative
  trend-watching, not statistics. Do not composite them into a "Q-ish"
  number; that smuggles false precision back in.
- **Shared failure mode: drift toward trivial work.** Every proxy looks
  better if the agent only attempts safe, small changes. The guard is that
  they are *watched, not optimised*, plus a qualitative eye on whether task
  *ambition* is holding.
- **Denominator blind spot.** These capture *downstream* human effort
  (fixing, reviewing — git-visible), not *upstream* scoping/prompting time.
  The maintainer's felt sense covers that; the proxies do not pretend to.
- **The human+system confound is in-scope, not a bug.** You cannot separate
  "the agent improved" from "the maintainer got better at driving it" — but
  Q is a property of the human+system *pair*, so measuring the pair is
  correct.

## Baseline & calibration methodology

The proxies mean nothing in the absolute; they are read **relative to an
expert+frontier reference** — a skilled human driving a frontier model
interactively (Claude Code sessions like the one that produced this plan).
This is what makes the *post-M0* milestones decidable at all: "are we at
Q1?" is unanswerable, but "how close is the autonomous loop's proxy profile
to the human-driven baseline's?" is a comparison you can make. Relative
calibration replaces absolute Q measurement.

- **The reference is a mid-axis point, not a ceiling.** A well-built system
  can *exceed* expert+frontier quality — the 2026-07-05 DST caught seven
  bugs the interactive pair had written and missed, because systematic
  machinery explores adversarially at a scale a human-in-the-loop won't
  sustain. So the reference sits mid-scale on the quality dimension, with
  headroom above (great systems) and below (most early attempts).
- **Two axes, not one number.** Quality/cost (which the baseline pins
  mid-scale) is *orthogonal* to **autonomy** (where the human-driven
  baseline sits at the bottom, being maximally interactive). The trajectory
  factor-q is chasing is "climb the autonomy axis without falling off — and
  ideally rising above — the baseline's quality," i.e. toward the top-right.
- **Not all proxies transfer to the baseline, and that is informative.** For
  the interactive baseline, autonomy is ≈0 by construction and acceptance is
  ≈1 (rejection happens continuously and invisibly as the human redirects
  mid-task). So those two do not compare cleanly; **correction-ratio,
  cost-per-change, and rework** are the axes that transfer, and they are
  what the baseline pins.
- **Profile distinct from label.** The durable artifact is the proxy
  *profile* of expert+frontier work; "≈Q10" is the maintainer's *felt
  annotation* of what that profile is worth, not a derived figure. This
  session (2026-07-05) is the first qualitative anchor, not a computed
  point.
- **Baseline is an ongoing stream, not a one-off.** Run the same proxy
  pipeline over the human-driven development stream so the reference is a
  distribution, and **tag task difficulty** so baseline-vs-autonomous
  compares like-with-like (the human-driven stream naturally takes harder
  tasks than an early loop is trusted with).

## Throughput (its own track)

Throughput matters — Q's numerator is *work produced* — but it is **not a Q
proxy** and does not belong on the quality×autonomy plot: Q is
work-per-human-effort (a ratio), throughput is work-per-calendar-time (a
rate), and they are orthogonal (a system can be high-Q/low-throughput or the
reverse). Forcing it into the proxy set is why its shape kept eluding us.

The least-gameable shape reuses the calibration reference: for each accepted
change the maintainer tags a rough **baseline-equivalent** ("this would have
taken a human-driven session ~half a day"), and throughput is
**baseline-equivalent-work per week**. Trivial-work drift cannot inflate it
(a trivial change earns a tiny tag); the only way to move it is to displace
real work. The cost is one *labelled judgment* per change (right side of the
profile-vs-label line). Bonus: the same tag, divided by human-effort instead
of calendar time, is the closest thing to a crude **Q-numerator estimate** —
so throughput is the one place a felt-but-grounded Q *number* can honestly
come from. Lighter variant if per-task time estimates feel heavy: T-shirt
sizes (S/M/L effort), one tag, same resistance to drift.

## The trajectory plot (deferred artifact)

The eventual artifact that shows whether we are moving in the right
direction: **autonomy (x) × quality (y)**, the expert+frontier reference at
roughly (low-x, mid-y), each tagged version a point, the trajectory expected
to march *right* without *sinking*; **cost-per-accepted-change as dot size**
(a third channel without a third axis); the **bottom-right corner the
visible danger cell** (high autonomy, low quality = confidently wrong at
scale). Throughput rides as its own line over time, not on this plot.

**Deferred until the proxies produce real data** — a plot drawn before then
is a drawn intention. One decision to make deliberately when we build it:
the y-axis needs a scalar, so "quality" must resolve to either one primary
proxy (e.g. acceptance-net-of-rework) or an *explicitly soft* composite —
chosen on purpose, not sleepwalked into.

## Execution path

Capability first; instrumentation rides on it; baseline and plot follow the
data.

1. **The change loop** — task intake, sandboxed edit, the `just ci`
   validation gate, PR creation (via `gh`). Reuses the runtime, tools, and
   sandbox we already have; the new surface is task-intake shape and
   PR-landing, plus the safety constraints above.
2. **Proxy capture** — PR-shaped landing yields four of five from git;
   autonomy from the loop's own events; wire the baseline-equivalent tag
   into the accept/reject step.
3. **Run it** — on real scoped tasks, accumulate accepted changes, watch the
   proxies relative to the (concurrently accumulating) baseline. Feed the
   gaps to the [backlog](../backlog.md), as the doc-drift loop already does.
4. **The plot** — deferred until step 3 has produced enough points to be
   honest.

## Done signal

M0 is met when the **autonomous loop** (not an interactive session) has
landed a handful of **accepted, `just ci`-validated, non-trivial** changes
to this repository, across **more than one task type** — a capability
demonstration — with the proxies showing the loop is genuinely useful
(acceptance holding, correction-ratio not swamping the value), read against
the baseline. This is explicitly a *capability demonstration at tiny N*, not
a statistical claim; the judgment is the maintainer's, informed by the
proxies rather than reduced to them.

## Not M0 (deferred to M1+ / M3)

- **The Q ratio itself.** M1 ("net zero", Q1) is the first ratio milestone,
  and this plan makes it decidable — via the autonomous proxy profile's
  distance from the baseline, not an absolute measurement.
- **The self-improvement loop and multi-agent orchestration** — M3
  concerns; see
  [shadow-mode-and-self-improvement](../../design/aspirational/shadow-mode-and-self-improvement.md).

## References

- [VISION.md](../../../VISION.md) — the M0 milestone and the Q ladder.
- [2026-07-05 project assessment](../../design/2026-07-05-project-assessment.md)
  §1 — the critique this plan answers.
- [reference-workloads.md](../../design/committed/reference-workloads.md) —
  the touchstones this turns into a running load.
- [reducer verification plan](../closed/2026-07-05-reducer-verification.md) — the
  hardened runtime the loop rides on.
- [ADR-0026](../../adrs/accepted/0026-event-log-system-of-record.md) — the
  event-log system of record the proxies ultimately read from.
