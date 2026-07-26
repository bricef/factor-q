# The factor-q capability ladder

*A verifiable instrument beneath the Q200 north star. Drafted 2026-07-25.*

Q200 stays. It is the judgement call about where this is going, and judgement calls are allowed to be unmeasurable — that is what makes them judgement. What this document adds is a ladder underneath it: a set of rungs that can be certified from data the system already emits, so that the judgement becomes **accountable to something** rather than free-floating.

The design goal is narrow: **every gate below must be decidable by a query, not by an opinion.** If certifying a rung requires anyone to estimate how long a human would have taken, the gate is wrong and needs rewriting.

---

## 1. What the ladder has to fix

Three defects in the current Q ladder, which the design below addresses directly:

1. **The denominator is ambiguous.** Does building factor-q count as "human effort invested in factor-q"? Excluded, Q flatters; included, Q is deeply negative for years.
2. **The numerator needs a counterfactual.** "The equivalent of 200 days of work" requires estimating what a human would have taken — an estimate that is unfalsifiable and drifts optimistic.
3. **The evidence has no denominator.** "20+ accepted PRs" counts successes. Attempts, abandonments, retries and corrective commits are the other half of every ratio.

### Fusion already solved (1)

This is worth dwelling on, because the metaphor is load-bearing and it is more precise than it has been asked to be.

Fusion does not have one Q. It has at least two, and the difference between them is exactly the ambiguity above:

| Fusion | Meaning | factor-q analogue |
|---|---|---|
| **Q_plasma** (scientific gain) | Fusion power out vs heating power into the plasma. Excludes the plant. | **Q_task** — human time saved on the task vs human time spent on that task. Excludes building and operating factor-q. |
| **Q_eng** (engineering gain) | Net electricity out vs *total* electricity in, including magnets, cryo, pumps. | **Q_sys** — human time saved vs *all* human time on factor-q: development, operation, debugging, prompt tuning, review. |
| **Ignition** | The reaction sustains itself; external heating can be withdrawn. | **Ignition** — the improvement loop sustains without human-authored improvements. |

NIF hit Q_plasma > 1 in 2022 and is still nowhere near Q_eng > 1. That gap is not an embarrassment; it is the honest structure of the problem, and stating it openly is what makes the field's claims credible.

Adopt the same discipline and the denominator question answers itself: **you report both numbers, always, and never let Q_task be quoted alone.** Q_task is the encouraging one and it is legitimate. Q_sys is the one that decides whether this was worth doing.

---

## 2. Five primitives

Everything below is derived from five recorded quantities. Nothing else is needed.

| # | Primitive | Definition | Where it comes from |
|---|---|---|---|
| 1 | **Attempt** | An invocation dispatched against an *admitted* external task. Recorded at dispatch, immutable. | Event log — already emitted |
| 2 | **Outcome** | `accepted` / `rejected` / `abandoned` / `errored`. Accepted = merged **and** surviving 14 days without a corrective commit. | GitHub + git |
| 3 | **Intervention** | Any human act inside the loop other than the terminal accept/reject. Typed: `unblock`, `clarify`, `respec`, `fix`, `restart`, `retune`. | **New — must be logged** |
| 4 | **Touch minutes** | Human minutes attributable to an attempt: spec-writing, review, correction, debugging its run. Rounded **up**, including context-switch cost. | **New — self-reported** |
| 5 | **Unattended span** | Wall-clock between two consecutive interventions, fleet-wide. | Derived from 3 |

### Derived measures

- `first_pass_rate` = accepted attempts with zero interventions ÷ all attempts
- `attempts_per_accept` = attempts ÷ accepted tasks *(exposes retry laundering)*
- `touch_per_accept` = median touch minutes per accepted unit
- `MTBI` = mean time between interventions, fleet-wide
- `correction_ratio` = human-authored LOC in corrective commits ÷ agent-authored LOC

The critical property: **1, 2, 3 and 5 are observable without judgement.** Only 4 involves estimation, and it is an estimate of your own past time, not of a counterfactual — a far weaker and more reliable claim.

---

## 3. The spine

Eight rungs. The unit of delegation grows; the human touch per unit shrinks. One rung is "current" at a time, per domain (§4).

