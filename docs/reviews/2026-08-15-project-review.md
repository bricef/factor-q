# Project review at the clean-surface boundary

*A self-assessment taken 2026-08-15 against `main` @ `eb6dd27`, as the
registry-first clean-surface refactor (ADR-0006 + ADR-0031) approaches
its Phase-4 acceptance. Method: a synthesis review, not a cleanroom
read — it re-reads the full corpus in this folder, all 110 open issues,
and the active plans; code verification was limited to replicating the
edge-migration gate's scan, reading the declared surface snapshot, and
re-measuring the files prior reviews flagged. It builds on the
[2026-07-25 cleanroom review](2026-07-25-factor-q-cleanroom-review.md)
and the [2026-07-27 metrics discussion](2026-07-27-code-quality-metrics.md),
and diffs the project against what they recorded.*

## Verdict

The refactor is one verb from done, and further along than its own
plan documents claim. The most alarming finding of the July reviews —
that the change loop appends but cannot restructure — has been
answered with evidence: the god-files the 07-25 review watched grow
have since been split, by the loop, under the ratchet. What has *not*
moved in the same six weeks is exactly what the review corpus predicts
would not: the proof-of-concept'd security findings, the Q instrument,
and the product itself (orchestration, non-code workloads). The next
steps are therefore mostly about refusing momentum: land the last
verb, run a truth pass, pay the security cluster down, and then pivot
capacity from substrate to product rather than letting Phases 5–6
carry the surface work onward by inertia.

## 1. Where the refactor actually is

Verified against code, not the plans — the plan docs lag the tree.

