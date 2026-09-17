# Measurement instrument — execution plan

> Opened 2026-09-17. Active. This is the execution track for
> [#340](https://github.com/bricef/factor-q/issues/340) (the M0 plan's
> deferred proxy instrumentation) and for the minimum viable instrument of
> the [capability ladder](../../reviews/2026-07-07-25-factor-q-capability-ladder.md)
> §6 ([#413](https://github.com/bricef/factor-q/issues/413)). It runs as a
> parallel thread to the graph-executor work so that the executor lands into
> an instrument that already exists, rather than being measured after the
> fact.

## 1. Why now

Every ladder factor-q is judged on is currently unmeasured. The Q ladder in
`VISION.md` has M0 met (2026-07-20) and M1 undecidable: the M0 plan judged
the bar on merged PRs alone and deferred every proxy. The capability ladder
places factor-q at L1 certified, L2 "plausible but unmeasured, because
attempts were never counted", and everything above unknown. Nine fleet
rounds between 2026-09-10 and 2026-09-17 produced roughly seventy
agent-authored PRs and a comparable number of typed human interventions
(duplicate PRs closed, review findings implemented by a second agent,
rebases, redesigns) and none of it was recorded in a form a query can
read. The dataset grows while the instrument does not, and the one input
that cannot be reconstructed from history, the intervention log, starts
empty every day it is not kept.

The instrument must cost less than the thing it measures (ladder §6). This
plan is a table, a few recipes, a query and two diagrams; it is not a
service, and it does not touch the daemon.

## 2. What is being built

Five deliverables, each an issue written to the refinement bar, in
dependency order.

| # | Deliverable | Issue | Shape |
|---|---|---|---|
| A | The **attempt ledger**: one SQLite built from issue timelines, PRs, the event log and git | [#838](https://github.com/bricef/factor-q/issues/838) | fleet |
| B | **Objective diagrams and measures**: cumulative flow diagram, throughput, stage cycle times, the ladder's derived measures, cost | [#839](https://github.com/bricef/factor-q/issues/839) | fleet, after A |
| C | The **human-entered primitives**: typed interventions, touch minutes, baseline-equivalent tags; `just touch` / `just tag`; the **rubric** | [#840](https://github.com/bricef/factor-q/issues/840) | maintainer authors the rubric; then fleet |
| D | The **trajectory plot**: autonomy × quality per daemon version, cost as dot size, baseline point, danger cell | [#841](https://github.com/bricef/factor-q/issues/841) | maintainer picks the scalars; then fleet |
| E | The **weekly report**: derived measures, current rung with the gate it failed, `Q_task` / `Q_sys` or their missing inputs | [#842](https://github.com/bricef/factor-q/issues/842) | fleet, after A and C |

The split follows the documents: A and B are the objective half (no
judgement anywhere; can run in CI), C is the subjective half made
decidable by a rubric, D and E are the artefacts the M0 plan and the ladder
respectively asked for.

## 3. The ledger is the design decision

Everything reads from `attempt_ledger.sqlite` and nothing else, so its grain
is what matters. The grain is **timestamped transitions**, not daily
snapshots, because the cumulative flow diagram, cycle times and the
ladder's primitives all fall out of transitions and none of them can be
recovered from snapshots.

Sources and the identifiers that join them, verified on 2026-09-17:

- **Issue timeline** (`gh api repos/{r}/issues/{n}/timeline`): `labeled` /
  `unlabeled` events for `status:*` and `fleet:*` carry the state machine
  the watcher drives (`candidate → refined → ready → in-progress →
  in-review → done`, with `failed` and `needs-decision` as side states);
  `closed`, `reopened`, `cross-referenced`. Admission is the moment
  `status:ready` is applied; the body hash at that moment is recorded so
  post-admission rescoping is visible (ladder §5.1, §5.2).
- **Pull requests**: the watcher stamps every fleet PR with
  `provenance: agent=<id> invocation=<uuid> model=<model>`; that line and
  the `m0/` branch prefix identify agent-authored work and join a PR to its
  attempt. Review events, pushes after the first review, merge and close
  are transitions.
- **Event log**: a `triggered` event is an **attempt** (ladder primitive 1,
  recorded at dispatch, immutable); its payload carries
  `trigger_payload.github.issue`, `trigger_id`, the invocation id and the
  model. `completed` / `failed` carry `task_status` or `error_kind`,
  `total_cost`, `total_llm_calls`, `total_tool_calls`. Two attempts for one
  issue are two rows; that is what exposes retry laundering (§5.4).
- **Git**: merge commits on `main`, and **corrective commits** within 14
  days of a merge that reference the PR or issue, revert it, or fix the same
  files. Accepted (primitive 2) means merged **and** surviving 14 days
  without one; the rule used is recorded per row so it can be tightened
  without re-extraction.
- **The log** (deliverable C): `metrics/interventions.csv` and
  `metrics/accepted.csv`, imported as the `interventions` table and the
  `baseline_equivalent` column.

The extractor is stdlib Python plus `gh` on PATH, idempotent and
incremental, with a file-based fallback for the event log so it runs
wherever `gh` runs. Rendering (B, D) is allowed a plotting dependency in an
environment the tool owns; the numbers are always written beside the
pictures as JSON so a dependency-free renderer can replace it.

## 4. The subjective half, and the rubric

The ladder names exactly two primitives a human must enter, interventions
and touch minutes, and the M0 plan adds one labelled judgement per accepted
change, the baseline-equivalent size. Everything else is observable. The
rubric's job is to make those three entries take five seconds and to hold
the anti-gaming rules the ladder says need a human.

A draft, to be edited by the maintainer and landed as
`docs/guide/measurement-rubric.md` under deliverable C:

- **Intervention types** (ladder §2): `unblock` (the run could not proceed
  without a human act: a rebase, re-running a flaky job, a dependency the
  agent could not install); `clarify` (answering a question or adding
  missing context after dispatch); `respec` (changing the acceptance
  criteria after dispatch; §5.2 says this ends the attempt); `fix` (a human
  or a non-fleet agent changes the code the fleet produced: implementing
  review findings, closing a duplicate PR, fixing a merge break);
  `restart` (re-dispatch after a failed or abandoned run); `retune`
  (changing the agent definition, prompt or config because of this
  attempt). Worked examples from the week this plan opened: closing the
  redelivery duplicates #790 and #794 was `fix`; the second-agent fixers on
  #795, #792, #813 and #817 were `fix`; the #807 rebuild on a new design
  was `respec` then `fix`; rebasing #807 after the #818 hotfix was
  `unblock`; writing a refinement stamp is **touch time before the
  attempt**, not an intervention.
- **Touch minutes**: rounded up to the next five, context switch included
  (§5.7, "biased pessimistic by design"); spec-writing, review, correction
  and debugging the run all count; so does reading a green PR that merges
  untouched, because that is the review cost of autonomy.
- **Baseline-equivalent**: S (a skilled human driving a frontier model
  interactively, under an hour), M (up to half a day), L (up to a day), XL
  (more); tagged at merge by the maintainer. The reference point is the M0
  plan's expert+frontier pair, which sits mid-scale on quality and at zero
  on autonomy.
- **Who logs**: the maintainer. A coordinating agent may log on the
  maintainer's behalf for interventions it performed, with `source=agent`,
  so those rows can be excluded or audited.
- **Rules a human holds** (§5): admission precedes attempt; no
  post-admission rescoping without a `respec` row; a correction inside 14
  days counts against the attempt; the holdout set (§5.5) is frozen once
  chosen.

## 5. The trajectory plot, specified before it has data

The M0 plan deferred the plot "until the proxies produce real data; a plot
drawn before then is a drawn intention", and asked for one decision to be
taken deliberately: the y-axis needs a scalar. Deliverable D fixes the
specification now so the first points can be drawn from the ledger without
a second design pass:

- one point per tagged daemon version the dogfood deployed;
- x, autonomy: `1 − interventions / attempts` over the version's window
  (recommended over `first_pass_rate` because it degrades gracefully while
  the log is sparse);
- y, quality: acceptance net of rework, a single proxy rather than a
  composite, with the formula printed on the plot;
- dot size: LLM cost per accepted change;
- the baseline point from the human-driven stream tagged under C, hollow
  until ten tagged changes exist;
- the bottom-right danger cell shaded and labelled.

Throughput is its own line over time, off the plot, as the M0 plan insisted.

## 6. Order of work and what gates what

1. **Start the log by hand today** (C's CSV, before any tooling): it is the
   only input that cannot be backfilled. The rubric draft above is enough
   to begin; the maintainer edits it as entries force decisions.
2. **A**, then **B**: the ledger and the objective diagrams. Fleet work; no
   decision outstanding once the schema in #838 is accepted.
3. **C's tooling** once the rubric text is agreed; **D** once the scalars
   are confirmed; **E** after A and C. All fleet-shaped at that point.
4. A first weekly report over the September fleet rounds is the acceptance
   test of the whole plan: if the maintainer reads it and it changes a
   decision, the instrument has earned the right to become software
   (ladder §6).

Blocked-by edges are set on GitHub: B, D and E on A; D and E on C.

## 7. Done when

- `just metrics-extract && just metrics-report` runs against this
  repository and produces the cumulative flow diagram, the throughput and
  cycle-time series, and the ladder's derived measures for the trailing 90
  days, with the numbers in JSON beside the pictures.
- `metrics/interventions.csv` has entries for every fleet round from the
  day the log starts, and `just touch` / `just tag` are how they get there.
- The trajectory plot renders with at least three version points and the
  hollow baseline.
- The weekly report prints the current rung per domain with the gate it
  failed, and either `Q_task` or the named input it is waiting on.
- `STATUS.md` "What's next" points at the report instead of saying M1 is
  undecidable, and the plan moves to `closed/` with the first report
  attached.

## 8. Explicitly not in this plan

The graph executor (measured by this instrument, not built by it); the
dashboard rendering these diagrams (a later consumer of the same JSON);
automating the L3 paired-sample calibration (a quarterly human procedure by
design); any change to the daemon's events beyond what the extractor reads.
