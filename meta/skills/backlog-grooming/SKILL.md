---
name: backlog-grooming
description: Weekly backlog grooming — sweep every open issue against main HEAD, close what's resolved (with evidence), rescope partial resolves, re-ground stale references, fix lying labels, and bring dispatch-candidates to fleet-ready. Use when asked to groom the backlog, sweep issues, check issue staleness, or make an issue fleet-ready.
---

# Backlog grooming

The tracker is a contract surface: fleet agents implement what issue bodies
say, so a stale body produces wrong work at machine speed. Grooming keeps
every open issue in one of two honest states — **accurate and actionable**,
or **closed with evidence**. Everything here is grounded the same way the
architecture diagram is: claims verified against source, never against
memory or the issue's own text.

Cadence: weekly, or after any large landing (a refactor that moves whole
subsystems staleness-bombs every issue that cites them).

## 0. Setup

Pin the ground truth so every verification cites one commit:

```sh
git fetch origin main
git worktree add .claude/worktrees/sweep origin/main --detach
gh issue list -R <owner>/<repo> --state open --limit 200 --json number,title,labels
```

Record the pinned SHA — every close comment, rescope, and re-ground note
cites it ("verified vs main @ <sha>"). Remove the worktree when done.

## 1. Sweep — classify every open issue

For scale, partition issues by subsystem and fan out read-only subagents,
each instructed to fetch the issue body (`gh issue view N --json title,body`)
and verify **every claim** against the pinned worktree. Feed each agent the
recent-landings context for its area (`git log --oneline --since=<last
groom> -- <paths>`) — resolution usually arrives as a side effect of other
work, and the agent that knows what landed finds it fast.

Four verdicts, applied skeptically:

- **RESOLVED** — the described problem no longer exists. Requires hard
  evidence: the fixing commit, or current file:line showing the new state.
  "Probably fixed by X" is not RESOLVED; go read it.
- **PARTIAL** — some sub-items landed, others stand. List exactly which,
  each with evidence. Multi-item issues must be checked item by item —
  never let one landed item resolve a checklist.
- **VALID** — problem confirmed still present; cite one confirming location.
- **VALID-STALE** — still real, but the body's premises drifted enough to
  mislead an implementer: dead line anchors, superseded version numbers,
  new code that interacts with the fix, changed failure modes.

While classifying, also check the **labels against reality**:
`status:blocked` whose blocker has landed; `status:in-progress` with
nothing on main (a dead fleet run); milestone membership that no longer
matches scope.

## 2. Act — one honest operation per verdict

### RESOLVED → close with evidence

The close comment names the fixing commits and the current code state, so
the closure is auditable without re-research. If *peripheral* acceptance
criteria remain unmet while the core is delivered: **never close over them
silently** — file a small follow-up issue holding exactly the unmet items
(with grounding refs), and link it from the close comment. Substance closes
the issue; the remainder gets a home.

### PARTIAL → rescope

Two forms, pick by how much the shape changed:

- **Body rewrite** when the remaining scope reads differently now: move
  delivered items to a `## Done (for the record)` section (struck through,
  with evidence), renumber the remaining problems, update the ACs, refresh
  every ref in what stays. Date the rescope in the opening line.
- **Status comment** when the body's own checklist already tracks progress
  correctly: a dated comment stating what landed (with refs), what the sole
  remaining scope is, and any framing that's now obsolete ("blocked on X"
  when X shipped).

### VALID-STALE → re-ground

- Refresh every code anchor. Keep the convention: cite line numbers but say
  "anchor on symbol names if lines drift".
- Update superseded facts: schema/version numbers ("the migration is now
  v9, not v8"), line counts, file paths after moves, renamed functions.
- Add **new interactions**: code that landed since filing and now
  constrains the fix (a helper that consumes the data structure being
  changed, a test harness that must be extended, a gate that reads the file
  being split). These are the traps a fleet agent hits blind.
- Check whether the failure mode itself changed — new machinery can make a
  bug's symptom *worse* or *different* (e.g. silent corruption instead of a
  loud error). Update the narrative, not just the anchors.
- For light drift, a prepended dated note mapping old→new anchors is
  enough; for heavy drift, edit inline.

### VALID → nothing

Unless a single sub-item died (a referenced idiom removed, a count already
fixed): strike it with a one-line dated comment so the next groomer doesn't
re-verify it.

## 3. Fleet-readiness — the bar for dispatch candidates

An issue about to get `status:ready` needs more than accuracy. Before
dispatching, verify:

- **No open design forks.** "Do A or B, decide and do one" is fine for a
  human, fatal for a fleet agent (it picks the cheap branch). Pre-make the
  decision, date it, and write the AC so the criterion cannot be satisfied
  by the rejected branch.
- **Named consumers and collisions.** If fixing the issue changes a
  vocabulary/format/schema, name every in-code consumer of the old form —
  the greppable constant an agent must update, not "callers may exist".
- **Explicit verification expectations.** Say what tests are possible and
  what the accepted evidence is. If a real test is impossible with current
  harnesses, say so and forbid the fake-test substitute by name.
- **Current migration/version slots.** Any schema change names the actual
  next version number as of the groom date.
- **Sequencing cautions.** Name in-flight issues touching the same
  functions; state which rebases on which.
- **Verified-safe claims stated as verified.** If the groom checked an
  assumption (e.g. "the derivation is safe for all N variants"), record
  that it was checked and when — so the agent doesn't re-derive or,
  worse, distrust it.

## 4. Report

End with a summary the maintainer can act on in one read: closed (with
what evidence), rescoped, re-grounded, labels fixed, recommend-close
judgment calls (where an AC is technically unmet but the issue has served
its purpose — the maintainer decides), and anything found mid-sweep that
deserves a *new* issue. Update milestone framing if counts changed.

## House rules learned the hard way

- Verification is per-claim, not per-issue. Issues bundle several claims;
  they rot independently.
- The sweep itself finds new work: a reproducer discovered while verifying
  one issue often belongs on a *different* issue — post it there, with the
  exact command and observed behaviour.
- Scope discipline cuts both ways: don't fix drive-by findings inside the
  groom (comment them onto the owning issue), and don't let a groom
  conclude "mostly fine" without the per-issue evidence trail.
- Line references in this repo's issues are a courtesy, symbols are the
  contract. Every refreshed body repeats that.
