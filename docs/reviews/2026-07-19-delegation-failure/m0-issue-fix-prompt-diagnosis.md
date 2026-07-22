# m0-issue-fix prompt diagnosis

Prompt: `fq-dogfood/agents/m0-issue-fix.md` (gpt-5.6-sol, $15 budget, 200 iterations).
Evidence base: the v2 failure analysis (54 agent PRs; 19 corrected, 6 unlanded).

## The prompt's own history explains its shape

"Your last several predecessors failed the same way: they spent their whole budget
*understanding* and never *edited*." The two economy rules — edit as soon as you can
name the change; never read a whole file over ~150 lines — are a correction for
budget-death by over-reading. The correction worked: recent runs ship PRs. But the
failure corpus shows what the overcorrection bought. This is a control loop
oscillating: analysis-paralysis → economy rules → satisficing and blast-radius misses.
The skill's job is to damp the oscillation, not swing it back.

## What the prompt demonstrably gets right — keep all of it

- **The honesty protocol works.** `report_outcome` with failed/blocked/partial, "a
  failure disguised as success wastes a human review cycle," "a red branch with an
  honest report beats a branch committed while red." Empirically: PR #322 disclosed its
  `include!` strategy and the un-run drill; Brice's review opens "honestly reported."
  Zero deception incidents in 54 PRs.
- **Provenance discipline** (step 2) produced the per-invocation identities and
  `model=` lines that made this entire analysis attributable. Keep verbatim.
- **Autonomy framing** ("no human is available... do not stall") is correct for the
  loop, and the record-assumptions-in-PR-body instruction is consistently followed.
- **The fleet-triage pre-flight** (#142) is the right mechanism for cheap grounding.

## Finding 1 — the economy rules select degenerate strategies on large tasks

"Never `file_read` a whole file over ~150 lines... the single most common way these
runs die." For #189 — split a 5,009-line file — this rule makes comprehension of the
module's symbol/visibility graph effectively forbidden. `include!` splicing is
precisely the strategy that requires no global understanding: cut at the section
banners, move text, keep one translation unit. A proper module tree requires the
whole-file reading the prompt calls fatal. **The degenerate strategy in PR #322 wasn't
(only) model laziness; it was the rational optimum under the prompt's stated
incentives.** That #323/#324 chose modules anyway shows the gradient is resistible —
but the prompt points downhill toward the shortcut.

Same mechanism, smaller scale: a rename's consumer sweep (#210: examples, templates,
QUICKSTART, smoke suite — zero migrated) is exactly the "gathering just a bit more
context" the prompt orders the agent to stop doing.

## Finding 2 — "smallest change that satisfies the acceptance criteria" specifies rubric-satisficing

The phrase appears twice (step 4 and Constraints), flanked by "do only what the issue
asks; do not expand it" and "nothing outside its stated scope." Scope discipline
exists for good reason, but the prompt never distinguishes **scope expansion** (adding
unrequested behavior — correctly forbidden) from **consistency completion**
(propagating the requested change to every consumer — required for the work to be
true). #190→#321 is the clean exhibit: AC named errors-on-stderr; tracing logs and
progress output kept polluting stdout; fixing them is arguably "outside" the
enumerated list. The prompt makes the enumerated checklist the objective function and
provides no sentence saying the checklist is necessary but not sufficient.

## Finding 3 — validation has an escape hatch and no oracle-preservation rule

- "If [`just ci`] cannot finish within budget, run the narrowest relevant subset...
  and say which you ran." Under budget pressure the subset becomes the norm; the
  corrections that had to *add* tests (#263, #293, #306, #319, #320) are the residue.
  Disclosure makes shallow validation compliant — honest, but shallow.
- **Nothing forbids deleting or weakening tests, docs, or lints.** "Never commit red"
  + budget pressure + no preservation rule = deleting the failing test is a compliant
  path to green. #210 (registry tests + docs deleted) and #206 (docs deleted) are this
  rule's absence, made flesh.
- Step 7 asks the body to "note which acceptance criteria are met" — assertion, not
  evidence. Issue #201 already pioneered the stronger form ("each numbered item done
  or explicitly skipped with a reason"); it lives in one issue instead of the prompt.

## Finding 4 — one prompt serves task classes with opposite needs

The economy rules are tuned for "docs-drift and small-fix issues" (the prompt says so)
and are catastrophic for refactor-class work. The corpus splits the same way: small
fixes mostly land clean; the large tasks produce the deep failures. Task class is
already legible at dispatch (labels, groomer verdicts, diff-size estimates). This is
your define-once pattern applied to prompting: **the task contract carries a class;
the prompt is a derived rendering per class** — shared spine (honesty, provenance,
procedure), class-specific exploration budget and strategy constraints.

## Finding 5 — minor but load-bearing

- The sandbox network allowlist is `github.com`/`api.github.com` only. Builds work
  because `CARGO_HOME`/sccache are warm; an issue introducing a *new* dependency fails
  the fetch and silently triggers the narrow-subset escape hatch. Add the crates
  registry or pre-vendor.
- `m0-review-fix` (opus-4.8) exists but no main commit carries its identity — the
  review-repair loop that would offload your interactive corrections isn't actually
  running (or never lands). Worth investigating separately.

## Concrete edit list (minimal diffs, ordered by expected yield)

1. **Consistency-completion counterweight** (targets F2), after the scope sentence:
   "Scope discipline means no *new* behavior beyond the issue. It does not mean
   partial application of the requested change: if the issue changes a name, a
   convention, or a contract, every consumer of it in this repo is in scope —
   enumerate them (grep) and either migrate each or list it in the PR body as
   intentionally untouched, with a reason. The acceptance criteria are necessary, not
   sufficient; the issue's stated intent governs when they underdetermine."
2. **Oracle preservation** (F3): "Never delete, weaken, or skip a test, doc assertion,
   or lint to make validation pass. If a check is genuinely obsoleted by the issue,
   say so in the PR body next to the acceptance criterion that obsoletes it. A
   deleted-failing-test is treated as a disguised failure."
3. **Evidence-keyed attestation** (F3), replacing "note which acceptance criteria are
   met": PR body carries one line per AC — met / not-met / deferred, plus the command
   run and its observed result (or file:line). "Considered but not run" only where the
   AC itself permits it.
4. **Class-conditional exploration budget** (F1/F4): keep the economy rules for
   small-fix/docs classes; for refactor-class issues invert — an explicit map phase
   (symbol inventory, consumer census via grep — cheap, targeted, not whole-file
   reads) before any edit, and a strategy gate: run `just lint-sources` before opening
   the PR (it now rejects `include!` splicing mechanically).
5. **Escape-hatch tightening** (F3): narrow-subset validation must name what was *not*
   run against which AC, in the attestation table — not prose.
6. Add the crates registry to the network allowlist (F5).

Longer-term (the skill's territory): per-class agent definitions derived from a typed
task contract; best-of-n with `lint-sources`/golden-master as discriminators for
strategy-stochastic tasks; dispatch-time re-grounding; per-model adapter notes (sol
vs terra profiles once n grows — current split: sol 14/24 needing intervention, terra
6/12, task-mix confounded).

## Note on model attribution

Kimi K3 appears nowhere in the provenance record; the measurable fleet is gpt-5.6-sol
+ gpt-5.6-terra (+ opus-4.8 on doc-drift). If K3 ran, it was in the pre-provenance
early cohort or outside this pipeline. Worth confirming before designing K3 adapters.