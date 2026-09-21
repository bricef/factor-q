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

```text
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

Every poll also reconciles all pre-existing `in-progress` issues from durable ground truth. An open closing PR moves the issue to `in-review`; otherwise retained `fq-events` lifecycle events drive the same completed/failed retry policy. A run with neither fact stays in flight for `--reconcile-after`, then is re-queued without exceeding the historical trigger budget. This makes restarts recoverable without changing the deliberately ephemeral live subscription. Durable consumption and deploy ordering are therefore unnecessary and are not part of this change.

Either way a failed invocation is moved *off* `in-progress` rather than left
claimed with no PR and no retry.

**Merged PR → done.** Each poll also sweeps open and closed
`in-review` issues; when an issue is closed and its proposed PR has merged (via
GitHub's GraphQL `closedByPullRequestsReferences` link), it moves `in-review` →
`done`. Open issues are untouched: a reopen is authoritative.

## Advisory merge verdicts

**On by default**; `--merge-verdicts=false` / `GHW_MERGE_VERDICTS=false`
opts out. The **last** step of every poll gives each open *fleet* PR — one
whose body carries the provenance footer — one advisory verdict: a label
and one comment. **It is advisory in the strongest sense: it merges
nothing, closes nothing, and cannot fail a poll cycle.** Every error is
logged and the next PR is tried. The point is to build and calibrate the
verdict against the real PR stream before any merge action is considered
([#879](https://github.com/bricef/factor-q/issues/879)).

**The two files it reads.**

- [`.github/areas.yml`](../../.github/areas.yml) — the repository's code
  areas, the same declaration CI's path filters use.
- [`.github/merge-policy.yml`](../../.github/merge-policy.yml) — area →
  tier (`unsupervised` / `supervised` / `never`), plus `default_tier` for
  a file in no mapped area and `min_age_minutes` for the age check.

Both are read **at the PR's base ref**, never at its head: a PR that
could ship the policy judging it could bless itself. The same reasoning
makes the `merge-verdicts` area — the two files above, the rubric file,
and the watcher sources implementing the sweep — tier `never`, so a PR
can never widen its own merge rights. The loader **refuses** a policy
naming a tier it does not know or an area `areas.yml` does not declare;
version skew between the two files stops the sweep rather than quietly
relaxing it.

**The verdict.** A changed file belongs to every area whose globs match
it; a PR's tier is the *most restrictive* tier of any file it changes,
and a file in no mapped area takes `default_tier`. Adding a file can
therefore only ever make a verdict stricter. Seven **structural checks**
are computed alongside and reported in full — provenance footer present,
closes exactly one issue and that issue is in review, `mergeable_state`
clean, a single commit, no changes-requested review, no hold label, older
than the minimum age. **None of them changes the tier.** CI is not
re-implemented: branch protection stays the boundary.

**The writes.** Exactly one of `merge:unsupervised`, `merge:supervised`,
`merge:never` (the other two are removed), and one comment carrying the
head SHA, the file → area → tier table, the rule that decided, and the
check table. The comment is found again by a hidden HTML marker and
**rewritten in place** — one comment per PR, ever — and only when its text
actually changed, so a watcher restart notifies nobody. A PR nobody has
pushed to, under a policy nobody has edited, costs no API requests at all.

**The three labels must exist in the repository.** The sweep logs and
skips rather than creating them, because a label it created would carry
no colour or description a human chose:

```console
gh label create merge:unsupervised -R bricef/factor-q -c 0E8A16 -d "Advisory: area policy allows an unsupervised merge (github-watcher, #879)"
gh label create merge:supervised   -R bricef/factor-q -c FBCA04 -d "Advisory: a human reads this change before it merges (github-watcher, #879)"
gh label create merge:never        -R bricef/factor-q -c B60205 -d "Advisory: a human merges this, always (github-watcher, #879)"
```

### The second verdict: a Jev rubric

The same comment carries a second, **non-deterministic** verdict: a small
rubric of yes/no questions put to a TypeSafe System One model (Jev,
`POST https://api.typesafe.ai/v1/systemone`, model `jev-latest`), answered
as calibrated probabilities with no rationale. Because it has no
rationale it can only ever *restrict*, never construct, and it is never
the safety boundary.

- **The questions live in git**, as data:
  [`.github/merge-rubric.yml`](../../.github/merge-rubric.yml) — an `id`,
  the `instructions`, and what `criteria.yes` / `criteria.no` mean, plus
  `flag_threshold`. **No prompt text is in Go**: the client derives the
  request from the file, so changing what is asked is a reviewable change
  to one file. (The API's criteria keys are `true`/`false`; the file says
  `yes`/`no`, which is what the question actually reads as.)
- **The state** is the closing issue's title and body, the PR's title,
  body, head branch and commit count, the changed files with their areas
  and tiers, and the diff stat. **Not the diff** — the questions are
  about what the change claims and whether that matches its shape.
