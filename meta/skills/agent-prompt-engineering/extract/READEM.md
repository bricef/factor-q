# Fleet-review extraction scripts

The deterministic extraction layer behind the 2026-07 delegation-failure
analysis. Three steps, run in order from a working directory; all state is
JSON files in that directory. Logic is unchanged from the analysis run — the
numbers in `factor-q-delegation-failure-analysis.md` (v2) reproduce from
these.

Suggested home: `factor-q/meta/skills/agent-prompt-engineering/extract/`,
next to the SKILL.md — same convention as `backlog-grooming/prefilter.sh`
(deterministic plumbing co-located with the judgment doc that consumes it).

## Prerequisites

```
git clone https://github.com/bricef/factor-q.git
git -C factor-q fetch origin '+refs/pull/*/head:refs/remotes/pr/*'
```

The PR-refs fetch is what makes closed/unmerged PRs visible without the API.

Env vars (all optional): `FQ_REPO` — path to the clone (default `factor-q`);
`GH_REPO` — owner/repo for the API step (default `bricef/factor-q`);
`GH_TOKEN` — required by step 3 only; read-only is sufficient.

## Steps

1. **`analyze_prs.py`** → `pr_dataset.json`
   Enumerates every `refs/remotes/pr/*`, classifies commits by author
   identity (agent = `@agents.invalid` / `@factor-q.local`), detects
   within-PR corrections (human commits after agent commits on the branch),
   and flags post-merge correction *candidates*.

2. **`analyze_prs2.py`** → `agent_pr_report.json`
   Maps each agent PR to its landing commit on main via the unique
   per-invocation author email (UUIDv7 cohort) or subject match (older
   cohort); separates landed from unlanded; extracts corrective-commit
   details incl. `Co-Authored-By` trailers and referenced issue numbers.

3. **`enrich.py`** → `issues_all.json`, `reviews/`, `timelines/`
   Authenticated one-shot fetch: all issues+PRs with bodies/labels/states,
   review threads for every agent PR, label timelines for referenced and
   unlanded items. ~210 calls for the current repo size.

## Known limits (from the analysis)

- Squash/rebase merges defeat SHA ancestry — landing detection relies on the
  unique author emails; keep the provenance identity scheme intact or step 2
  degrades to subject matching (which missed one rename in practice: #13).
- Post-merge correction detection (step 1) is overlap+fix-message heuristic:
  candidates only, verify semantically. Within-PR detection is
  high-confidence.
- Review findings live in corrective **commit messages** and PR-close
  comments; the review-thread JSON is nearly empty by construction of the
  current workflow — don't point future judgment layers at it first.
- Step 2's "unlanded" cannot distinguish open from rejected; step 3's PR
  states resolve that.