| Phase ([registry-and-split plan](../plans/active/2026-07-20-registry-and-split-execution.md)) | State |
|---|---|
| 0 golden net · 1 `fq-ops` crate · 2 authenticated edge · 3 exemplars | Done, as claimed |
| 4 fleet migration | **One verb remains** (see below); gate at zero; ReadService deleted; dashboard on the edge |
| 5 binary split (`fq` / `fqd`) | Not started — `fq-cli` still links `fq-runtime`, `async-nats`, `sqlx`; `fqd` exists only as a second binary target; no distribution changes |
| 6 MCP operator face (#84) | Not started |

What Phase 4 has produced, concretely: the localhost tarpc read
service is gone (`b35b571`); the dashboard authenticates to the edge
with a token attenuated to six read grants and refuses to start
without one (`b5c0233`); `invocation.active` replaced the old
`active_invocations` RPC (`ab03ed1`); the migration-gate test
(`fq-cli/tests/edge_migration_gate.rs`) asserts `REMAINING: usize = 0`
and its scan, replicated exactly for this review, confirms zero
unmarked legacy call points.

**The one remaining verb.** `fq invocation resume` still does a raw
NATS request/reply (`fq-cli/src/invocations.rs:170`,
`fq.control.invocation.resume` — `bus.rs:124` calls it "the last of
the `fq.control.*` subjects"). It is invisible to the gate's five
patterns, so the counter's zero slightly overstates completion — a
blind-spot class the gate's own comments acknowledge. There is no
`Invocation::Resume` op id yet. Flipping it is not just cleanup: done
per the plan's amendment (fallible steps before injection, terminality
read at a watermark), it closes **#383** — the drop/resume
coordination race — by construction.

**Documents lagging the tree**, for the truth pass:
[STATUS.md](../../STATUS.md) (self-dated 2026-07-20) still says the
dashboard reads "the daemon's localhost tarpc read service"; the
[Phase-4 call-point inventory](../plans/active/2026-07-28-phase-4-call-point-inventory.md)
still counts two remaining flips when the true count is one; the
committed architecture diagram predates the read service's
retirement.

## 2. The surface work has already named its own successors

The August issues are the maintainer's articulation of what follows,
and they arrive pre-sequenced:

- **#465 → #468.** Byte-budgeted paging first, so List becomes the
  bulk historical read; then List/Stream split by definition
  (historical/paginated/redacted vs immediate/live/unredacted), which
  retires a whole class of "List and Stream disagree" bugs
  permanently.
- **#469, narrowed.** The transport question ("the edge is
  request/response but the domain is event-driven") was headed toward
  a forcing function, but the decision recorded on #478 — a **protocol
  gateway**: a separate Rust process that is an ordinary edge client
  on one side and speaks HTTP (or anything) on the other — removes
  *reach* from the argument. What remains of #469 is *shape* only:
  whether the daemon can express a subscription rather than a long
  poll. That is a real question (the `DeadlineExceeded` follow-verb
  defect was shipped, careful code and all), but it no longer blocks
  anything. It should stay open as a question and not become a
  transport migration by drift.
- **#478 → #479.** Migrating the two Go adapters onto the edge via the
  first gateway, then retiring direct `fq.trigger.*` publishing. Worth
  saying plainly: **this is also security work.** It closes the last
  unauthenticated write path into the system (today mitigated only by
  NATS's localhost binding and a public dev token) and removes the
  header-less trigger-id assignment branch whose redelivery wrinkle is
  unfixable while the path exists. The confused-deputy discipline
  recorded on #478 — the gateway holds an attenuated token, never a
  broad one — should be treated as an acceptance criterion of the
  first gateway, not a note.

This cluster is the natural continuation while the surface context is
warm, and it is bounded: it does not reopen the registry design.

## 3. The ledger against the July reviews

**What moved.** The structural-debt finding has inverted. At the
07-25 review, `runner.rs` had grown to 7,387 lines and fq-cli's
`lib.rs` to 5,877, and Part 2 of that review concluded the loop
"selectively executes issue-shaped work" and could not restructure.
Since then: `runner.rs` is 2,636 lines with the rest split into
`runner/{config,failure,llm,replay,server_request}.rs` (#78, closed);
fq-cli's `lib.rs` is 1,329 lines across a module tree (#189's target,
substantially done); `mcp.rs` shrank slightly to 1,851 (#191 still
open). The size ratchet plus escalation did what the 07-25 review
doubted the loop could do. The 07-19 delegation analysis's
countermeasures also held: exactly-once dispatch has a merged inbox
(#328), an accepted-draft ADR-0032, and an active plan against the
remaining storm (#327). `LlmFailure` became a real event (#447) —
an aspirational design graduating on schedule.

**What did not move.** Verified, not inferred:

1. **The 07-25 security cluster is untouched.** `install.sh` still
   fails open when the `.sha256` fetch fails (`install.sh:72`,
   flagged in both cleanroom reviews); no cargo audit/deny/Dependabot
   anywhere in CI (#406, flagged twice); the PoC'd dangling-symlink
   write escape (#399) and the `/proc` env exposure (#400) are three
   weeks past their proof-of-concept with no commits touching them;
   token expiry/revocation (#404), pricing pin (#408), posture doc
   (#401) likewise. These are mostly small, they are the named gate on
   the multi-node hold (#414), and the fleet runs daily against this
   repo meanwhile.
2. **Q is still unmeasured.** The oldest theme in the corpus (every
   review since 2026-07-05). The capability ladder exists as a
   document (#413), M0 instrumentation as an issue (#340); neither has
   an owner-confirmed decision, so M1 remains undecidable.
3. **The product remains unbuilt.** No graph executor, no multi-agent
   handoff, no non-code workload evidence (#428) — while roughly six
   further weeks went into substrate. The clean surface is excellent
   substrate; it is still substrate. #437 (reasoning as message
   parts), the one maintainer-confirmed precondition, has had no
   movement since 2026-07-28.
4. **Context management** (#77) — called "the single most
   consequential gap" by the 07-09 review — is still undesigned.

## 4. Backlog health

110 open issues. The clusters, roughly: the August surface successors
(~10), the 07-25 security findings (~10), the 07-27 quality-metrics
proposals (~12, mostly `fleet:needs-decision`), runtime-correctness
bugs (~15), dogfood/ops (~10), and the long tail of Phase-2 features
and July debt sweeps.

Three observations:

- **The refactor has orphaned some issues.** #254 (split the CLI and
  read via the tarpc read service) describes a mechanism that no
  longer exists — its intent is now Phase 5. #264 (sqlx-free CLI) *is*
  Phase 5's acceptance criterion and should be folded into it rather
  than standing alone. #189 is near-closable against the current tree.
  #186 (invocation detail's 200-event scan) needs re-checking against
  `invocation.active`. The master findings issues (#398, #166, #75)
  deserve a sweep now that many children are closed.
- **The refinement queue is the throughput constraint, still.** The
  07-19 analysis found refinement decays and the refined queue runs
  empty; today exactly one open issue carries `fleet:refined` (#185)
  while ~25 sit at `fleet:needs-decision`. The fleet can only be as
  useful as the queue is refined; the needs-decision pile is
  maintainer time, not fleet time.
- **A handful of correctness bugs deserve triage before features**:
  #475 (nothing consumes `worker.orphaned` — dead workers' invocations
  never recover), #453 (the rolling progress summary has never fired —
  a subject-name mismatch), #467 (read-only store open skips schema
  verification), #409 (write-only `SCHEMA_VERSION`, the 07-25 review's
  4.1). The first two are cheap and embarrassing in a system whose
  identity is autonomous operation.

## 5. What I'd do first

Ranked by leverage against the above, assuming roughly the capacity
that executed Phase 4.

1. **Flip `invocation.resume`** per the amendment and declare Phase 4.
   One verb, closes #383 structurally, makes the gate's zero true
   without asterisks.
2. **Run the truth pass** (#458's one-off sweep, plus grooming):
   refresh STATUS.md and the call-point inventory, regenerate the
   architecture diagram, close/rescope #254, #264, #189, #186, sweep
   the master findings issues. Cheap, and every future session
   reads these first.
3. **A security week, before more autonomy.** #399, #400, #405, #406,
   #408 are small and PoC'd or twice-flagged; #401 writes the honest
   posture down. This is also the named exit condition on the
   multi-node hold — paying it unblocks strategy, not just hygiene.
4. **Build the first gateway as the #478 adapter migration, then
   retire the raw trigger path (#479).** Surface completion and the
   closure of the last unauthenticated write path in one move, with
   the attenuated-token discipline as acceptance.
5. **#465 then #468**, the last of the surface semantics. Keep #469
   open as a shape question.
6. **Hold Phases 5–6 with named exit conditions** instead of letting
   them ride momentum: Phase 5 (two-binary distribution) becomes due
   with the first tagged release (it is an ADR-0022 concern); Phase 6
   (MCP face) with the first external consumer. Write the holds into
   the plan so they read as decisions, not neglect — the repo's own
   rule.
7. **Then the product, deliberately**: #437 first (confirmed
   precondition, and it unblocks #424's coupling work too), then the
   two-node vertical under the
   [graph-executor plan](../plans/active/2026-07-07-graph-executor-two-node-vertical.md),
   with the non-code workload (#428) run against whatever exists —
   the transfer-cost measurement does not need orchestration to start.
8. **Make M1 decidable** (#340/#413): pick the instrument — ladder or
   proxy metrics — and start counting. Every review since July has
   filed this; it is the difference between "the loop lands PRs" and
   "the loop is worth what it costs."

The one-sentence version: finish the surface by closing it — the last
verb, the truth pass, the trigger path — spend one deliberate week on
the security debt that gates everything, and then let the next
six-week block belong to the product for the first time since May.
