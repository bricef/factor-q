# Measurement rubric

The rules for the three human-entered inputs of the measurement instrument:
typed **interventions**, **touch minutes** and the **baseline-equivalent**
size of an accepted change. Everything else the instrument reads is
extracted from the event log, GitHub and git
([#838](https://github.com/bricef/factor-q/issues/838)); these three cannot
be reconstructed after the fact, so they are entered as they happen, by the
rules below. The mechanics of entering them are in
[measurement-logs.md](measurement-logs.md); the reasoning is in the
[capability ladder](../reviews/2026-07-07-25-factor-q-capability-ladder.md)
(§2 and §5) and the
[measurement instrument plan](../plans/active/2026-09-17-measurement-instrument.md)
(§4). Decided 2026-09-20 under
[#840](https://github.com/bricef/factor-q/issues/840).

The rubric's job is that an entry takes five seconds and means the same
thing in March as in September. Every rule here is one sentence long on
purpose; the examples carry the nuance.

## The three entries

| Entry | Command | Granularity | When |
|---|---|---|---|
| Intervention | `just touch <issue> <minutes> <type> [note]` | one row per human act inside the loop | as it happens, or at the end of the session |
| Touch minutes | the `<minutes>` of that row | per act, rounded **up** to the next five | measured, not remembered (below) |
| Baseline-equivalent | `just tag <pr> <S\|M\|L\|XL> [note]` | **one row per merged pull request**, never per intervention | at merge, by the maintainer |

An intervention is any human act inside the loop other than the terminal
accept or reject of a pull request. Merging a green PR untouched is not an
intervention, but the minutes spent reading it are touch minutes and get a
row of their own with type `fix` and a note saying "review only" — that is
the review cost of autonomy and it counts. Writing a refinement stamp
before dispatch is touch time **before the attempt** and is not logged as an
intervention: it is admission work, and admission precedes the attempt.

## Intervention types

The set is closed at any moment and open over time. The log script refuses
a type it does not know, so adding one is a line in `scripts/metrics-log.py`
and a dated row in the table below, in one pull request. Never rename or
merge a type: the history was logged against the old meaning.

| Type | One sentence | Since |
|---|---|---|
| `unblock` | The run could not proceed without a human act: a rebase, a re-run of a flaky job, a dependency the agent could not install. | 2026-09-17 |
| `clarify` | Answering a question or adding missing context to the issue after dispatch. | 2026-09-17 |
| `respec` | Changing the task's acceptance criteria after dispatch; this ends the attempt (ladder §5.2). | 2026-09-17 |
| `fix` | A human or a non-fleet agent changes the code the fleet produced: implementing review findings, closing a duplicate PR, fixing a merge break, or reviewing the PR. | 2026-09-17 |
| `restart` | Re-dispatching after a failed or abandoned run. | 2026-09-17 |
| `retune` | Changing the agent definition, prompt or config because of this attempt. | 2026-09-17 |

When two types apply to one act, log two rows and split the minutes; the
rebuild of #807 below is the example.

## Touch minutes: measured, not remembered

Self-reported minutes drift optimistic without exception (ladder §5.7), and
a sense of time is not a measurement. So minutes come from three sources,
in this order of preference:

1. **Session hooks.** Claude Code fires shell hooks on prompt submit, stop
   and session end; a hook that appends a timestamp, the session id and the
   working directory to a local log captures every interaction burst at zero
   model cost. A burst is the span from its first prompt to its last stop,
   and a gap over **ten minutes** ends a burst. The coordinating agent marks
   which issue a burst belongs to when the work starts, and at session end
   turns the log into draft rows for the maintainer to confirm. The tooling
   is [#871](https://github.com/bricef/factor-q/issues/871).
2. **Manual entry for acts outside a session**: reading a PR in the
   browser, a rebase in a terminal, re-running a job. Enter them after the
   fact, the same day.
3. **Sampling is calibration, not a source.** A random "what are you working
   on?" ping estimates time allocation without bias, but a fifteen-minute
   intervention sampled every thirty minutes is seen zero or one times, which
   is useless per attempt. Run a fortnight of sampling to calibrate the hook
   estimates against perceived time, then stop.

Whatever the source, the rule at entry is the same: **round up to the next
five minutes, and count the context switch** — the ten minutes after an
interruption when you were not really back yet are the attempt's minutes.

## Baseline-equivalent: one size per accepted change

The tag answers one question about the whole merged pull request: *how long
would a skilled human, driving a frontier model interactively, have taken to
produce this change?* That pair is the M0 plan's reference point
([2026-07-05](../plans/closed/2026-07-05-m0-close-the-loop.md)); it sits
mid-scale on quality and at zero on autonomy, which is what makes fleet
throughput comparable to human-driven work.

| Size | The human-driven session would have taken |
|---|---|
| `S` | under an hour |
| `M` | an hour to half a day |
| `L` | half a day to a day |
| `XL` | more than a day |

Tag at merge, by the maintainer, judged on the change as merged (not as the
issue was written). Interventions are never sized; they have minutes.

## Who logs

- **The maintainer** logs, and only the maintainer tags sizes.
- **A coordinating agent** (a session that dispatches, reviews or fixes fleet
  work) logs the interventions it performed itself, with `--source agent`,
  and at session end drafts the touch rows from the hook log for the
  maintainer to confirm. Agent rows can be excluded or audited.
- **The attempting agent never logs.** No row about an attempt comes from the
  fleet agent that made it, and no row is derived from its own transcript.

## Rules a human holds

The ladder's anti-gaming rules (§5), each with who holds it and how it is
checked. A rule the ledger can check is checked by the ledger; the rest are
held by hand, and the point of writing them down is that "by hand" is a
promise, not a habit.

| # | Rule | Held by | Checked by |
|---|---|---|---|
| 1 | **Admission precedes attempt.** A task is `status:ready` before any agent sees it; an attempt dispatched before admission is excluded from the pool. | the dispatcher (human or coordinating agent) | the ledger: `attempts.dispatched_at` after `tasks.admitted_at`, or the attempt is flagged |
| 2 | **No rescoping after admission without a `respec` row.** A groomer or human edit to an admitted issue's acceptance criteria ends the attempt. | the maintainer and the backlog groomer's operator | by hand: GitHub's timeline does not keep the body as it was at admission, so the ledger cannot hash it (`admission_body_hash` is NULL) |
| 3 | **A correction within fourteen days counts against the attempt**, and its minutes land as a `fix` row. | nobody: it is mechanical | the extractor's acceptance rule; which commits qualify is [#868](https://github.com/bricef/factor-q/issues/868) |
| 4 | **The holdout set is frozen once chosen.** Rung certification runs on it only; it is never used for prompt tuning, agent-definition changes or harness debugging. | the maintainer | a committed list file, changed only by a dated pull request; the holdout is not yet drawn (plan §8 defers L3 and above) |
| 5 | **No self-tagging by the fleet.** Sizes and intervention rows come from the maintainer or from a coordinating agent acting on the maintainer's behalf, never from the agent that made the attempt. | every session that logs | the `source` column, and the audit in the next rule |

And the rubric audits itself: each quarter, re-review a random ten per cent
of the quarter's accepted changes against this file as it stood at the
start of the quarter (ladder §5.6). If the re-review disagrees with the
original verdict on more than fifteen per cent of them, the standard has
drifted and the trend line is measuring patience, not the system; say so in
the weekly report and re-baseline.

## Worked examples

From the week the plan opened (2026-09-14 to 2026-09-18):

| What happened | Rows |
|---|---|
| Closing the redelivery duplicates [#790](https://github.com/bricef/factor-q/pull/790) and [#794](https://github.com/bricef/factor-q/pull/794) | `fix`, one row each, five minutes each |
| An Opus agent implementing review findings on [#813](https://github.com/bricef/factor-q/pull/813), six commits, re-gated, coordinated by a session | `fix`, one row, the coordinating session's minutes, `--source agent` |
| The [#807](https://github.com/bricef/factor-q/pull/807) rebuild on a new design | `respec` (the criteria changed) **and** `fix` (the code changed), minutes split |
| Rebasing #807 after the [#818](https://github.com/bricef/factor-q/pull/818) hotfix | `unblock` |
| Reading [#861](https://github.com/bricef/factor-q/pull/861) and merging it untouched | `fix`, note "review only", the reading minutes |
| Writing the refinement stamp on [#839](https://github.com/bricef/factor-q/issues/839) before dispatch | nothing: admission work, before the attempt |
| Dispatching [#857](https://github.com/bricef/factor-q/issues/857) by applying `status:ready` | nothing: that is the admission itself |
| Merging #861 | `just tag 861 M` (the report command, its tests and README: a half-day human session) |

## Changing this rubric

Any change is a dated pull request that edits this file and, for a type,
`scripts/metrics-log.py` in the same change. A change never re-labels old
rows. The weekly report ([#842](https://github.com/bricef/factor-q/issues/842))
prints the rubric's last-changed date beside the measures so a step in a
trend can be read against it.
