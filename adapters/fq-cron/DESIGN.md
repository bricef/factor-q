# fq-cron — design

## Status

Draft (2026-07-17). Design for a new adapter; nothing here is implemented
yet. This doc records the foundational choices — language, durability
layer, configuration and reload, delivery semantics, missed-fire policy —
and the reasoning behind each, so implementation can proceed against
settled decisions.

Related: [trigger wire contract](../../docs/design/committed/trigger-wire-contract.md)
(the boundary this adapter publishes across),
[event schema](../../docs/design/committed/event-schema.md),
[design principles](../../docs/design/committed/design-principles.md),
[github-watcher](../github-watcher/README.md) (the precedent adapter).

## What it is

A **standalone durable scheduler adapter** for factor-q. It reads a small
TOML file describing *jobs* — a cron schedule, a NATS subject, a JSON
payload — and publishes each job's payload to its subject when the
schedule fires. The subject is arbitrary: `fq.trigger.<agent>` makes it a
time-driven agent trigger (the expected main use), but any subject works,
so it also covers heartbeats, periodic sweep signals, and test traffic.

The config file is **hot-reloaded**: edit it, and the running scheduler
picks up added, removed, and changed jobs without a restart. *Durable*
means restarts and crashes neither double-fire jobs nor silently lose
track of where the schedule stood: fire state survives in a JetStream KV
bucket, and a per-job policy decides what happens to fires missed while
the scheduler was down.

## Why

The fleet currently has two trigger sources: a human running `fq trigger`,
and the event-driven github-watcher. Nothing fires on a clock. Recurring
work — nightly maintenance agents, periodic dependency or issue sweeps,
scheduled reports — needs a cron seat in the loop, and it should be an
*adapter* (outside the daemon) for the same reason github-watcher is:
the daemon stays small, and time-driven triggering couples to factor-q
only through the documented wire contracts.

## Decisions

### D1 — Language: Go

Same rationale as github-watcher, which
[made the argument first](../github-watcher/README.md#why-go-and-why-standalone):
an adapter in a different language than the runtime makes the wire-contract
boundary a *construction* rather than a convention (design principle 3
applied to integrations) — a Go binary cannot reach into `fq-runtime`'s
Rust internals, so the coupling can only be the documented contracts.
fq-cron becomes the **second consumer** of the
[trigger wire contract](../../docs/design/committed/trigger-wire-contract.md),
which strengthens it as the seed of a trigger-source SDK; patterns proven
in github-watcher (interface-seamed sources/sinks, pure decision core,
table tests against fakes) carry over directly, as does the toolchain and
the `gate-adapters` gate.

Libraries: `nats.go` (already vetted via github-watcher),
`robfig/cron/v3` (cron parsing, `@every`/`@daily` descriptors, per-schedule
timezones), `fsnotify` (config watching). All pure Go, no CGO.

### D2 — Durability layer: NATS JetStream KV

Fire state lives in a JetStream **key-value bucket** (`fq-cron-state`),
one entry per job, created idempotently by the adapter at startup. The
entry records the last *acknowledged* fire:

```json
{ "last_scheduled": "2026-07-17T02:00:00Z", "published_at": "2026-07-17T02:00:01Z" }
```

Why KV over the alternatives:

- **The broker is already a hard dependency.** fq-cron's only output is a
  NATS publish; without the broker there is nothing to do, so keeping
  state there adds no new failure mode. It does mean state is unreachable
  exactly when NATS is down — but during that window no fire can be
  published or recorded anyway, so nothing is lost that a local store
  would have saved.
- **No new infrastructure.** JetStream is already file-backed and running
  (the daemon owns the `fq-triggers` stream on it). SQLite would add a
  dependency and a host-local file; a flat state file would need
  hand-rolled atomic-write discipline. Both tie state to the host —
  under the containerised deploy, that means volume management, whereas
  KV state survives container replacement for free.
- **Bounded blast radius on loss.** If the bucket is ever wiped, the
  scheduler starts with no history: every job simply waits for its next
  scheduled fire (a job with no recorded state never catches up — see
  D6). Worst case is one skipped catch-up, not a replay storm.

Per design principle 6, the store sits behind a small `StateStore`
interface with an in-memory fake for tests; KV is the reference
implementation, swappable later without touching the scheduling core.

### D3 — Configuration: one TOML file, watched; connection via flags/env

Job definitions live in a single **TOML** file — TOML matches the
daemon's `fq.toml` convention, and a declarative jobs file is design
principle 8 in miniature: every schedule, subject, payload, and policy is
an edit-and-reload, never a rebuild.

Two configuration planes, deliberately separate:

- **Connection plane** (NATS URL, config file path, log level): flags
  with env fallbacks, exactly like github-watcher. Fixed for the life of
  the process — changing where the scheduler *connects* is a restart,
  not a reload.
- **Jobs plane** (the TOML file): everything hot-reloadable.

| Flag | Env | Default |
|---|---|---|
| `--config` | `FQCRON_CONFIG` | *(required)* |
| `--nats-url` | `FQCRON_NATS_URL` | `nats://127.0.0.1:4222` |
| `--kv-bucket` | `FQCRON_KV_BUCKET` | `fq-cron-state` |

### D4 — Hot reload: watch, validate wholesale, diff by job name

- **Detection.** `fsnotify` on the config file's *directory* (editors
  save via atomic rename, which orphans a watch on the file itself), a
  short debounce to coalesce write bursts, a low-frequency mtime poll as
  a fallback for filesystems where fsnotify is unreliable, and `SIGHUP`
  as an explicit operator override.
