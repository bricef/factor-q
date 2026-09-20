# Measurement logs

The two human-entered inputs of the measurement instrument
([plan](../plans/active/2026-09-17-measurement-instrument.md), issue
[#840](https://github.com/bricef/factor-q/issues/840)). Everything else the
instrument reads (attempts, outcomes, corrective commits, cost) is extracted
from the event log, GitHub and git by the attempt-ledger extractor
(`just metrics-extract`, landed 2026-09-18 for
[#838](https://github.com/bricef/factor-q/issues/838) — see the
[extractor README](../../tools/fq-metrics/README.md)), which also reads the
intervention log below; these two logs exist
because interventions and touch minutes cannot be reconstructed from
history, so they are kept from the day the instrument was decided.

## Where the data lives

On the **orphan branch `metrics`**, never merged into `main`, as
`metrics/interventions.csv` and `metrics/accepted.csv` (append-only CSV with
a header row). The branch shares no history with `main`, so a logged row
never triggers `main`'s CI or the hourly deploy, and there is nothing to
merge or rebase. Read it without a checkout:

```text
git fetch origin metrics
git show origin/metrics:metrics/interventions.csv
```

Entries go through `scripts/metrics-log.py`, which keeps a worktree of the
branch at `.metrics-data/` in the repository root (gitignored), fast-forwards
it, validates the vocabulary, stamps UTC time, appends the row, commits it
with a fixed message form (`log: fix #NNN 15m`) and pushes. Do not edit rows
by hand except to correct a typo, and then in a commit that says so.

## `interventions.csv`

One row per human act inside the loop other than the terminal accept or
reject of a PR (capability ladder §2, primitive 3), with the minutes it took
(primitive 4).

```text
just touch <issue> <minutes> <type> [note...]
```

| Column | Meaning |
|---|---|
| `at` | UTC timestamp, stamped by the script |
| `issue` | the issue the attempt was for |
| `type` | `unblock`, `clarify`, `respec`, `fix`, `restart`, `retune` |
| `minutes` | touch minutes, **rounded up to the next five**, context switch included |
| `note` | free text, one line |
| `source` | `human` (default) or `agent` (a coordinating agent logging on the maintainer's behalf, `--source agent`) |

Types, in one line each; the worked examples, the rules a human holds and
how minutes are measured are in the [rubric](measurement-rubric.md):

- `unblock` — the run could not proceed without a human act: a rebase, a
  re-run of a flaky job, a dependency the agent could not install.
- `clarify` — answering a question or adding missing context after dispatch.
- `respec` — changing the acceptance criteria after dispatch; this ends the
  attempt, per the ladder's §5.2.
- `fix` — a human or a non-fleet agent changed the code the fleet produced:
  implementing review findings, closing a duplicate PR, fixing a merge break.
- `restart` — re-dispatch after a failed or abandoned run.
- `retune` — changing the agent definition, prompt or config because of this
  attempt.

Writing a refinement stamp before dispatch is touch time **before** the
attempt, not an intervention; it is not logged here.

## `accepted.csv`

One row per accepted change, tagging how long a skilled human driving a
frontier model interactively would have taken (the M0 plan's
baseline-equivalent; the throughput denominator and the trajectory plot's
cost channel).

```text
just tag <pr> <S|M|L|XL> [--issue N] [note...]
```

| Column | Meaning |
|---|---|
| `at` | UTC timestamp |
| `pr` | the merged pull request |
| `issue` | the issue it closed, if known |
| `baseline_equivalent` | `S` under an hour · `M` up to half a day · `L` up to a day · `XL` more |
| `note` | free text |
| `source` | `human` or `agent` |

Tag at merge, by the maintainer.

## Flags

`--no-push` commits to the local worktree only; `--no-commit` appends to the
file without touching git (a dry run); `--data-dir` points at a different
worktree. A push rejected because someone else appended first is rebased and
retried once; if that fails the row is committed locally and the script says
to run again.
