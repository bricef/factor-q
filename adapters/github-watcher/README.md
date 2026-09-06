# github-watcher

A **standalone external trigger adapter** for factor-q. It polls a GitHub
repository for issues labelled `status:ready` and, for each, triggers a factor-q
agent — so the human input becomes *"write a clear issue and label it
`ready`"* and the fleet does the rest. It then **observes the outcome** of
what it triggered and moves the issue's label onward, so a triggered issue
never gets stranded mid-flight.

## Why Go (and why standalone)

This adapter is deliberately **not** part of the `fq` CLI or `fq-runtime`.
It talks to factor-q **only through documented wire contracts** — the
[trigger wire contract](../../docs/design/committed/trigger-wire-contract.md)
(a NATS subject and a JSON payload, including its task-oriented convention)
and the
[event schema](../../docs/design/committed/event-schema.md) (the lifecycle
events it observes) — never through Rust code. Writing it in a different
language makes that boundary a *construction* rather than a convention: a Go
binary literally cannot reach into the runtime's internals, so the coupling
can only be the documented wire contracts. It is also the first consumer of
those contracts, and the seed of a trigger-source SDK.

## What it does

The watcher drives an issue through a label state machine:

```
ready ──trigger──▶ in-progress ──completed (task success/partial)──▶ in-review ──PR merged──▶ done
                        │
                        ├──completed (task failed/blocked, retries left)──▶ ready   (bounded retry)
                        ├──completed (task failed/blocked, exhausted)──▶ failed
                        ├──failed (transient, retries left)──▶ ready   (bounded retry)
                        └──failed (terminal / retries exhausted)──▶ failed
```

**On each poll**, for every open issue labelled `status:ready`:

1. **Claim** the issue: add `in-progress`, **then** remove `ready`.
2. **Then** publish a trigger on `fq.trigger.<agent>`.

Relabelling *out of* `ready` before triggering is the idempotency
mechanism — a re-seen issue is no longer `ready`, so edits, re-polls, and
watcher restarts cannot double-trigger. If the publish fails after the
relabel, the claim is reverted (`in-progress` → `ready`) so the next poll
retries. A `max-per-poll` guard bounds how many issues trigger at once.

