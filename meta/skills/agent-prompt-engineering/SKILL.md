---
name: agent-prompt-engineering
description: How to write, revise, and review fleet agent definitions for factor-q — and how to review runs and transcripts for prompt-caused failure. Use when creating a new agent, editing a prompt after failures, reviewing a delegated PR or invocation transcript, or changing any contract the agents read (tool descriptions, host notices, issue AC conventions). Grounded in the 2026-07 delegation-failure corpus.
---

# Agent prompt engineering for factor-q

Evidence base: 54 delegated PRs (Jul 2026); 19 human-corrected, 6 unlanded. Full
analysis: `factor-q-delegation-failure-analysis.md` (v2) and
`m0-issue-fix-prompt-diagnosis.md`. Every rule below cites the failure that
motivated it. Keep it that way — that is Rule 0.

## Rule 0 — every rule carries its failure

A prompt is a stack of countermeasures. A rule without its motivating failure
is dogma: the next editor cannot see the trade-off, deletes or doubles the
rule, and the pendulum swings. One full swing is on record: budget-death by
over-reading → the economy rules → rubric-satisficing and blast-radius misses
(#210, #321, #322). When you add a rule, annotate the issue/PR it answers.
When you revise one, name the failure the revision targets **and** the failure
the old rule was guarding — you are choosing a point on an axis, not fixing a
bug.

## The attribution ladder — run it before blaming the model

A finding lands on model behaviour only after cheaper causes are excluded, in
order:

1. **Infrastructure.** Redelivery/duplication, timeouts, sandbox and network
   gaps. (The "blind triple retry" was a NATS redelivery storm — #327; the two
   *correct* attempts were closed unread as duplicates. A missing crates-io
   allowlist entry silently downgrades validation.)
2. **Contract.** Stale, impossible, or underdetermined acceptance criteria.
   (The "verification-evasion" drain-drill disclaimer was literal AC
   compliance; #189's refined body decayed within ~30h of refinement.)
3. **Harness signal conflict.** Prompt, tool descriptions, host notices, and
   contract docs disagreeing. (`report_outcome`'s description promised
   bare-turn success while the harness nudged the opposite — four stale copies
   of one dead contract.)
4. **Model behaviour.** Only what survives: rubric-satisficing, oracle
   deletion, blast-radius misses, deep-invariant errors.

Corollary: an output-only reviewer systematically over-attributes to the
model. Any reviewer — human, Claude, or a future transcript-review agent —
must be handed the **dispatch-time contract snapshot** (the issue body as the
agent saw it, not as it is now) and the **event timeline**, not just the diff.

## Objective-function hygiene

- The enumerable checklist is never the whole objective. "Smallest change" is
  a vector: **smallest in expansion, full in propagation**. Pair every
  scope-discipline clause with a propagation clause and a census procedure
  (`git grep` the old form → migrate each hit or justify it). Exhibits: #210
  (rename, zero call sites migrated), #321 (errors moved to stderr, tracing
  logs left behind).
- Intent governs where criteria underdetermine; criteria text wins where they
  conflict, flagged. The enumerated instance of a rule implies its
  unenumerated neighbours.
- ACs must define the load-bearing noun so the degenerate reading fails a
  **named machine check**. If only a human reading the diff can judge a
  requirement, build the gate first (`just lint-sources`, #326), then
  dispatch. The groomer's bar — "write the AC so the rejected branch cannot
  satisfy it" — is necessary but not sufficient without the gate: #189 was
  refined and still produced `include!` splicing.

## Oracle preservation — always explicit

Never delete, weaken, `#[ignore]`, or skip a test, doc assertion, lint, or CI
gate to make validation pass; obsoleted-by-AC is the only path and must be
attested next to the criterion that obsoletes it. The absence of this rule is
the specification of the shortcut (#210, #206: tests and docs deleted to get
green). A deleted failing test is a disguised failure.

## Class conditioning

One prompt cannot serve opposite failure modes. **Patch** work fails by
mapping when it should edit (budget-death); **restructure** work fails by
editing before it holds the map (`include!` splicing is what patch-mode
reading rules produce when pointed at a 5,000-line split). Condition the
exploration budget on class; mode declared in the PR body; map phases built
from cheap instruments (grep inventories, consumer census), never whole-file
reads. As the fleet grows, prefer per-class definitions derived from a shared
spine — task-class as the type, the prompt as the derived interface.

## Attestation and honesty

- Evidence-keyed attestation, one row per AC: met / not-met / deferred /
  not-run (permitted), plus the command run and its observed result or
  file:line. **Assertions without evidence are not-met.**
- Couple attestation to the outcome protocol: any not-met/deferred row ⇒
  `partial`. All termination through `report_outcome`; the summary is the run
  report.
- The honesty protocol is the fleet's most valuable measured property — zero
  deception in 54 PRs; rejected work was rejected on honest self-description.
  Never add an incentive that makes an honest `partial` worse for the agent
  than a disguised success, and never let a review punish disclosure itself.

## Signal consistency

Every behavioural contract the agent reads exists in several places: the
prompt, tool `description()` strings, harness notices, contract docs, issue
conventions. On any semantics change, census the phrasing across all of them
and fix every copy in one commit (the `report_outcome` fix touched four
sites). A prompt revision that contradicts a live tool description ships a
contradiction to the model at runtime.

## Strategy-stochastic tasks

Same prompt, same issue, same model, minutes apart: one `include!` splice and
two correct module trees (#322 vs #323/#324). For refactors and designs, run
best-of-n with a mechanical discriminator (lint-sources, build,
golden-master) filtering before human review; harvest survivors, close the
rest with reasons. The redelivery storm ran this experiment by accident and
threw away the winners.

## Measurement

- Intervention rate per model per class, computable from the PR-body
  `provenance:` line (current, task-mix confounded: sol 14/24 needing
  intervention, terra 6/12, opus 0/2-trivial).
- Corrections live in **corrective commit messages and PR-close comments**,
  not review threads (10 inline review comments exist across 54 PRs). Point
  extractors there.
- Refinement decays (~30h at current velocity): re-ground at dispatch, not on
  a cron cadence.

## Continuous transcript review — pipeline shape

What this corpus review demonstrated, mechanized:

1. **Deterministic extraction layer** (scripts, no LLM): PR↔issue↔commit
   mapping via provenance identities; corrective-commit detection
   (agent-authored followed by human/Claude-co-authored on the same
   branch/files); attestation-table parsing; label timelines. The existing
   `analyze_prs*.py` / `enrich.py` are the seed.
2. **Judgment layer** (model): runs the attribution ladder over each failed or
   corrected run, with the dispatch-time contract snapshot and timeline in
   context; checks attestation rows against evidence; runs the propagation
   census on the diff.
3. **Findings as events**: every finding carries its evidence tier and a
   falsifiable statement; later evidence appends a *revision event* rather
   than rewriting — the v1→v2 corrections in this analysis are the pattern,
   and the correction record is itself calibration data about which evidence
   tiers mislead.
4. **Operator statements are hypotheses too.** When the operator's stated
   intent or memory disagrees with the code, verify against the repo; the
   *diff between memory and code* is where contract-drift bugs live (the
   `report_outcome` description bug was found exactly this way).
5. **Close the loop structurally.** A confirmed model-behaviour finding
   yields a prompt edit (Rule 0 annotated); a contract finding yields an
   issue-template or groomer change; a harness finding yields a gate or a
   consistency fix. Exhortation is not a countermeasure.

## Procedures

**New agent.** Start from the spine (autonomy, honesty/`report_outcome`,
provenance identity, budget discipline). Pick the class(es) it serves. Write
behavioural rules only against known failures, each cited. Check signal
consistency against the tool descriptions it will see. Define the attestation
format. Dry-run against a closed issue with a known-good outcome before
fleet dispatch.

**Prompt revision.** Pull the exhibits (corrective commits, close comments)
→ run the attribution ladder → target only surviving model-behaviour findings
→ one falsifiable change per rule touched, provenance-annotated → census any
contract phrasing changed → compare intervention rate before/after.

**Run/transcript review.** Ladder first; then attestation-vs-evidence; then
diff-vs-intent (propagation census); file findings with evidence tier;
revise by appending, never by silent rewrite.