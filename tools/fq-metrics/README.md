# Attempt ledger extractor

`fq-metrics extract` builds or incrementally updates `attempt_ledger.sqlite`,
the objective input to factor-q's measurement instrument. The extractor uses
only Python's standard library and SQLite; GitHub access is through the `gh`
executable already used by repository automation.

From the repository root:

```console
just metrics-extract
```

Useful options can be passed after `--`:

```console
just metrics-extract -- --since 2026-09-15T00:00:00Z
just metrics-extract -- --events /path/to/event-export --output /tmp/ledger.sqlite
```

Without `--events`, the command asks the edge for the triggered-event index
with `fq events query`, then retrieves each record with `fq events get`.
Event payloads are retained for thirty days, so run the extractor at least that
often to preserve the issue join from triggered events. Older attempts retain
cost and timing from the durable index, but cannot be joined to an issue.
`--events` recursively reads JSON files from an export instead, which makes
historical extraction and offline tests deterministic. Repeating an extraction
upserts source identities (issue, invocation, PR, commit and transition key),
so rows are updated rather than duplicated. `--since` limits both the GitHub
issue window and edge index; existing rows outside the window remain intact.

## Sources and joins

* **Issue timeline:** tracked `status:*` and `fleet:*` label changes,
  close/reopen events, PR cross-references, issue creation, and admission when
  `status:ready` is first applied. GitHub's timeline does not expose the body
  as it existed at a label event, so `admission_body_hash` is `NULL` rather
  than a hash of today's potentially changed body.
* **Pull requests:** creation, merge/close, head branch, review events and
  comments, and commits after the first review. A line of the form
  `provenance: agent=<id> invocation=<uuid> model=<model>` joins a PR to an
  attempt. The line or an `m0/` head marks agent-authored work; neither means
  human-authored.
* **Event log:** every issue-bearing `triggered` event creates exactly one
  attempt keyed by invocation ID. A trigger whose payload is plain text rather than
  a GitHub reference (the doc-drift and probe agents) names no issue and is
  skipped, not counted. Its `completed` or `failed` event supplies
  terminal status, cost, call counts, and end time. Missing fields from older
  event versions remain `NULL`.
* **Git history:** mainline merge commits associate files with PRs. A commit
  during the 14 days after merge is corrective when it references the PR or
  issue, reverts the merge, or has `fix`/`revert` in its subject and touches a
  merge file. `corrective_commits.rule` records which test matched.
* **Human logs:** `metrics/interventions.csv` and `metrics/accepted.csv` are
  read from the working tree, or from `origin/metrics` when the metrics orphan
  branch is not checked out. Accepted changes carry baseline-equivalent sizes
  weighted as S=1, M=4, L=8, and XL=16 hours. Tags whose outcome has not been
  extracted yet remain in `size_tags` and are applied by a later extraction.
  Missing logs leave their tables empty.

## Acceptance rule

An outcome is accepted only after its PR was merged and completed 14 full days
without a corrective commit. Until that deadline it has
`accepted = 0`, `accepted_at = NULL`, and `rule = pending_14_days`. A correction
keeps it unaccepted and records the first correction SHA and matching rule.
After survival, `accepted_at` is exactly `merged_at + 14 days` and the rule is
`survived_14_days`. Re-extraction advances pending outcomes once time passes.

The core tables are `tasks`, `transitions`, `attempts`, `outcomes`, and
`interventions`. Internal `pull_requests` and `corrective_commits` tables retain
the source facts needed to recompute acceptance and correction LOC without
re-querying history. `views.sql` installs `first_pass_rate`,
`attempts_per_accept`, `touch_per_accept` (median minutes), `mtbi` (hours), and
`correction_ratio` (corrective human LOC / agent PR LOC); each view reports an
`all_time` and a trailing `30_days` row.

## Objective report

`fq-metrics report` reads only the extracted ledger and writes a cumulative-flow
diagram, accepted-change throughput, stage cycle-time distributions, and the
ladder and cost measures. The renderer uses only Python’s standard library.
From the repository root:

```console
just metrics-report
just metrics-report -- --ledger /path/to/attempt_ledger.sqlite --since 30 --by day --output-dir target/metrics
```

The defaults are a 90-day window, weekly buckets, `attempt_ledger.sqlite`, and
`target/metrics/`. The output directory contains `report.md`, machine-readable
`report.json`, and three SVG diagrams. Closed issues leave cumulative flow unless
they carry `status:done`; the latest applied `status:*` label wins, falling back
to the latest applied `fleet:*` label. `status:failed`, `status:blocked`, and
`fleet:needs-decision` are separate bands. Touch per accept and MTBI are reported
as `n/a (no interventions logged)` when the window contains no interventions.

## Tests

Recorded fixtures cover an issue timeline, an agent PR with provenance, a
human PR without it, exported terminal events, and synthetic git history:

```console
cd tools/fq-metrics
python3 -m unittest
```