- **Validation is all-or-nothing.** A reload parses and validates the
  whole file; any error (bad TOML, duplicate name, invalid cron
  expression, invalid subject, unknown timezone) is logged with the
  offending job and line, and the running config **stays in force
  unchanged**. A half-applied config never exists; a broken edit can
  never stop jobs that were previously valid.
- **Job identity is `name`.** The diff against the running config is
  keyed by job name: new names are scheduled, missing names are
  cancelled (and their KV entry deleted), changed jobs are rescheduled
  in place **keeping their fire state** — so editing a schedule or
  payload does not reset the job's history, and catch-up (D6) is
  evaluated against the *new* schedule.

Names are `[a-z0-9][a-z0-9-]*`, unique, ≤ 64 chars — the name is the KV
key and part of the dedup message ID, so it is a stable identifier, not a
label.

### D5 — Delivery: JetStream publish with broker-side dedup; per-job opt-out

The durable path follows the
[trigger wire contract](../../docs/design/committed/trigger-wire-contract.md)'s
producer rules: JetStream publish, await the ack. On top of that, every
durable publish sets the standard dedup header:

```
Nats-Msg-Id: fq-cron/<job-name>@<scheduled-time RFC3339>
```

The write order is **publish, then record** — at-least-once, matching
the contract's stated semantics. The failure window (crash after the
publish ack, before the KV write) is closed by the dedup header: a
restart that re-publishes the same logical fire inside the stream's
dedup window (broker default 2 minutes; the `fq-triggers` stream is
daemon-owned, so fq-cron treats the window as given) is discarded by the
broker. Beyond the window, the recorded state plus the catch-up policy
(D6) governs, and the residual is a documented at-least-once, never
silent loss.

Per-job `durable = false` switches to a **core NATS publish** —
fire-and-forget, at-most-once — for subjects no stream captures
(heartbeats, live-observation topics). A *durable* publish to a subject
no stream matches is a configuration error, not a transient one: the
broker's "no stream matches subject" response is surfaced loudly in the
log and the job is marked unhealthy until a reload fixes it, rather than
retried to no effect.

