# github-watcher

A **standalone external trigger adapter** for factor-q. It polls a GitHub
repository for issues labelled `ready` and, for each, triggers a factor-q
agent — so the human input becomes *"write a clear issue and label it
`ready`"* and the fleet does the rest. It then **observes the outcome** of
what it triggered and moves the issue's label onward, so a triggered issue
never gets stranded mid-flight.

## Why Go (and why standalone)

This adapter is deliberately **not** part of the `fq` CLI or `fq-runtime`.
It talks to factor-q **only through documented wire contracts** — the
[trigger wire contract](../../docs/design/committed/trigger-wire-contract.md)
(a NATS subject and a JSON payload) and the
[event schema](../../docs/design/committed/event-schema.md) (the lifecycle
events it observes) — never through Rust code. Writing it in a different
language makes that boundary a *construction* rather than a convention: a Go
binary literally cannot reach into the runtime's internals, so the coupling
can only be the documented wire contracts. It is also the first consumer of
those contracts, and the seed of a trigger-source SDK.

## What it does

The watcher drives an issue through a label state machine:

```
ready ──trigger──▶ in-progress ──completed──▶ in-review ──PR merged──▶ done
                        │
                        ├──failed (transient, retries left)──▶ ready   (bounded retry)
                        └──failed (terminal / retries exhausted)──▶ failed
```

**On each poll**, for every open issue labelled `ready`:

1. **Relabel** the issue `ready` → `in-progress`.
2. **Then** publish a trigger on `fq.trigger.<agent>`.

Relabelling *out of* `ready` before triggering is the idempotency
mechanism — a re-seen issue is no longer `ready`, so edits, re-polls, and
watcher restarts cannot double-trigger. If the publish fails after the
relabel, the claim is reverted (`in-progress` → `ready`) so the next poll
retries. A `max-per-poll` guard bounds how many issues trigger at once.

**Observing the outcome** (closes the gap that stranded issue #9). The
watcher subscribes to the triggered agent's lifecycle events
(`fq.agent.<agent>.triggered` / `.completed` / `.failed`), binds each
invocation to its issue via the `triggered` event's payload, and reacts:

- **completed** → `in-progress` → `in-review` (the agent opened its PR);
- **failed, transient** (e.g. `llm_error`) → `in-progress` → `ready`, up to
  `--max-retries` times — a **bounded** auto-retry, not infinite;
- **failed, terminal** (`budget_exceeded`, `max_iterations`,
  `sandbox_violation`) or retries exhausted → `in-progress` → `failed` for
  operator attention.

Either way a failed invocation is moved *off* `in-progress` rather than left
claimed with no PR and no retry.

**Merged PR → done.** Each poll also sweeps `in-review` issues; when an
issue's proposed PR has merged (via the GitHub GraphQL
`closedByPullRequestsReferences` link), it moves `in-review` → `done`.

Event observation uses core NATS (at-most-once). A missed outcome is not
fatal: the review sweep is the backstop, and a re-queued issue is re-picked
on the next poll.

## Requirements

- `gh` on `PATH`, authenticated (e.g. via `GH_TOKEN`) — GitHub access is via
  the `gh` CLI.
- A running `fq run` daemon, which owns the `fq-triggers` JetStream stream
  the adapter publishes to and emits the lifecycle events it observes.

## Run

```
github-watcher --repo bricef/factor-q --agent m0-issue-fix \
  --nats-url nats://127.0.0.1:4223 --poll 60s
```

## Configuration

Every flag has an environment-variable fallback.

| Flag | Env | Default | Notes |
|---|---|---|---|
| `--repo` | `GHW_REPO` | *(required)* | `owner/name` |
| `--agent` | `GHW_AGENT` | `m0-issue-fix` | target agent id |
| `--nats-url` | `GHW_NATS_URL` | `nats://127.0.0.1:4222` | the daemon's NATS |
| `--ready-label` | `GHW_READY_LABEL` | `ready` | the label that triggers |
| `--in-progress-label` | `GHW_IN_PROGRESS_LABEL` | `in-progress` | applied on trigger |
| `--in-review-label` | `GHW_IN_REVIEW_LABEL` | `in-review` | applied when the agent completes (PR open) |
| `--failed-label` | `GHW_FAILED_LABEL` | `failed` | applied when retries are exhausted / terminal failure |
| `--done-label` | `GHW_DONE_LABEL` | `done` | applied when the proposed PR merges |
| `--poll` | `GHW_POLL` | `60s` | must be ≥ 60s (rate limits) |
| `--max-per-poll` | `GHW_MAX_PER_POLL` | `3` | 0 = unbounded |
| `--max-retries` | `GHW_MAX_RETRIES` | `2` | bounded auto-retry budget per issue for transient failures |
| `--task-template` | `GHW_TASK_TEMPLATE` | `Implement the fix described in GitHub issue #%d.` | `%d` = issue number |

The trigger payload is a JSON string (the rendered task template), per the
wire contract. The `<agent>` interprets it. The same template is used in
reverse to recover the issue number from the `triggered` event, so the
watcher can bind an outcome back to its issue — keep it stable across a
watcher restart while invocations are in flight.

## Development

```
go test ./...   # pure planner, poll-loop dedup, outcome reactor, review sweep — all against in-memory fakes (no network)
go vet ./...
go build .
```

The GitHub calls (`IssueSource` / `ReviewSource`), the NATS publish
(`TriggerPublisher`), and the event stream (`OutcomeSource`) are all
interfaces, so the decision logic (`planTriggers`, `OutcomeReactor`, the
review sweep) is tested without touching the network or a broker.
