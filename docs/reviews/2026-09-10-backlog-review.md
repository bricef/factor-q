# Backlog review — 2026-09-10

Verified against `main` @ `22d4aaba` (2026-09-10). A sweep of every open issue against the tree at that commit, alongside the plans, reviews and ADRs — then the corrections applied the same day.

121 open issues, all 121 read in full (body + comments) and checked claim-by-claim against the pinned tree, alongside the production-readiness review (2026-09-03), the five active plans, the July review documents, the draft ADRs, and the design docs.

## Headline

- **The roadmap's next step is unchanged and human-gated:** finish the dogfood host migration (#587), then Phase 2 "the record is trustworthy" by hand, one PR each. Eight of Phase 2's ten items have no GitHub issue yet.
- **Six issues were already fixed on main and are now closed with evidence comments:** #117, #189, #254, #264, #367, #390. Two more collapsed into siblings and are closed too: #170 → #523, #70 → #42 (cross-reference comments posted on #523 and #42). Closed 2026-09-10 evening, each after re-verifying every acceptance criterion at 22d4aaba.
- **Two verified sandbox escapes with a public PoC have been open seven weeks** (#399, #400). Half a day each. They are the top of the list.
- **The biggest single decision** is whether the ADR-0032 exactly-once claim registry is still the shape wanted (#327). The draft ADR rests on a false premise: it says `Nats-Msg-Id` dedup is "already in place", and nothing under `services/` implements it.
- **Labels:** 53 issues corrected (14 had no labels at all). `fleet:refined` removed from #185 because its anchors are dead.
- **Decisions:** 79 issue-level judgement calls across 58 issues, plus 20-odd that live only in plans, reviews and ADRs. Curated into 51 register entries below, each traced to its issue or document anchor.

## Verdict counts

| Verdict | Count | Meaning |
|---|---|---|
| VALID | 76 | problem confirmed present, body accurate enough to act on |
| PARTIAL | 26 | some sub-items landed; rescope needed |
| VALID-STALE | 13 | still real, but the body's anchors or premises drifted |
| RESOLVED | 6 | fixed on main; close with evidence |

Fleet suitability after this pass: 51 `fleet:candidate`, 40 `fleet:needs-decision`, 30 need a human.

---

## 1. Themes

Sixteen areas. Counts are open issues; the note is the state of the area, not a list.

| # | Theme | Issues | State |
|---|---|---|---|
| 1 | **Sandbox & isolation security** | #399 #400 #188 #208 #209 #183 #401 #403 | Two verified escapes unfixed. #208 (egress proxy) is the roadmap's first item after Phase 2; #209 is held and names a partly-superseded ADR. |
| 2 | **Secrets & credential hygiene** | #523 #72 #402 #558 #593 #404 | #523 is a live cross-agent credential confusion. #402 needs no code, only a scoped PAT. `HighEntropyRedactor` now exists for #72 to reuse. |
| 3 | **Dispatch, recovery & record integrity** | #327 #479 #669 #180 #181 #475 #383 #410 #70 #338 #647 #467 | The Phase 2 pillar. #327 is unstarted and decision-gated; #181/#180/#475 are the cheap wedge-removers; #647/#669 sit on the migration path. |
| 4 | **Rate limits, cost & pricing** | #278 #651 #650 #42 #408 #344 #660 #91 | PR #649 (per-model throttle + deferral) is open, not merged. #408 and #344 pull in opposite directions (pin vs refresh) and neither cites the other. |
| 5 | **Config, reload & definitions** | #185 #114 #276 #170 #204 #187 #275 #505 | `fq reload` still re-reads only agents; #276 inherits #114's decision. #185 is a one-field fix that strands suspended work. |
| 6 | **Edge & API shape** | #469 #478 #517 #464 #465 #468 #254 #264 | #254/#264 are done (close). The rest is one design conversation (#469) with four tickets written in a vocabulary it might retire. |
| 7 | **Observability & operator surface** | #453 #342 #186 #525 #506 #535 #225 #534 | #453 has never fired (subject typo) and is a fleet-marked Phase 2 item. #342 is now scoped to five gauges by the roadmap. |
| 8 | **Agent-facing affordances** (notice channel, context) | #88 #157 #158 #159 #85 #148 #77 | The notice channel landed (#155) and carries nothing in production: three small PRs (#157 #158 #159) turn dead schema into behaviour. #77 is the July review's risk #1 and unphased. |
| 9 | **Storage / fq-store** | #174 #203 #89 #202 #253 #255 #353 | Explicitly parked until Phase 2 exits. #174 is real data loss on a store the runtime does not yet depend on. #255 closes on one sentence. |
| 10 | **Code quality gates & coupling** | #189 #415 #416 #417 #418 #419 #420 #421 #422 #423 #424 #391 #392 #507 | #189 is done. The #424 epic is frozen on a block whose trigger (#437) closed 2026-09-04. Every number in #415/#418/#419/#423 is stale, in the good direction. |
| 11 | **Test hygiene** | #198 #199 #411 #433 #258 #390 #473 #670 | #390 is done. #433 has already caused a false CI red and is 20 files, not six; #670 (filed during this review) is the same "Runtime ready" race hitting `edge_reports`. #199 hides a security gate that cannot fail (smoke sandbox-denial asserts nothing) and should be split out. |
| 12 | **CI & supply-chain gates** | #643 #594 #640 #407 | #643: the Go adapter gate reports green on failure, and did. #640 was stale on arrival (the glob was widened three days before filing). |
| 13 | **Docs drift** | #458 #512 #197 | #512 and #197 are mostly landed; rescope both. #458 can start now (Phase 4 closed). |
| 14 | **Dogfood ops & deployment** | #587 #667 #292 #339 #367 #205 | #587's build-out is complete; only the live cutover remains. #667 is the deploy path the cutover lands on. #367 and most of #339/#205 are done. |
| 15 | **Fleet tooling** | #123 #117 #389 #142 | #117 is done. #123 is now cheap because #117 shipped the ground-truth query it needs. #389 exposes a doctrine contradiction between AGENTS.md and a maintainer comment. |
| 16 | **Strategy, epics & umbrellas** | #413 #414 #428 #166 #75 #398 #340 #257 #343 #345 #341 | The three umbrellas (#75 #166 #398) all claim zero progress while 13/18, 20/37 and 4/14 children are closed. #341's premise is false (rmcp 1.8.0 has no Audio variant either). |

Two structural observations from the document pass:

- **The roadmap is not a map of the backlog.** Its phases reference 38 of 121 open issues. Whole clusters have no roadmap home: the 14-issue quality-ratchet cluster, the edge-shape cluster, the notice channel, fleet tooling.
- **The inverse gap is larger.** 21 roadmap items have no issue (8 of Phase 2's 10). Phase 0 was filed as issues (#539–#553, tracker #554) before execution; Phases 2–4 never got that pass.

---

## 2. Top five, in priority order

Above the list: the standing next step is the dogfood cutover (#587). It is not work anyone can dispatch; it is a window to take, and every human item is on the maintainer's side (converge the guest, then bootstrap).

### 1. #399 + #400 — the two verified sandbox escapes

`fq-tools/src/sandbox.rs:381-414` still branches on `target.exists()` and lexically joins a dangling symlink's filename, approving a write the OS then follows out of the sandbox. `exec.rs` clears the child env, but the child is the same uid and reads `/proc/<fqd-pid>/environ`; there is no `PR_SET_DUMPABLE` anywhere in production code and `libc` is already a dependency. Half a day each. A PoC has sat in a public tracker since 2026-07-25. Both are Phase 2 item 9 and two of the three remaining exit criteria on #414's hold. Decision D1 asks whether they wait for Phase 2; recommendation: no.

### 2. #667 — `deploy.sh` cannot tell a refused config from a sick broker

Today's #664 made `fq-cron` exit 1 on a jobless config before any broker contact. A container that dies in under 100 ms never completes a probe, so `bring_up` reports "not healthy after 90s", `--auto` rolls back to the previous sha, which reads the same file from the same volume and dies the same way. The migration lands on exactly this deploy path. Fix before the cutover, not after. Decision D3 attached (does a scheduler refusal fail the deploy at all).

### 3. #523 — shared MCP servers ignore `env` when deduplicating

`mcp/server_config.rs:44-73`: `SharedServerKey::Stdio { command, args }` excludes `env`, so two agents declaring the same server with different credentials collapse to one process and the second runs on the first's credentials, winner by start order. Impact 5, uncommented since 2026-08-27. One policy answer unblocks it (D2). Absorbs the last open criterion of #170, which should close against it.

### 4. #643 — `gate-adapters` returns only the last adapter's status

`justfile:269-271` runs a POSIX `for` under `sh -cu` with no `-e`; a failing `fq-cron` gate is masked when `github-watcher` passes. The 2026-09-08 comment records it happening: a non-compiling `fq-cron` with `just go-ci` exiting 0. Three lines. Until it is fixed every adapter green is decorative, and `fq-cron` is in the stack being migrated. Fleet-dispatchable today.

### 5. #181 — permanent resume conditions reported through the transient error variant

`worker/mod.rs:178` hardwires `ExecutorError::WorkerStore(String)` as transient; `resume()` returns three permanent conditions through it (runner.rs:689, :709, :901). The dispatcher NAKs and JetStream redelivers forever, because #49's consumer still has no `max_deliver`. S-effort typed-variant split; removes one whole poison class ahead of Phase 2, and no longer conflicts with the runner.rs split (#78 is closed).

**Close behind them:** #185 (one-field fix; re-ground then dispatch), #453 (fleet-marked Phase 2 item, fix measured in lines), #180 (control-plane idempotency), #475 (needs D10), #408's pin-and-checksum half (after the split), and #402 (no code: a scoped PAT the maintainer creates).

**Not in the top five on purpose:** #327 is the highest-severity open issue but is decision-gated (D5) and is Phase 2's last item; #587 is a window, not a ticket; #208 is the roadmap's first item after Phase 2 and should be queued, with D12 answered now.

---

## 3. Decisions register

Every judgement call that needs the maintainer, traced to its issue or document anchor. Tier 1 blocks the next step or the top five. Tier 2 is architecture that should be answered before its track un-parks. Tier 3 is strategy, product and process. "No issue" marks decisions that exist only in documents.

### Tier 1 — answer now

- **D1. Do #399/#400 wait for Phase 2 or jump the queue?** Roadmap L671 schedules them as Phase 2 item 9; its own L578 notes the July review's first four items are all still open after 129 commits. → #399, #400. Recommendation: pull forward.
- **D2. Are two declarations of the same MCP server differing only by `env` legal?** (a) illegal, reject loudly; (b) legal, normalise `env` into the key and solve the resulting `name` collision; (c) legal, shared server takes the first env and warns. → #523. Recommendation: (a).
- **D3. Does an `fq-cron` refusal fail the whole deploy?** (a) yes, the scheduler is part of the stack; (b) scope the health wait per service, warn and notify. → #667 item 3. The runbook must state the answer.
- **D4. The cutover.** Take the window now, rehearse once more, or keep deferring (#587 body + comments). Settle in writing that the no-public-address branch is the one being walked (migration plan §Assumptions L18–24; the plan's primary narrative is the unused branch). Certificate: re-issue or copy `infra_caddy-data` (plan §The day, step 5). Identity copy-not-rotate is already decided in the plan.
- **D5. Exactly-once dispatch: is the ADR-0032 claim registry still the shape wanted?** (a) proceed with PR-1/PR-2 and the KV trigger inbox; (b) ship only the hygiene slices (pull discipline, backoff/ack_wait, alarms) and defer the registry; (c) redesign against async-nats 0.50 and the single-daemon container deployment. Also: accept or withdraw the draft ADR. Note ADR-0032 L117–119 claims `Nats-Msg-Id` dedup is already in place; it is not (Phase 2 item C3 is exactly that). → #327; plan `2026-07-18-exactly-once-trigger-dispatch.md` L47–50.
- **D6. Lift the #424 block?** The 2026-07-28 block ("not until #437 lands") has fired: #437 closed 2026-09-04. (a) release the unblocked gates/reports (#417 #418 #419 #421 #423) to the fleet now; (b) keep parked behind Phase 2, which hand-splits the same files; (c) start with #415's decomposition. → #424, #415, #416–#423. Recommendation: (b), but post the note and release #419/#423 as fleet fillers.
- **D7. Pricing table trust.** Pin-and-checksum (#408) vs periodic refresh (#344) vs both. And: is a zero-priced model refused outright, refused unless an explicit override declares it free, or warned? → #408 item 3, #344. Recommendation: split #408, dispatch the pin half now; zero price = refuse unless overridden.
- **D8. Provider throttling.** Limiter grain: global / per-provider / per-(provider, model) / adaptive (#278 §Design decisions). Pause ceiling and operator release: cap `Retry-After`, add `fq throttle release`, or both (#651). PR #649 is open and per-model; decide before it merges.
- **D9. `fq reload` semantics.** Which `fq.toml` keys are hot-swappable; warn-and-apply-the-rest vs reject-whole (#114 §THE DECISION). On an unpriced model at reload: reject-whole, drop-the-agent, or reject-whole-for-pricing-only with the asymmetry documented (#276).
- **D10. Orphaned workers.** Always resume / resume only if the last turn completed / notify only (#475 "Reassign or fail?"). Leases with fencing tokens now, or a plain consumer and revisit when remote workers land.

### Tier 2 — architecture, answer before the track un-parks

- **D11. Which isolation model gets built:** ADR-0010's agent-scoped container, ADR-0028's tool-scoped VFS, or a superseding ADR (both Accepted, neither built). Also: one substrate with #70 or two; whether stdio MCP server args are constrained by the agent's sandbox (#209 comment 2026-09-04). → #209.
- **D12. Egress proxy: bespoke or off-the-shelf CONNECT proxy.** Supply-chain vs owning a network-facing component; reused at the container edge later. → #208 §Sketch. Roadmap: first item after Phase 2.
- **D13. Edge transport and the gateway pattern.** Stay tarpc / gRPC / streaming side-channel / QUIC (#469). ADR for "a new protocol costs a new gateway process" before the first gateway; what authority the gateway token carries (#478, comment 2026-08-13). Does `trigger-wire-contract.md` survive (#479).
- **D14. Edge frame and read contracts.** Single oversized row: truncate on write / byte-budget pages / raise the frame; pin broker `max_payload` to the frame (#465). Drop replay-from-origin from `event.stream` (#468). Turn/DeadLetter identity: derive / mint / accept positional (#464).
- **D15. Token TTL defaults and where the revocation list lives** (control-plane store vs file beside the edge identity). → #404. Becomes urgent at v0.1.0.
- **D16. Is `validate` a report or a command** on the ADR-0006 surface; does it also check the live registry. → #517.
- **D17. Trigger-boundary cost layer:** dispatcher / adapters / edge; refuse-and-dead-letter vs hold-for-authorisation on breach (#42). Internet-facing webhook before #42/#404 land (#343).
- **D18. Secret redaction:** render-time / never-persist / both with an authorised raw-read verb; is `--no-redact` a flag or an authority (#72). Tension with ADR-0026's system-of-record guarantee.
- **D19. Context management:** ADR the seam now / stopgap truncation / hold until Phase 2 exits (#77). The July review's risk #1; the roadmap does not phase it.
- **D20. Storage seams.** Raw `fq-cas put`: real API / long TTL / debug surface (#174). NameIndex split shape (#202). Artifact namespace and cross-agent grants (#89). Shared `Domain` enum; does `fqd` front the store (#353). Accept bounded convergence as the audit's contract (#255: one sentence closes it).
- **D21. Record semantics.** Resume under original definition vs current (#338). Where the drop/resume fence lives: client waits / writer stamps / accept the window (#383). Extend `ConfigSnapshot` (event-schema change) or keep the narrowed contract (#204, #512).
- **D22. Embedded scheduler vs fq-cron as the scheduler** with a daemon-side maintenance consumer (#257). Affects #344, CAS GC, the sweep.
- **D23. Fleet plumbing.** Groomer applier shape: typed runtime tool / narrow agent / ops script (#403). Pre-flight location and freshness (#142). How the agent sees `attempt` (#148). Notice format: the `<host-notice>` sentinel #88's body calls universal, or the JSON tool-result field that #373 (closed 2026-07-24) shipped for interrupted resumes; settle before #157 (#88, corrected by the 2026-09-10 rescope).
- **D24. Warn at load when `exec_cwd` sits beside narrower fs grants?** → #401 item 3.

### Tier 3 — strategy, product, process

- **D25. Capability ladder** (#413): supersede #340 or keep it as L3 calibration; backfill attempts or start clean; ADR or amend VISION directly; where the intervention log lives. Underlying: build the Q instrument or demote the ladder (review 2026-07-25 §1.1).
- **D26. Multi-node hold** (#414): confirm or amend the exit criteria (STATUS L242–247 says still unconfirmed; 4 of 6 have changed state); crude MVP first vs straight to the two-node vertical. The orchestration spike has been recommended by four reviews (2026-04-19, 07-05 §7, 07-09 §7, 07-25 §1.2) and has no issue.
- **D27. Non-code workload** (#428): corpus (Canopy/Pelorus vs repo-owned); does the #414 dependency stand (it currently means "never"); operations domain later or out of scope.
- **D28. Licensing.** BSL vs Apache-2.0; the `chore/apache-2-license` branch (2026-07-29) lands or closes; ADR-0033's revisit triggers are unratified. → #398 §1.4 (never filed), #512 comment 2026-08-26. No issue.
- **D29. Registry plan Phases 6/7:** file issues or record out of scope. → #512 comment 2026-08-26.
- **D30. The five agent-ergonomics children** (#77 #85 #88 #89 #91): hold all behind Phase 2, pull the now-cheap ones forward (#91's warning, #159), or close #89/#77 as not-now. The 2026-09-03 roadmap does not mention them at all (the rescope checked), so they are unscheduled rather than deferred; #75's sequencing hint is the only plan that speaks to them.
- **D31. Fleet ratchet doctrine:** escalate (AGENTS.md:15) vs restructure (maintainer comment on #389). Both currently live in the tree. → #389.
- **D32. Gate policy.** `too_many_lines` advisory vs gated (#392); `HUB_THRESHOLD=4` (#417); ast-grep vs fq-lint (#391); mutation-testing cadence incl. not adopting (#422); audit thresholds (#594). The metrics review's own advice: add no gate until the existing ones move something.
- **D33. Doc sweep scope and the status-header policy** (#458); docs gate beyond `docs/` (#640).
- **D34. M0/M1 proxy metrics:** build / defer / drop (#340). STATUS no longer names it.
- **D35. `nats_url` in `system_startup`:** drop or justify before beta (#593).
- **D36. Transcript read:** narrow the fold or accept and document (#525).
- **D37. Anthropic Admin API credential** (org-wide read scope) yes/no; price the unmodelled multipliers or write them off (#660 items 2–3).
- **D38. `CARGO_TARGET_DIR` for exec children after the cutover** (#587 F8, decide with a measurement).
- **D39. Event-trail lifetime:** wire the ADR-0026 archive or write the 30-day window into README's claim (roadmap Phase 3 item 6). No issue.
- **D40. The "production level" bar** itself: single-tenant, single-host (review L64–78). Confirm; everything inherits it. No issue.
- **D41. Structural work gets its own queue and budget** (review 2026-07-25 Part 2 rec 3). No issue.
- **D42. Promote the eleven fq-ops candidate principles** into design-principles, or decline (2026-07-21 doc). No issue.
- **D43. Fleet-prompt posture:** scope discipline vs consistency completion; per-class prompts (2026-07-19 delegation-failure analysis). Prompts live in fq-dogfood. No issue in this repo.
- **D44. Ladder anti-gaming:** body-hash at admission so the groomer cannot lower its own bar (ladder §5.2). No issue.
- **D45. The committed NATS dev token** is an accepted risk with no expiry condition (SECURITY.md:20–23). No issue.
- **D46. Approval gates** (ARCHITECTURE.md:68–72) vs ADR-0017's autonomous resolution: reconcile. No issue.
- **D47. Two decided designs living in `aspirational/`:** `llm-failure-event.md` (accepted 2026-08-05, never ADR'd) and `agent-orchestration-tools.md` §Decided spawn semantics (cited as authority by ADR-0004 and ADR-0017). Write the ADRs and move them, or say why not. No issue.
- **D48. File Phase 2–4 as issues** the way Phase 0 was (#539–#553 + #554)? 21 roadmap items untracked, plus findings A10, A11, B10, F. No issue.
- **D49. Registry decision points** D-1 (in-process `fq trigger`, likely de facto retired by the thin-client gate), D-2 (NATS `control.*`), D-5 (doctor internals). Closed plan §Decision points ahead. No issue.
- **D50. Security disclosure via public issues** during alpha expires at 1.0 (SECURITY.md:37–40). No issue.
- **D51. ADR follow-ups with no issue:** ADR-0004 delegation enforcement (escrow vs aggregate-halt) and per-origin cost breakdown; ADR-0028's five open questions; ADR-0027 step-boundary reachability. Attach to #458 or file.
- **D52. Does "Runtime ready" become a real barrier?** (a) create every durable before the hosted tasks are spawned, so the log line means "every durable exists" and #433's class (20 files) retires wholesale; (b) keep the line as-is and make each fixture wait per-durable. → #670 (filed 2026-09-10 19:36, during this review), #433.

---

## 4. Labels corrected (54 applied)

Applied directly with `gh issue edit`, watcher-managed `status:*` labels untouched. #670, filed while the review ran, was typed `bug` + `fleet:needs-decision` afterwards (D52).

- **Typed the 14 unlabelled issues.** bug: #465 #534 #535 #643 #647 #667 #669. enhancement: #468 #478 #650 #651. task: #479. tech-debt: #640. question: #469.
- **Umbrellas and epics to `task`:** #88 (was enhancement), #209 (was enhancement + fleet:needs-decision; no fleet label, it is ADR work), #512 (+task alongside documentation).
- **Design questions to `question`:** #257 (was enhancement), #469, and #389 (+question: the maintainer's comment contradicts AGENTS.md).
- **Bug/debt corrections:** #383 tech-debt → bug (the title already said so).
- **`fleet:candidate` added (23):** #123 #225 #253 #258 #345 #433 #453 #467 #473 #505 #506 #507 #534 #535 #558 #640 #643 #647 #650 #669 and #342 (flipped from needs-decision: the roadmap's Phase 3 item 5 settled the scope).
- **`fleet:needs-decision` added (22):** #114 #142 #255 #340 #344 #391 #458 #464 #465 #468 #475 #479 #517 #523 #525 #593 #651 #660 #667, and #383, #408 (flipped from candidate: the zero-price floor is a money-policy fork).
- **`fleet:needs-decision` removed (4):** #264 and #339 (decision made and executed), #410 (its sequencing question died with #78), #209 (not fleet work at any point).
- **`fleet:refined` removed:** #185. The body's path-field audit is now false (`state.directory` was added and is resolved) and every anchor predates the fq/fqd split. Re-ground, then re-stamp.

---

## 5. Backlog hygiene the review surfaced

The eight closures below were applied on 2026-09-10 (evening) with evidence comments; the rest is not applied.

### Closed with evidence (6)

| Issue | Evidence |
|---|---|
| #117 | `adapters/github-watcher/outcome.go:155-171` deliverable gate; tests `outcome_test.go:103`; commits `2f96172f`, `2d5128c4` |
| #189 | `fq-cli/src/lib.rs` is 244 lines, one module per verb; off `.file-size-baseline`; roadmap itself says "#189 can close" |
| #254 | `fq-daemon/Cargo.toml` declares `fqd`; `read_service.rs` deleted; reads ride `fq-edge` |
| #264 | `fq-cli/tests/thin_client_gate.rs` names #264 as its acceptance criterion and asserts no sqlx/fq-runtime |
| #367 | `WorkspaceProvider::reclaim` on terminal outcome; startup `prune`; build state at `/var/lib/factor-q/build`; `hygiene.sh` disk alarm |
| #390 | both files its reopened scope named are 244 lines with no test blocks, or moved to fq-ops |

**Closed as superseded (2):** #170 → #523 (remote collision fixed in `server_config.rs:44`, commit `51532ac2`; the env criterion is #523's subject). #70 → #42 (fan-out shipped at `dispatcher.rs:246`; the last criterion is #42's scope). Notes left in the close comments: #254's `--offline` fallback and #189's "byte-identical reload/drain/down" were reversed by design under ADR-0031, not delivered; `.file-size-baseline`'s header still names #189; `server_config.rs:42` mis-cites #512 where it means #170.

**One maintainer sentence closes:** #255 (accept bounded convergence), #339 (or fold into #667).

**Split (3):** #408 (pin+checksum ↔ zero-price policy), #660 (1h cache-write rate ↔ Admin API reconciliation), #413 (file the attempt ledger + intervention log as Phase 5's instrument). Also pull the smoke sandbox-denial assertion out of #199 and the tmp-reaping + denial-tracing items out of #203.

**Rescoped (24, applied 2026-09-10 evening):** every open PARTIAL issue was rewritten or annotated against 22d4aaba by four Opus 5 agents, following the repo's grooming convention (delivered items struck through under "Done (for the record)" with commit and `path:line` evidence; remaining scope renumbered; decisions in comments lifted into an "Open decisions" section). Body rewrites: #85 #187 #197 #198 #199 #203 #204 #205 #275 #276 #292 #339 #458 #464 #473 #512 #640. Umbrellas ticked with a dated status block: #75 (13/18 closed), #88 (1/4), #166 (22/37), #398 (3/13, and finding 1.4 BSL recorded as never filed), #587 (18/24 boxes). Status comments only, under in-flight work: #278 (PR #649 open), #469 (a question, not a spec).

What the rescope corrected in this morning's findings:

- #88's proposed fifth child was not unfiled: it is #373, closed 2026-07-24. The live decision is the notice format (the `<host-notice>` sentinel the body calls universal vs the JSON tool-result field #373 shipped); settle before #157.
- #473 leaks from four fixtures, not three (`edge_event_tail.rs:196` is the biggest), and the leak is 7,838 dirs / ~17 GB at HEAD, because the suites that clean up do so unconditionally and skip cleanup on panic.
- #205 item 11 (`take(8)` re-implementation) is still open at `pages.rs:318` and `pages/transcript.rs:276`; the residual batch is five items, not four.
- #278's limiter grain is not settled by PR #649: it keys on model, while most providers meter per account across models. A pre-merge call (D8).
- #464's "the reshape that would make this urgent" has landed (`2306bc16`, 2026-08-07): four `AtomRef` mint sites, all identity-keyed.
- #640 has 95 markdownlint errors across 24 files outside `docs/`, only 14 of them MD040; `ARCHITECTURE.md` alone is 33.
- The claim that the 2026-09-03 roadmap defers the agent's-seat set (#77 #85 #88 #89 #91) is not supported: those issues appear nowhere in it. They are unscheduled rather than deferred, which reframes D30.
- The roadmap document itself is stale in one place: it cites #78 as open.

**Title changes (applied 2026-09-10 evening, with a comment on each):** #587 → "ops: migrate the live dogfood instance onto the ADR-0035 compose stack" (the build-out it named is fully delivered). #640 → "ci: `just lint-docs` stops at docs/ — adapter, service, ops and root markdown are outside the gate and carry 95 errors" (the old title was factually wrong).

### Splits filed

Filed 2026-09-10 evening; parents re-pointed at the new issues, dependencies set with GitHub's blocked-by.

| New | Split from | Labels | Blocks |
|---|---|---|---|
| #675 smoke `test_shell_tool_sandbox_denial` asserts only that the run completed | #199 item 2 | bug, fleet:candidate | (none; sibling under #166) |
| #676 fq-store crash-orphaned `.tmp.*` files invisible to the audit | #203 item 1 | bug, fleet:candidate | (none; sibling under #166) |
| #677 fq-store access-control denials emit no tracing | #203 item 2 | tech-debt, fleet:candidate | (none; sibling under #166) |
| #678 `ConfigSnapshot` drops five of `Agent`'s sixteen fields | #512 item 1 (+ #204's deferred half) | tech-debt, fleet:needs-decision | #512, #204 |
| #679 JetStream retention `DEFAULT_MAX_AGE` is a compile-time constant | #512 item 2 | tech-debt, fleet:candidate | #512 |
| #680 pin the LiteLLM pricing source and verify the cache checksum | #408 (trust half) | bug, fleet:candidate | #408 |
| #681 model Anthropic's 1-hour cache-write rate | #660 item 1 | bug, fleet:candidate | #660 |

Also set: #660 blocked by #257 (its reconciliation half needs the scheduler decision) and #458 blocked by #453 (its finding 4 stays untrue until the summary subject is fixed), both stated in the rescoped bodies. The repo uses no sub-issue hierarchy anywhere, so relationships are blocked-by edges plus checklist entries on #166, matching existing practice. Not filed: #413's ladder children (pre-empts D25) and #398's Part 2 process claim (it is decision D41, not work).

Six of the seven new issues are fleet-ready today: #675 #676 #677 #679 #680 #681.

**Re-ground before dispatch (33):** #42 #72 #77 #114 #142 #158 #159 #185 #208 #209 #225 #257 #276 #340 #341 #345 #391 #392 #398 #410 #414 #415 #418 #419 #423 #424 #433 #464 #465 #469 #505 #640 and #389. Most cite `fq-cli/src/main.rs`, `docs/plans/backlog.md`, or line numbers from before the August refactor. #341 is the one an agent could close falsely.

**Ready for the fleet this week** (candidate, valid, small, anchors current or trivially fixed): #453 #643 #535 #506 #534 #558 #669 #647 #123 #159 #467 #188 #181.

**Stale active plans (4 of 5):** storage (says "nothing is built yet" after describing M1–M2 as shipped); exactly-once (async-nats now 0.50, PR-6 half-landed via #549); migration (host exists, compat read done 2026-09-07); graph-executor (exit list two-thirds closed). Phase-2-MCP needs a park line.

---

## Method

Nine Opus 5 subagents under one orchestrating session: six issue slices of about twenty issues each, every claim checked against a detached worktree at `main` @ `22d4aaba` with `path:line` evidence, following `meta/skills/backlog-grooming/SKILL.md`; and three document readers (roadmap and plans; review documents; ADRs and design docs). `meta/skills/backlog-grooming/prefilter.sh` flagged 38 issues citing paths absent at HEAD before any agent started. The follow-up passes (closures, rescopes, retitles, splits) were delegated the same way, each write citing the pinned commit. The evidence trail lives on the issues themselves — close comments, rescoped bodies, and the new issues' provenance lines — not in this document, which is the snapshot.
