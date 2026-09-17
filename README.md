# factor-q metrics data

This is an **orphan branch**: it shares no history with `main` and is never
merged into it. It holds the measurement instrument's human-entered logs
and, later, the weekly report's snapshots, so that logging a row never
triggers `main`'s CI or the hourly deploy.

- `metrics/interventions.csv` — one row per human act inside the autonomous
  loop other than the terminal accept/reject of a PR, with touch minutes.
- `metrics/accepted.csv` — one row per accepted change, tagged with its
  baseline-equivalent size (`S` `M` `L` `XL`).

Write through `just touch` / `just tag` on `main` (`scripts/metrics-log.py`),
which checks this branch out into a local worktree, appends the row, commits
and pushes. Do not edit rows by hand except to correct a typo, and say so in
the commit.

Vocabulary and rubric: `docs/guide/measurement-logs.md` on `main`, and the
plan `docs/plans/active/2026-09-17-measurement-instrument.md`. Readers:
`tools/fq-metrics` (issue #838) via `git show origin/metrics:metrics/<file>`.
