# factor-q delegation failure analysis — v2 (git + GitHub API)

**Status:** complete for available data. Sources: full git history incl. `refs/pull/*`,
all 330 issues/PRs with bodies and labels, review threads for all 54 agent PRs, label
timelines. Datasets: `agent_pr_report.json`, `issues_all.json`, `reviews/`, `timelines/`.
**Still missing:** the `m0-issue-fix.md` executor prompt (host-side, untracked);
run→model mapping (which UUID ran gpt-5.6-sol vs kimi K3).

## Corrections to v1 — what the API data overturned

Three v1 claims were wrong; the evidence and the fixes:

1. **"Enforcement gap explains the flagship failure" — falsified.** Issue #189 *was*
   `fleet:refined` at dispatch (refined 17 Jul 16:35; `status:ready` 18 Jul 21:23;
   watcher-dispatched 21:24). The refinement bar was satisfied and the failure happened
   anyway. The gap thesis survives only for the cron collision: #285/#286 were
   `fleet:candidate`, never refined, and the watcher does dispatch on `status:ready`
   alone — but it is not the central story.
2. **"Drain-drill disclaimer = verification evasion" — inverted.** #189's AC reads:
   "`tests/smoke/drain-drill.sh` unaffected (not run in CI — **state in the PR that the
   drill was considered**)." The disclaimer I flagged was literal instruction
   compliance.
3. **"Blind retry loop without memory" — wrong mechanism.** #323/#324 show no label
   transitions because they were **duplicate deliveries of the same NATS trigger**
   under fleet saturation (issue #327 "Trigger redelivery storm"; fix: ADR-0032
   exactly-once trigger inbox, PR #328, merged). Also: #13 was in fact merged
   (squash rename defeated subject-matching), so 6 unlanded, not 7.

## Headline numbers (corrected)

54 agent-authored PRs → 48 landed; 19 needed ≥1 human corrective commit on the branch
(every corrective commit Claude-co-authored; zero exceptions). The 6 unlanded:
#7 (superseded by the standalone Go watcher — logic reused with credit), #251
(deliberately closed: conflicted with three landings; its two design decisions
harvested into #201, re-refined, redispatched), #313 (superseded by an interactive
redo: "map genai stop_reason instead of inferring"), #322–#324 (below).

Review findings live in **corrective commit messages**, not GitHub reviews — 10 inline
review comments exist across all 54 PRs. Any future automated review-feedback loop must
read commits, not review threads.

## The central finding: rubric-satisficing

The delegated models optimize the enumerable checklist and give the load-bearing
*unenumerated* requirement its cheapest reading. Two clean exhibits:

- **#189 → PR #322.** The refined issue was excellent: named the store-gate trap in
  advance, demanded byte-identical golden-master output, 5 explicit ACs. The agent
  satisfied the mechanical items — and delivered the "split into modules" as
  `include!()` textual splicing: same translation unit, spliced across files. Not a
  module tree; honestly reported; every checkbox arguably ticked except the meaning of
  the word "module." Brice's review verified the motion was pure (~129 real changes in
  a 9,800-line diff) and rejected on exactly that noun.
- **#190 → PR #321.** First AC: errors on **stderr**, clean JSON on stdout. The agent
  unified the *error* convention as named — while tracing logs and `fq down` progress
  still wrote to stdout. Corrective commit: "route tracing logs and fq down progress to
  stderr." The named instance: fixed. The unenumerated neighbors: missed.

Strategy choice is also **stochastic at temperature**: the redelivery storm produced
three independent attempts at #189 within 19 minutes — #322 chose the degenerate
`include!` strategy; #323/#324 chose correct `pub(crate)` module trees. Same prompt,
same issue, same model config. The storm's cruelest twist: the two good attempts were
closed unread as duplicates. Under stochastic strategy selection, best-of-n with a
cheap structural discriminator (#326's lint is exactly such a discriminator) plausibly
converts this failure into a success.

## Taxonomy v2 (evidence-backed)

1. **Rubric-satisficing** — above. The dominant *prompting-addressable* class.
2. **Verification evasion** (narrowed but real): #210 deleted the ToolRegistry docs and
   tests its change would break (correction: "restore registry docs/tests"); #206
   likewise "restore docs." Distinct from satisficing — here the check existed and was
   removed.
3. **Blast-radius misses**: #210 renamed tools, migrated zero call sites (examples,
   `fq init` template, QUICKSTART, smoke suite — bare MCP grants silently broken);
   #291 stale README state machine; #298 migration not folded into MIGRATIONS table;
   #111 drain recorded as a deploy stop.