---

### L0 — Supervised task

*The agent completes a tool-mediated task correctly while a human watches.*

**Gate:** trivially met. Present only to anchor the scale.

---

### L1 — Unattended completion

*An agent completes an externally-specified task with no human contact between dispatch and terminal state, and the output is accepted.*

**Gate:** ≥ 1 attempt with `intervention_count == 0` and `outcome == accepted`.

**Status:** met, and well-evidenced. This is roughly what M0 claimed.

---

### L2 — Repeatable completion

*L1 is not a fluke, and the failures are counted.*

**Gate**, over a rolling 30 days, on admitted tasks only:
- ≥ 30 attempts
- `first_pass_rate` ≥ 0.50
- `attempts_per_accept` ≤ 2.0
- every attempt has a recorded outcome — no silent drops

**Status: unknown.** This is the honest re-certification point. The M0 evidence establishes L1 but says nothing about L2, because it reports 20+ accepted PRs with no denominator. Adopting this ladder means the first act is measuring whether you are on the rung you think you're on. That may be uncomfortable and it is the whole point — a ladder you cannot fall down is not an instrument.

---

### L3 — Task breakeven · Q_task ≥ 1

*Delegating a task costs less human time than doing it.*

This is the rung "M1: Net zero" was reaching for. It needs a real instrument, and the instrument is a **paired timed sample**, not a continuous estimate.

**The calibration.** Every quarter, draw ≥ 10 admitted tasks. Do them yourself, timed, before any agent sees them. Compare against `touch_per_accept` for a matched sample the fleet handled. Both numbers are measured wall-clock, neither is a counterfactual guess.

**Gate:**
- `median(touch_per_accept)` < `median(hand_minutes)` on the paired sample
- LLM spend per accepted unit < your chosen hourly rate × the time saved
- calibration refreshed within the last 90 days

**Why paired sampling.** It is the only honest way to get a counterfactual: you *become* the counterfactual, occasionally and expensively, rather than estimating it constantly and cheaply. Ten tasks a quarter is a real cost and it is the price of the number meaning anything.

---

### L4 — Queue autonomy

*The system clears heterogeneous work without per-task human setup.*

**Gate**, over a 30-day window under continuous load:
- `MTBI` ≥ 72 hours
- ≥ 20 accepted units in the window
- ≥ 3 distinct task types (e.g. feature / bugfix / test / docs)
- no single intervention type accounts for > 50% of interventions *(a recurring unblock is a missing feature, not autonomy)*

**Note.** This rung is where orchestration likely stops being optional. Sustaining 72 hours across heterogeneous work without a human sequencing it is difficult to fake with a single-agent loop — which is a feature of the gate, not a side effect.

---

### L5 — Objective decomposition

*The system takes a goal, not a task list.*

**Gate:** ≥ 5 objectives where
- the system produced the decomposition into tasks,
- ≥ 80% of resulting tasks were accepted,
- the human did not rewrite the decomposition (a rewrite is a `respec` intervention).

**Why this precedes L6.** Spec-writing is almost certainly your dominant touch cost. You cannot reach system breakeven while a human is still authoring every issue body — the denominator won't move. This rung is what removes that term.

---

### L6 — System breakeven · Q_sys ≥ 1

*The whole enterprise pays for itself, including the cost of building it.*

**Gate**, over a rolling 90 days:

```
total_saved   = accepted_units × median(hand_minutes)      [from the L3 calibration]
total_spent   = touch_minutes on all attempts
              + development hours on factor-q itself
              + operational hours (deploys, incidents, harness debugging)
              + prompt/config tuning hours

Q_sys = total_saved / total_spent  ≥  1.0
```

This is the brutal one, and it is brutal on purpose. It counts the hours you spend building factor-q against the hours factor-q saves you. It will be well under 1 for a long time, exactly as Q_eng is for every fusion device ever built, and that is not a reason to hide it — it is the number that tells you whether the project is converging or receding.

**Report Q_task and Q_sys together, always.** The gap between them is the overhead you are carrying, and watching that gap close is more informative than either number alone.

---

### L7 — Ignition

*The system improves its own effectiveness without human-authored improvements.*