**Add before remove**, in that order, for two reasons. An interrupted
claim then leaves the issue carrying *both* labels, which the planner
skips and the ready list still shows — removing first leaves a window
where the issue has no status label at all, and a failure there strands it
where no list will ever surface it again. And it arbitrates between two
watchers: both may add `in-progress`, but only one removal can find
`ready` there, so the loser gets a 404, reads it as a lost claim, and does
not publish a second trigger for the same issue. Two removals racing
inside GitHub could still both report success; one watcher per repo is the
definitive fix ([#553](https://github.com/bricef/factor-q/issues/553)).

A removal that fails with anything *other* than a 404 — a 5xx, a timeout —
is a failed transition rather than a lost race, and the add is rolled back
before it is reported, so the issue is left where it started and the next
poll tries again. An issue left carrying both labels would be skipped for
ever. If that rollback fails too, the issue really is stuck: the watcher
says so, names both labels, and asks for a hand repair instead of
promising a retry.

**Observing the outcome** (closes the gap that stranded issue #9). The
watcher subscribes to the triggered agent's lifecycle events
(`fq.agent.<agent>.triggered` / `.completed` / `.failed`), binds each
invocation to its issue via the `triggered` event's payload, and reacts:

- **completed** → routed by the agent's declared `task_status` (#125):
  `success`/`partial`/absent → `in-progress` → `in-review` only after verifying an open PR closes the issue (otherwise bounded retry); `failed`/`blocked`
  → the same bounded retry as a transient runtime failure (re-queue to
  `ready`, then `failed` when exhausted) — an honest "done but failed"
  never lands in review. On the in-review path,
  The watcher then stamps a **provenance footer** on the open PR closing
  the issue — agent id, invocation id, trigger issue, completion time
  (issue #162) — so any PR traces back to the exact invocation that
  produced it. The stamp is idempotent (an HTML marker guards against
  re-stamping) and best-effort: a failure is logged and never blocks the
  label transition;
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

- A GitHub token in `GH_TOKEN` (preferred) or `GITHUB_TOKEN` — GitHub access uses the API directly; `gh` is not required.
- A running `fqd` daemon, which owns the `fq-triggers` JetStream stream
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
| `--ready-label` | `GHW_READY_LABEL` | `status:ready` | the label that triggers |
| `--in-progress-label` | `GHW_IN_PROGRESS_LABEL` | `status:in-progress` | applied on trigger |
| `--in-review-label` | `GHW_IN_REVIEW_LABEL` | `status:in-review` | applied when the agent completes (PR open) |
| `--failed-label` | `GHW_FAILED_LABEL` | `status:failed` | applied when retries are exhausted / terminal failure |
| `--done-label` | `GHW_DONE_LABEL` | `status:done` | applied when the proposed PR merges |
| `--poll` | `GHW_POLL` | `60s` | must be ≥ 60s (rate limits) |
| `--max-per-poll` | `GHW_MAX_PER_POLL` | `3` | 0 = unbounded |
| `--max-retries` | `GHW_MAX_RETRIES` | `2` | bounded auto-retry budget per issue for transient failures |
| `--task-template` | `GHW_TASK_TEMPLATE` | `Implement the fix described in GitHub issue #%d.` | `%d` = issue number |
| `--health-bind` | `GHW_HEALTH_BIND` | `127.0.0.1:9473` | loopback address of `GET /healthz`; empty disables |
| `--probe` | — | | ask the running watcher's `/healthz` and exit 0 on healthy — the container's `HEALTHCHECK`; needs no `--repo` |
| `--version` | — | | print `github-watcher <commit>` and exit |

`/healthz` answers 200 while the NATS connection is up and the poll loop
has completed a cycle within three intervals, 503 otherwise, with a small
JSON body saying which. It deliberately says nothing about GitHub: an
outage there is logged per cycle and is not this process's fault. The
bind must be loopback — the endpoint is for the supervisor on the same
host or in the same container, not for the network.

## Broker outages

A broker outage is waited out, never a reason to exit, and never a reason
to move a label. The connection retries the initial dial (so starting
before the broker is up is fine, which is what the deploy does) and
reconnects without an attempt limit — nats.go's default gives up after
sixty tries two seconds apart, which left the watcher polling GitHub for
ever with a dead connection: claiming issues it could not trigger,
reverting them, and repeating every cycle, with the outcome subscriptions
that would have rescued them gone too.

The connection is checked once per cycle, so a disconnect *mid*-cycle
still reaches the publish. That publish fails immediately rather than
being buffered for delivery on reconnect (`ReconnectBufSize(-1)`): a
buffered trigger would be reverted as failed and then delivered anyway,
so the next cycle would claim the issue and trigger it a second time.

While the connection is down each poll cycle is skipped whole — no claim,
no trigger, no review sweep — with a log line saying so, and `/healthz`
reports 503. Nothing is lost: a `ready` issue is still `ready` when the
broker returns.

The trigger payload follows the [task-oriented payload convention](../../docs/design/committed/trigger-wire-contract.md#task-oriented-payload-convention):
it includes `task`, `refs`, `constraints`, and `done_criteria`, plus a `github`
object with the repository and issue number. The `<agent>` interprets it. The
`github.issue` field binds an outcome back to its issue; the watcher also accepts
the former task-string payload while in-flight legacy invocations finish.

## Development

```
go test ./...   # pure planner, poll-loop dedup, outcome reactor, review sweep — all against in-memory fakes (no network)
go vet ./...
go build .
```

The reconnect policy is the exception: its tests start and stop a private
broker, so they need the pinned `nats-server` the repository gate installs
(`just install-nats`, then `FQ_TEST_NATS_SERVER=../../.tools/nats-server
go test ./...`). Without it they skip rather than fail.

The GitHub calls (`IssueSource` / `ReviewSource`), the NATS publish
(`TriggerPublisher`), and the event stream (`OutcomeSource`) are all
interfaces, so the decision logic (`planTriggers`, `OutcomeReactor`, the
review sweep) is tested without touching the network or a broker.