- **The token** comes from `TYPESAFE_API_KEY` in the environment only,
  delivered the way `GH_TOKEN` is (the dogfood stack's `env_file`), never
  written to the repo. Unset, the watcher says so once at startup and
  every comment says `rubric: not configured`.
- **Failure degrades, never propagates.** Any API failure, timeout (20 s)
  or unparseable response becomes `rubric: unavailable (reason)` in the
  comment. Verdict 1 is labelled *before* the rubric is asked anything,
  so a slow or broken scorer cannot delay or withhold it.
- **`merge:rubric-flagged`** is set when any probability reaches the
  threshold and **removed** when none does, so a push that answers the
  concern clears the flag. The label must exist in the repository, like
  the three tier labels:

  ```console
  gh label create merge:rubric-flagged -R bricef/factor-q -c D93F0B -d "Advisory: the Jev rubric flagged a question on this PR (github-watcher, #879)"
  ```

- **Scored once per push**: the sweep's cache key is the head SHA plus a
  digest of all three declaration files, so a PR is scored again only
  when someone pushes to it or edits the areas, the policy or the rubric.

### Replaying the verdict over merged PRs

A verdict nobody has measured is an opinion. `github-watcher verdict` is
the seam that lets the already-merged PRs answer for it:

```console
echo '{"number":900,"files":["docs/a.md"],"commit_count":1}' \
  | github-watcher verdict --areas .github/areas.yml --policy .github/merge-policy.yml
```

It reads **JSON Lines** on stdin — one `PRFacts` object per line — and
writes one verdict object per line of stdout, in the same order, so a
caller may batch or not without changing anything. A line that cannot be
decoded is answered with an `error` field rather than killing the batch.
It makes **no GitHub calls**: facts in, verdict out, no `--repo`, no
token, no broker. A line that omits `observed_at` is measured from the
PR's own creation, so the age check reads as zero age rather than as an
accidental pass.

The seam exists so the replay runs *this* verdict function. A replay that
reimplemented the rules would measure something that merely agrees today.

```console
just merge-verdicts-replay 2026-09-12 -o replay.csv
```

[`scripts/merge-verdicts-replay.py`](../../scripts/merge-verdicts-replay.py)
(stdlib only) reads the merged PRs with `gh api graphql`, pipes their
facts through the subcommand in one batch, and writes one CSV row per PR
— tier, the deciding rule, each check, commit count and `reworked`
(more than one commit: the ground-truth signal available today) — plus a
summary to stderr. `--rubric` adds one column per rubric question plus `rubric_flagged`,
scored through the same subcommand; it needs `TYPESAFE_API_KEY` and makes
one paid call per PR, so it is off unless asked for. It writes nothing to
GitHub. Two checks read
differently in replay and the script's docstring says so: GitHub reports
`mergeable: UNKNOWN` for a merged PR, and a merged PR's issue carries
`status:done` rather than `status:in-review`, which the script replays as
`in-review` because the review sweep is the only thing that writes `done`.

Event observation uses core NATS (at-most-once). A missed outcome is not
fatal: the durable reconciliation pass recovers the transition on a later poll,
and a re-queued issue is re-picked on the next poll.

**Schema versions.** The decoder reads envelope `schema_version` 2 and 3
(`supportedSchemaVersions` in `events.go`) and counts anything else as a
mismatch and skips it. The set is wider than what the runtime writes
because the four fields the watcher decodes — `invocation_id`,
`trigger_payload`, `task_status`, `error_kind` — are identical across both;
it must never be *narrower*, because refusing the version the runtime emits
silently disables everything above. That is what
[#694](https://github.com/bricef/factor-q/issues/694) was: a hard-coded
`!= 2` against events that had been version 3 since
[#510](https://github.com/bricef/factor-q/issues/510), so for eight days
every completion was skipped and claimed issues sat at
`status:in-progress`. `just check-schema-versions` (a phase of `just
quality`) now fails the gate if the runtime's `SUPPORTED_SCHEMA_VERSIONS`
declares a version this list omits.

## Requirements

- A GitHub token in `GH_TOKEN` (preferred) or `GITHUB_TOKEN` — GitHub access uses the API directly; `gh` is not required.
- A running `fqd` daemon, which owns the `fq-triggers` JetStream stream
  the adapter publishes to and emits the lifecycle events it observes.

## Run

```console
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
| `--reconcile-after` | `GHW_RECONCILE_AFTER` | `4h` | re-queue an eventless `in-progress` issue after twice the expected 2h maximum run |
| `--hold-label` | `GHW_HOLD_LABEL` | `hold` | a human's "not yet" on a PR; reported by the merge verdict's checks |
| `--merge-verdicts` | `GHW_MERGE_VERDICTS` | `true` | the advisory merge-verdict sweep (below); `false` opts out |
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

Every published trigger also carries two tracing headers. `Fq-Trigger-Id` is a
UUIDv7 that the runtime preserves across redelivery, and `Nats-Msg-Id` is
`github-watcher/issue-<N>@<uuid>`. Including the unique trigger UUID means
JetStream deduplicates a repeated publish of that trigger without collapsing a
legitimate later re-trigger of the same issue. The success log records the same
UUID as `trigger_id`, linking the issue to runtime invocations.

## Development

```console
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