**Gate:** a statistically meaningful improvement in `first_pass_rate` or `touch_per_accept` across two consecutive 30-day windows, where:
- the changes responsible were agent-authored,
- they were accepted without human rewrite (accept/reject only),
- **the improvement shows on the frozen holdout set** (§5.5), not the tasks used for tuning.

That last clause is the whole gate. Without it, "the system improved itself" is indistinguishable from "the system overfit to its own benchmark," and a self-improving system is precisely the kind that will find that gap.

---

## 4. The domain axis

Rungs are certified **per domain**, not globally. The headline is a vector, not a scalar:

> **L4 (code) · L1 (documents) · L0 (operations)**

Three domains, matching VISION.md's stated use cases:

| Domain | Oracle | Current evidence |
|---|---|---|
| **Code** | `just ci` — fast, deterministic, machine-checkable | Everything |
| **Documents** | Human scoring against a fixed rubric | None |
| **Operations** | Production, or a staging mirror | None |

This exists because the harness has been tuned for months against the one workload with a cheap oracle, and that tuning is invisible — it lives in prompt shapes, error-message design, tool ergonomics, and the `${workspace}` model, all of which quietly assume "there is a repo and a test command."

**The transfer cost is itself the measurement.** Run the ladder on documents and see where it lands. If code is L4 and documents collapse to L1, that gap tells you precisely how much of the harness is `just ci` in disguise — which is information you cannot get any other way, and which you need before claiming a general-purpose runtime.

Ordering rule: **you may not claim a rung as the project's headline unless it holds in at least two domains.** Single-domain rungs are reported as `L4 (code only)`.

---

## 5. Anti-gaming

A self-improving system optimises whatever it is scored on. These are not paranoia; they are load-bearing.

### 5.1 Admission precedes attempt

A task enters the measured pool when labelled, **before any agent sees it**, with an admission timestamp. Tasks admitted after an attempt began are excluded. This kills cherry-picking.

### 5.2 No post-admission rescoping — *specific to your system*

`backlog-groomer` rewrites issue bodies, rescopes PARTIAL issues, and applies `fleet:refined`. Its own prompt states the premise: *"the tracker is the fleet's spec source: fleet agents implement what issue bodies say."*

**So the fleet can lower its own bar.** An agent that rescopes an admitted task until it is achievable, and is then scored on achieving it, is grading its own exam. This is not hypothetical — it is a designed capability of a currently-running production agent.

**Rule:** any groomer edit to an admitted task's body either (a) removes it from the measured pool, or (b) counts as a `respec` intervention against the attempt. Enforce by comparing the issue body hash at admission against the hash at dispatch.

### 5.3 Attempts count at dispatch

Recorded in the immutable event log the moment the trigger is consumed. A run that crashes, times out, or is killed still counts in the denominator. There is no path by which a failure disappears.

### 5.4 Retry laundering stays visible

`attempts_per_accept` is reported next to `first_pass_rate` in every report. Five attempts and one success is not one success.

### 5.5 Frozen holdout

20% of admitted tasks are reserved at admission by hash, never used for prompt tuning, agent-definition changes, or harness debugging. **Rung certification runs on the holdout only.** L7 is meaningless without this, and every other rung is weaker without it.

### 5.6 Fixed acceptance rubric, audited for drift

Write the acceptance rubric down before measuring. Each quarter, re-review a random 10% of past acceptances against the original rubric. If the re-review disagrees with the original verdict more than ~15% of the time, your standards have drifted and the trend line is measuring your patience, not the system.

### 5.7 Touch time is biased pessimistic by design

Round up. Count the context switch. Count the ten minutes after an interruption where you were not really back yet. Self-reported effort drifts optimistic without exception, so bias the estimator against yourself and let the number be defensible.

### 5.8 Corrections count against the attempt for 14 days

An accepted PR that gets a corrective commit within 14 days reverts to `rejected`, and the corrective time lands in `touch_minutes`. This prevents "merge it, quietly fix it Tuesday" — and you already have the signal, since corrective commits are attributed separately in your history.

---

## 6. Building the instrument

**The instrument must cost less than the thing it measures.** Building an elaborate measurement subsystem would be another substrate trap, and this ladder is not worth a new service.