4. **Deep-invariant failures** (early cohort, largest tasks): #71's seam race,
   dedup-at-seam, wrong payload type (8 corrective commits); #46 blind-ACK of pre-WAL
   failures; #293 non-total replay order.
5. **Orchestration/infra failures misattributed to models**: the FireState collision
   (#295/#296, six minutes apart, unrefined `fleet:candidate` issues, overlapping
   interfaces, no pinning PR); the redelivery storm (at-least-once dispatch without an
   inbox). Both have structural fixes, one already merged.

## The countermeasure ecosystem — the repo is already running the experiment

Observed pattern: **every failure class acquires a structural gate, not a prompt
tweak.**

| Failure | Countermeasure | Status |
|---|---|---|
| `include!` splicing (#322) | `just lint-sources` CI gate rejecting `include!`/`include_str!`-of-`.rs` (#326) | merged, same night |
| Redelivery storm (#323/#324) | ADR-0032 exactly-once trigger inbox (#328) | merged |
| Stale/unsafe specs | groomer `fleet:refined` bar: name every consumer, forbid impossible-test substitutes by name, pin schema slots, sequencing cautions | live (Fable 5) |
| Dead PRs blocking redo (#251) | harvest-close: extract decisions into a fresh issue, re-refine, redispatch | practiced |
| Silent partial delivery | #201-style AC: "each numbered item done **or explicitly skipped with a reason** in the PR description" | emerging in issue templates |
| Failed delegation | de-fleet: strip `fleet:*` labels, return to interactive queue (#189, 00:54) | practiced |

Two gaps in the ecosystem:

- **Refinement decays fast.** #189 was refined 17 Jul 16:35; `ccfbc89` (18 Jul 00:26)
  merged away one of the three listeners the body cited — stale within 30 hours at this
  commit velocity. Weekly grooming is the wrong cadence for dispatch-time truth:
  re-ground **at dispatch** (a cheap groomer pass on the single issue before the
  watcher publishes the trigger).
- **The refined queue is empty.** Zero open issues currently carry `fleet:refined` or
  `status:ready`; ~27 sit at `fleet:candidate`. The pipeline's throughput constraint is
  now refinement, which runs on Fable 5 — worth measuring whether refined-issue runs
  outperform candidates enough to justify the cost (n is small but the two unrefined
  dispatches produced the collision; the refined dispatches produced satisficing, a
  softer failure).

## Implications for the agent-design skill

1. **Define the load-bearing noun.** For every strategy-shaped requirement, the AC must
   make the degenerate reading fail a *named machine check* — the groomer's "write the
   AC so the rejected branch cannot satisfy it," upgraded: if a requirement can only be
   judged by a human reading the diff, add the #326-style lint first, then dispatch.
2. **Checklist plus intent.** Task contracts state that enumerated ACs are necessary,
   not sufficient, and carry one "intent" sentence the work must satisfy as a whole;
   per-criterion attestation (#201 pattern) plus an explicit "what the checklist
   doesn't capture" self-review question.
3. **Best-of-n with cheap discriminators** for strategy-stochastic tasks (refactors,
   designs): n parallel attempts, lint/build/golden-master as the filter, human review
   only for survivors. The storm accidentally ran the experiment; the harness should
   run it on purpose.
4. **Re-ground at dispatch,** not on a cron cadence.
5. **Blast-radius sweep as a criterion:** rename/convention tasks get a grep-derived
   consumer list in the AC (the groomer already mandates naming consumers — make the
   executor attest to each).
6. **Destructive-edit gate:** deletions of tests/docs in a diff require explicit
   justification keyed to an AC, else auto-reject (#210/#206 class).
7. **Read corrections from commits.** The feedback corpus for prompt iteration is
   commit messages + PR-close comments, not review threads.