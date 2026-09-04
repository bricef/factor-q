# Code quality metrics — beyond file and function size

*Captured 2026-07-27 against the size-ratchet branch that became PR #388, at what is now `6bea27a` on `main`. A conversational analysis, recorded verbatim, prompted by the question: now that `just quality` reports and gates file and function size, what else is worth watching, and can cross-module coupling be measured? The coupling numbers below come from an ad-hoc `use crate::`-only pass over `fq-runtime` written for this discussion, not from `fq-lint`; the analysis discloses why that makes them a floor — and the `fq-lint --coupling` implementation that followed in this PR measures several of them higher for exactly that reason. Second person throughout is an artifact of the conversational origin.*

---

## Why size gates feel crude — and the loophole they open

Size metrics aren't measuring size. They're a proxy for *concentration of decision-making*, and they proxy it badly enough that they can be satisfied without the underlying problem moving.

The sharpest version of this: **a file-size ratchet with no coupling counterweight actively rewards the wrong refactor.** Splitting `runner.rs` into `runner_a.rs` + `runner_b.rs` that import each other heavily passes `lint-sizes` and makes the codebase strictly worse — you've added a module boundary that carries no semantic weight, which is precisely the thing your domain-first design notes argue against. Right now nothing in `just quality` can tell that split apart from a real one. Coupling metrics are the natural complement not because they add coverage, but because they *close that loophole*.

Second framing issue: **a ratchet stops bleeding, it doesn't heal.** `run_daemon` is baselined at 1373 lines against a 250 cap. Monotonic-non-increase permits it to sit there forever. The five over-cap functions carry ~1,450 lines of overage in total — that's one number, and the useful question is whether it trends down quarter over quarter. Reporting *total debt overhang* rather than six individually-frozen entries turns a permanent amnesty into a visible paydown schedule.

## What the coupling picture actually looks like today

I ran an import-graph pass over `fq-runtime` (top-level modules, `#[cfg(test)]` stripped, production `use crate::` edges):

| module | prod LOC | fan-out | fan-in | pub items | instability |
|---|---|---|---|---|---|
| `worker` | 7,714 | 8 | 5 | 141 | 0.62 |
| `control_plane` | 5,048 | 6 | 5 | 99 | 0.55 |
| `mcp` | 1,849 | 4 | 1 | 23 | 0.80 |
| `events` | 1,328 | 1 | 10 | 104 | **0.09** |
| `views` | 1,257 | 4 | 1 | 27 | 0.80 |
| `agent` | 781 | 1 | 8 | 30 | 0.11 |
| `config` | 744 | 0 | 1 | 30 | 0.00 |

The healthy shape shows up clearly. `events` and `agent` are stable cores — lots of things depend on them, they depend on almost nothing. `mcp` and `views` are leaves — they consume, nobody consumes them. `test_support` (fan-out 8, fan-in 0) is exactly right.

The problem modules are `worker` and `control_plane`: **high fan-in *and* high fan-out simultaneously.** That's the painful quadrant — heavily depended upon, so changing them is expensive; heavily dependent, so they're changed often by upstream churn. That combination, not their line count, is why they're the two files every review keeps naming. Their size is a symptom.

One mutual cycle: `agent ↔ events`. Rust forbids crate cycles but permits module cycles silently, so nothing currently catches these.

I'd note the scalar "instability" number is a heuristic — Martin's formulation needs an abstractness axis that doesn't translate cleanly to Rust. What *does* survive translation is the shape rule: **high fan-in should imply low fan-out.** Gate on the violation, don't report the float.

## What I'd actually add, ranked

**Tier 1 — cheap, fits the existing syn AST in `fq-lint`, ratchetable:**