**Transient publish failure** (broker unreachable, ack timeout) retries
with capped exponential backoff, under one rule: **at most one in-flight
fire per job**. If the job's next scheduled fire arrives while a previous
one is still retrying, the new fire supersedes it and the old one is
logged as missed — fires never queue behind each other, so an outage
produces bounded, recent traffic rather than a backlog replay.

### D6 — Missed fires: per-job `skip` or `once`, default `skip`

What happens when the scheduler was down (or superseded, per D5) across
one or more scheduled times:

- `catch_up = "skip"` (default): resume with the next future fire.
- `catch_up = "once"`: if at least one scheduled time was missed since
  `last_scheduled`, fire **exactly once** on startup/reload, stamped with
  the most recent missed slot; then resume normally.

There is deliberately no "replay every missed slot" mode. Cost is a
first-order safety concern (principle 4): a scheduler that has been down
overnight must not greet the morning by firing N agent invocations. The
cautious default is `skip`; jobs whose work is cumulative-and-idempotent
("run the nightly sweep") opt into `once`.

Two guard rails, same principle:

- A job with **no recorded state** (first appearance in config, or state
  lost) never catches up — it waits for its next scheduled time. Adding
  a job can never fire it immediately by accident.
- **Minimum interval is 1 minute**, enforced at validation. Standard
  five-field cron expressions plus `@every`/`@daily`-style descriptors;
  seconds-resolution schedules are rejected.

Timezones are per-job (`tz`, IANA name, default `"UTC"`), evaluated by
the cron library; the usual DST caveats (skipped/repeated wall-clock
times) apply to non-UTC jobs and are on the operator.

### D7 — Naming: `fq-cron`

Directory `adapters/fq-cron`, binary `fq-cron`. Descriptive-by-function
like `github-watcher`, distinctive enough to live on a `PATH`, and it
reads as part of the `fqd` / `fq` family
([draft ADR-0031](../../docs/adrs/draft/0031-daemon-cli-split.md)).

## Configuration reference

```toml
# fq-cron.toml — hot-reloaded; connection settings live on the command line.

[defaults]          # optional; each key overridable per job
tz = "UTC"
catch_up = "skip"
durable = true

[[job]]
name = "nightly-maintenance"
schedule = "0 2 * * *"                    # five-field cron, or @daily / @every 4h
subject = "fq.trigger.m0-maintenance"
catch_up = "once"
[job.payload]                             # TOML table → JSON object
task = "Run the nightly maintenance sweep. Scheduled slot: {{scheduled_time}}."
refs = []
constraints = ["Open a PR; never push to main."]
done_criteria = ["A PR is open, or a no-op run is reported."]

[[job]]
name = "ops-heartbeat"
schedule = "@every 5m"
subject = "ops.fq-cron.heartbeat"
durable = false                           # core NATS, fire-and-forget
payload_json = '{"source": "fq-cron", "slot": "{{scheduled_time}}"}'
```

Per-job fields: `name`, `schedule`, `subject` (a concrete subject — no
wildcards), `tz`, `catch_up`, `durable`, `enabled` (default `true`;
`false` pauses a job without deleting its state), and exactly one of:

- `payload` — a TOML table, serialised 1:1 to a JSON object. Jobs
  targeting `fq.trigger.<agent>` should use the
  [task-oriented payload convention](../../docs/design/committed/trigger-wire-contract.md#task-oriented-payload-convention)
  as above.
- `payload_json` — a string of raw JSON, for non-object payloads or
  exact control.
- neither — the body is empty, which the contract reads as JSON `null`.

Payloads are otherwise **opaque and static**, per the wire contract, with
exactly two template variables substituted in string values at fire time:
`{{scheduled_time}}` (the slot being fired, RFC3339, in the job's
timezone) and `{{job}}` (the job name). No further templating in v1.

## Architecture

```
fq-cron.toml ──▶ ConfigWatcher ──(validated JobSet)──▶ Scheduler core ──▶ Publisher ──▶ NATS
 (fsnotify+poll)                                          │      ▲
                                                          ▼      │
                                                        StateStore (JetStream KV)
```

The scheduler core follows github-watcher's shape: the decision logic is
a **pure planning function** —

```go
plan(now time.Time, jobs JobSet, state map[string]FireState) []Fire
```

— which owns next-fire computation, catch-up evaluation, and supersession,
against an **injected clock**. `Publisher` (JetStream / core publish) and
`StateStore` (KV) are interfaces with in-memory fakes, so every semantic
in this document is exercised in table tests with no broker and no real
time. The main loop is a thin shell: wait until the earliest next fire or
a reload event, call `plan`, execute the returned fires.

## Failure modes

| Failure | Behaviour |
|---|---|
| Broker unreachable at fire time | Backoff retries until acked or superseded by the job's next fire (D5); never a queue. |
| Crash between publish ack and state write | Restart re-publishes; broker dedup via `Nats-Msg-Id` discards inside the window; documented at-least-once beyond it. |
| Invalid config on reload | Logged with job + line; old config keeps running in full (D4). |
| Config file deleted | Treated as an invalid reload: running jobs continue; recreating the file resumes normal reloads. |
| KV bucket lost | All jobs start with no history: no catch-up, next scheduled fires only (D2, D6). |
| Durable job whose subject no stream matches | Configuration error: logged, job unhealthy until reload; not retried (D5). |
| Trigger for an unknown agent id | Not detectable by fq-cron (the contract stores it durably, undelivered); operator checks the agent id against the fleet. |
| Scheduler down across fires | Per-job catch-up policy: `skip` or one `once` fire (D6). |

## Operations

Matches the fleet's config-first deploy discipline: a schedule, payload,
or policy change is an edit to `fq-cron.toml` and an automatic reload —
no restart, no binary. A binary upgrade is a plain restart at any time:
fq-cron holds no in-flight work beyond a single pending publish, and D5/D6
define exactly what a restart can and cannot re-fire. Observability in v1
is structured logs — one line per fire attempt with job, scheduled slot,
publish outcome, stream sequence, and attempt count — plus a loud line
for every reload (accepted or rejected, with the diff).

## Testing

- **Pure core:** table tests over `plan` — cron edges (DST, month
  boundaries), catch-up × state combinations, supersession, reload
  diffing — with the injected clock; no network, no sleeping.
- **Fakes:** `Publisher` and `StateStore` in-memory doubles, as
  github-watcher does for its sources and sinks.
- **Integration:** one test against a private `nats-server` (the repo
  already pins `.nats-version` and spawns private brokers for tests):
  real JetStream publish + dedup header + KV round-trip.
- **Gate:** `gate-adapters` currently names github-watcher explicitly; it
  generalises to fan out over `adapters/*/go.mod` (gofmt, vet, test,
  build) so both adapters ride the same gate.

## Non-goals

- **Not a workflow engine.** No job dependencies, chaining, or
  conditionals — composition is the graph's job (principle 5). fq-cron
  only starts things.
- **Not distributed.** One instance; running two concurrently will
  double-fire (dedup narrows but does not close this). Leader election
  via KV compare-and-set is a plausible later swap behind `StateStore`,
  not v1.
- **No sub-minute schedules**, no missed-fire replay queues, no dynamic
  payload computation beyond the two template variables.
- **No daemon-side awareness.** The daemon neither knows nor cares that
  fq-cron exists; everything crosses the public wire contracts.

## Open questions

1. **Rate guard.** Is the 1-minute floor enough, or should a global
   `max_fires_per_hour` valve exist from day one? (Agent-side budgets
   already bound spend; this would bound trigger *count*.)
2. **Own status subject.** Should fires/misses also be published on an
   `fq.cron.>` observability subject, or are logs enough until something
   needs to consume scheduler state?
3. **Config in git.** The jobs file will likely live in the ops repo and
   land via the existing deploy flow; confirm the path and ownership
   when wiring up the dogfood instance.
