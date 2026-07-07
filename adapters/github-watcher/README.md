# github-watcher

A **standalone external trigger adapter** for factor-q. It polls a GitHub
repository for issues labelled `ready` and, for each, triggers a factor-q
agent — so the human input becomes *"write a clear issue and label it
`ready`"* and the fleet does the rest.

## Why Go (and why standalone)

This adapter is deliberately **not** part of the `fq` CLI or `fq-runtime`.
It talks to factor-q **only through the [trigger wire
contract](../../docs/design/committed/trigger-wire-contract.md)** — a NATS
subject and a JSON payload — never through Rust code. Writing it in a
different language makes that boundary a *construction* rather than a
convention: a Go binary literally cannot reach into the runtime's
internals, so the coupling can only be the documented wire contract. It is
also the first consumer of that contract, and the seed of a trigger-source
SDK.

## What it does

On each poll, for every open issue labelled `ready`:

1. **Relabel** the issue `ready` → `in-progress`.
2. **Then** publish a trigger on `fq.trigger.<agent>`.

Relabelling *out of* `ready` before triggering is the idempotency
mechanism — a re-seen issue is no longer `ready`, so edits, re-polls, and
watcher restarts cannot double-trigger. If the publish fails after the
relabel, the claim is reverted (`in-progress` → `ready`) so the next poll
retries. A `max-per-poll` guard bounds how many issues trigger at once.

## Requirements

- `gh` on `PATH`, authenticated (e.g. via `GH_TOKEN`) — GitHub access is via
  the `gh` CLI.
- A running `fq run` daemon, which owns the `fq-triggers` JetStream stream
  the adapter publishes to.

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
| `--poll` | `GHW_POLL` | `60s` | must be ≥ 60s (rate limits) |
| `--max-per-poll` | `GHW_MAX_PER_POLL` | `3` | 0 = unbounded |
| `--task-template` | `GHW_TASK_TEMPLATE` | `Implement the fix described in GitHub issue #%d.` | `%d` = issue number |

The trigger payload is a JSON string (the rendered task template), per the
wire contract. The `<agent>` interprets it.

## Development

```
go test ./...   # pure planner + poll-loop dedup, all against in-memory fakes (no network)
go vet ./...
go build .
```

The GitHub calls (`IssueSource`) and the NATS publish (`TriggerPublisher`)
are interfaces, so the decision logic (`planTriggers`) and the poll loop
are tested without touching the network or a broker.