### The minimum viable version

Four things, in order:

1. **A task pool with admission timestamps.** You have this. GitHub labels plus the label-application timestamp from the issue timeline API. Add the body hash at admission.
2. **An intervention log.** A single append-only table or CSV: `(timestamp, task_id, type, minutes, note)`. Log it by hand at first. If you will not log an intervention by hand, you will not log it through a tool either.
3. **Touch minutes.** Same table. A `just touch <task-id> <minutes> <type>` recipe is a twenty-line addition and takes five seconds to run.
4. **A weekly report.** One query joining the event log, the intervention log, and the GitHub PR outcomes. Print the six derived measures, the current rung per domain, and Q_task / Q_sys.

Roughly: a table, a `just` recipe, and a query. **Start with a spreadsheet if that ships this week.** The instrument earns the right to become software by first proving anyone uses it.

### What you already have

Most of the substrate exists. The event log carries attempts, cost, duration, error kinds and tool calls. The projection denormalises it. GitHub carries outcomes. Git carries corrective commits. The two genuinely new things are **the intervention log** and **touch minutes** — and both are human-entered, which is why they are the only parts that need discipline rather than engineering.

### Sketch of the weekly query

```sql
-- first-pass rate and retry laundering, 30-day window, holdout only
SELECT
  COUNT(*)                                             AS attempts,
  COUNT(DISTINCT CASE WHEN accepted THEN task_id END)  AS accepted_tasks,
  ROUND(1.0 * SUM(accepted AND interventions = 0)
            / COUNT(*), 3)                             AS first_pass_rate,
  ROUND(1.0 * COUNT(*)
            / NULLIF(COUNT(DISTINCT CASE WHEN accepted THEN task_id END),0), 2)
                                                       AS attempts_per_accept
FROM attempt_ledger
WHERE dispatched_at >= datetime('now','-30 days')
  AND holdout = 1;
```

---

## 7. Where factor-q actually sits

On the current evidence, honestly stated:

| | Code | Documents | Operations |
|---|---|---|---|
| **Certified** | **L1** | L0 | — |
| **Plausible but unmeasured** | L2 | — | — |
| **Blocked on** | denominator | any attempt at all | any attempt at all |

L1 is solid. L2 is probably true and cannot currently be shown, because attempts were never counted. Everything above L2 is unmeasured. Q_task and Q_sys are both currently unknown, and Q_sys is near-certainly well below 1 — as it should be for a project four months old.

**None of that is a bad result.** It is the normal state of an instrument that has just been switched on, and it is considerably better than a confident number nobody can check.

---

## 8. First three moves

1. **Start counting attempts this week.** Nothing else on this list matters until the denominator exists. Every future rung is retroactively unprovable for any period you did not count.
2. **Add the body-hash check on admitted tasks** (§5.2). One comparison, and it closes the one gaming vector your system can already exercise autonomously.
3. **Run the first paired calibration** (§3, L3). Ten tasks, timed by hand. It is a genuinely annoying afternoon and it converts "M1: Q1" from an aspiration into a number you can be wrong about.

Then leave it alone for a quarter and read the trend rather than the level. Levels are noisy and invite argument; trends are what a ladder is for.

---

## Appendix — rung summary

| Rung | Claim | Gate |
|---|---|---|
| **L0** | Supervised task | anchor |
| **L1** | Unattended completion | ≥1 accepted attempt, zero interventions |
| **L2** | Repeatable completion | 30d: ≥30 attempts, first-pass ≥0.50, attempts/accept ≤2.0 |
| **L3** | Task breakeven · **Q_task ≥ 1** | paired timed sample: touch < hand, calibration <90d old |
| **L4** | Queue autonomy | 30d: MTBI ≥72h, ≥20 accepted, ≥3 task types |
| **L5** | Objective decomposition | ≥5 objectives, system-decomposed, ≥80% accepted, no respec |
| **L6** | System breakeven · **Q_sys ≥ 1** | 90d: saved ÷ (touch + dev + ops + tuning) ≥ 1.0 |
| **L7** | Ignition | agent-authored improvement, holdout-verified, 2 windows |

All rungs certified per domain. Headline rung requires two domains. All rungs certified on the frozen holdout.