1. **Module cycles → hard zero.** Binary, no threshold to argue about, no baseline needed beyond the one pair. This is the highest value-per-line-of-linter you'll get.
2. **Fan-in/fan-out with the shape rule.** Ratchet the count of modules in the painful quadrant. This is what closes the file-splitting loophole.
3. **Arity as a coupling metric, not a style one.** You already report it — `run_loop_inner` at 21 params. Twenty-one parameters means twenty-one collaborators; that's the coupling number, stated in the clearest possible terms. Ratchet it like size.
4. **`#[allow]` census by lint.** Your 18 allows (14 `too_many_arguments`) are a debt register that currently costs nothing to grow. One ratcheted count makes each new allow a visible decision.
5. **Single-consumer `pub` items.** A `pub` used by exactly one other module should be `pub(crate)` or private. Directly actionable encapsulation debt — and `worker`'s 141 pub items suggest a real haul there.

**Tier 2 — highest signal available, needs git history rather than the AST:**

<!-- markdownlint-disable MD029 -->

6. **Change coupling (logical coupling).** Files that repeatedly change in the same commit but live in different modules. This is the best coupling metric in practice because it measures *actual* coupling rather than declared coupling — two files that always change together are one concept split across a seam that isn't real. It's a `git log --name-only` parse, no instrumentation. And it independently catches the bad-split failure mode: if a `runner.rs` split produces two files that always change together, you split the wrong seam, and only this metric can tell you.
7. **Churn × size hotspots.** The size gate says "`runner.rs` is big." Churn says "and you edit it every week" — which is the argument that converts *should* refactor into *must*. Given the dogfood loop's commit density, you have unusually good data for this.

**Tier 3 — worth knowing, different axis:**

8. **Mutation score (`cargo-mutants`).** Your review named the test-suite gap precisely: *"the suite thoroughly explores the state space it imagined and does not probe the boundary of that imagination."* Coverage percentage won't find that; mutation testing is the one measurement that does, because it attacks the suite rather than the code. For a repo whose strongest asset is verification, this is the metric that proves the asset is load-bearing rather than merely voluminous. Expensive — run it sampled or incrementally on changed files, not in `just quality`.
9. **`cargo-deny` / `cargo-audit`.** Neither is present; finding 3.6 (510 crates, no vuln scanning) is still open. This is a quality gate as much as a security one.
10. **Cyclomatic/cognitive complexity** — I'd skip it. It correlates so heavily with function length that it adds little over what `lint-creep` already tells you.

<!-- markdownlint-enable MD029 -->

## One measurement-correctness note

Route all of this through `fq-lint`'s syn layer, never grep. Concrete evidence: a naive grep for panic sites in `services/*/src` returns 2,641; the cleanroom review's production figure is ~90. The 30× gap is entirely inline `#[cfg(test)]` modules. That's the same class of error the justfile already documents from the hand-rolled line scanner — the AST either parses exactly or fails loudly.

Also worth knowing before you build the import graph: `use` statements undercount Rust coupling. Fully-qualified inline paths (`crate::foo::bar()`), re-exports, and trait-method resolution all create real edges that no `use` records. My numbers above are `use`-only and therefore a floor. A `syn::visit` pass over all `Path` expressions gets closer.

## The thing I'd genuinely push back on

Your justfile articulates the selection criterion better than I could: *"Reviews reliably land work that decomposes into issues and reliably do not land structural work, so this is a gate rather than a preference."* That's the right test — **does this metric convert a structural problem into a merge-blocking fact?**

By that test, most metrics fail. Complexity scores, maintainability indices, coverage percentages — they generate dashboards nobody acts on. I'd add cycles, fan-in/fan-out, and arity as *gates*, keep change coupling and churn as a *quarterly report* that feeds issue creation, and resist the urge to gate anything else until those have visibly moved something. Adding gates faster than you act on them trains everyone to bless the baseline, which is worse than not measuring.

**Where I'd start:** module cycles (an afternoon, hard zero, no baseline debate) and the debt-overhang number for the existing ratchets (trivial, and it converts your current permanent amnesty into a trend you can watch).
