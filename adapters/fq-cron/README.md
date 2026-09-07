# fq-cron

A standalone durable scheduler adapter for factor-q. It reads cron jobs from a
TOML file, publishes their payloads to NATS, and hot-reloads valid file edits
without restarting. Durable jobs use JetStream acknowledgements, broker-side
message deduplication, and JetStream KV state; `durable = false` jobs use core
NATS.

## Requirements

- A NATS server with JetStream enabled.
- For durable trigger subjects, a stream that captures the subject (normally
  the stream owned by `fqd`).

## Configuration

| Flag | Environment | Default |
|---|---|---|
| `--config` | `FQCRON_CONFIG` | required |
| `--nats-url` | `FQCRON_NATS_URL` | `nats://127.0.0.1:4222` |
| `--kv-bucket` | `FQCRON_KV_BUCKET` | `fq-cron-state` |
| `--check` | — | `false` |
| `--health-bind` | `FQCRON_HEALTH_BIND` | `127.0.0.1:9474` — loopback address of `GET /healthz`; empty disables |
| `--reload-settle` | `FQCRON_RELOAD_SETTLE` | `250ms` — quiet period a changed config file must hold before it is reloaded |
| `--removal-confirm` | `FQCRON_REMOVAL_CONFIRM` | `1m` — how long a job dropped by a reload keeps its fire state before it is deleted |
| `--probe` | — | asks the running scheduler's `/healthz` and exits 0 on healthy — the container's `HEALTHCHECK`; needs no config |
| `--version` | — | prints `fq-cron <commit>` and exits; needs no config |

`/healthz` answers 200 while the NATS connection is up, 503 otherwise.
The scheduler loop is not age-checked: between fires it sleeps for as
long as the schedule says, and a loop that exits on error ends the
process, which the supervisor sees directly. The bind must be loopback.

## What counts as a reload

A saved file replaces the running configuration only when all of this holds
([DESIGN.md D4](DESIGN.md#d4--hot-reload-watch-validate-wholesale-diff-by-job-name)):

- **It reads and parses and validates**, whole. A broken edit is logged with
  its reason and the previous configuration keeps running, unchanged.
- **It held still.** Every changed read is confirmed by a second read taken
  `--reload-settle` later, and only byte-identical reads are trusted, so a
  save that truncates then writes — an editor, `scp`, a config-management
  tool — is read once, complete. Writes inside that window are one reload of
  the finished file.
- **It declares at least one job** — or declares that it has none, in as many
  words, with a top-level `job = []`. A file with no `[[job]]` blocks and no
  `job = []` is refused: zero bytes are valid TOML with no jobs, so accepting
  it would let a read caught mid-save delete every job *and* its fire ledger
  ([#623](https://github.com/bricef/factor-q/issues/623)). To run with no
  jobs, say so:

  ```toml
  job = []
  ```

Only `job = []` stops every job and deletes their state, and even then only
after the confirmation window below. Everything else that looks empty is a file
being written, and is waited out. The two refusals an operator sees in the log
are:

```text
config reload rejected: 0 bytes declaring no jobs, and no explicit `job = []`
config changed while being read; reload deferred until it settles
```

The second is not a failure — the file is mid-save and the watcher looks
again after another settle.

**This rule governs reloads only.** `--check` and startup validate the file
and stop there: a job-less file is *valid*, so `--check` passes on it and
`fq-cron` will start with nothing scheduled. What it will never do is
replace a **running** configuration, because that is the reload that deletes
job state.

**The comparison starts from the configuration that is running.** The watcher
is seeded with the bytes `fq-cron` loaded at startup, and never reads the file
to seed itself. So an edit made while `fq-cron` was still connecting to the
broker — a wait that lasts as long as the outage does — is seen by the first
check after the scheduler starts, on the same schedule as any other edit: the
next poll tick (30 s) or an `fsnotify` event, whichever comes first. It does
not sit unnoticed until the file is written again. This is also how a startup
that read a file mid-save recovers: `fq-cron` begins with nothing scheduled,
and the writer's completed file is picked up on that first check rather than
waiting for another edit
([#634](https://github.com/bricef/factor-q/issues/634)).

## When a removed job's state is deleted

Additions and changes apply the instant a reload is accepted. So does a
removal — a job that has left the file stops firing at once — but **deleting
its fire state waits `--removal-confirm`** (default `1m`, two config poll
intervals). At the deadline `fq-cron` reads the file once more and deletes only
what is still absent; a job that came back inside the window keeps the ledger
it left with, valve history and all.

The settle above defends against a read that lands *inside* a writer's truncate
gap: a second read catches the file still moving. What it cannot see is a
writer that emits the first of two `[[job]]` blocks and then stalls for longer
than the settle — both reads return the same complete, valid, one-job file, and
nothing about it says the second job is still coming. Accepting that reload is
harmless; deleting the missing job's ledger is not, because the next complete
write re-adds it empty and the loss is silent
([#635](https://github.com/bricef/factor-q/issues/635)). A stall is as long as
the writer chooses, so removals are made slower than additions rather than the
settle made longer.

**Write the file atomically.** Every case above is a reader seeing a partial
file, and a writer that renders to a temp file in the same directory and
`rename`s it over the target never produces one: the rename is atomic, so a
reader sees the old file or the new one and never a prefix of either. `install
-m`, Ansible's `copy`/`template`, `sops -i` and most editors already do this;
`scp`, `rsync` without `--inplace` guards, shell redirection into the live path,
and a template rendered straight to it do not. The settle and the confirmation
window exist for the writers that truncate in place; they are a backstop, not a
substitute.

What an operator sees in the log, in each case:

```text
job=beta removed from the configuration: it stops firing now, and its fire state is deleted in 1m0s unless it returns
job=beta removal cancelled: back in the configuration before its deadline, fire state kept
job=beta removal confirmed after 1m0s: fire state deleted
```

A `SIGTERM` while a removal is parked deletes nothing — the scheduler cannot
tell an orderly stop from one that lands mid-save:

```text
stopping with 1 unconfirmed removal(s) [beta]: fire state kept, and reclaimed if a job of the same name returns
```

The residual is that a job removed deliberately and never re-added, in a
process that stops before the deadline, leaves one row in the KV bucket that
nothing reads. A job of the same name reclaims it; otherwise it is inert.

## Broker outages

A broker outage is waited out, never a reason to exit. The connection
retries the initial dial (so starting before the broker is up is fine,
which is what the deploy does) and reconnects without an attempt limit —
nats.go's default gives up after sixty tries two seconds apart, so two
minutes of downtime killed the process and compose restarted it into the
same outage, over and over, for as long as it lasted. Every state-store
call retries with capped backoff for the same reason, from the bucket
opened at startup onwards: the state store *is* the broker, and its first
read is the first thing each loop iteration does. Disconnects, reconnects
and a closed connection each get a log line, and `/healthz` reports 503
throughout.

A publish attempted while the connection is down fails immediately rather
than being buffered for delivery on reconnect (`ReconnectBufSize(-1)`), so
a fire is never both recorded as failed and delivered later; what is
re-sent stays the retry policy's decision.

Fires are not queued across the outage: the per-job `catch_up` policy
([DESIGN.md D6](DESIGN.md#d6--missed-fires-per-job-skip-or-once-default-skip))
decides what happens on the way back, exactly as it does after a restart.

Example `fq-cron.toml`:

```toml
[limits]
max_fires_per_hour = 120   # sliding-window ceiling on fires across every job

[defaults]
tz = "UTC"
catch_up = "skip"
durable = true

[[job]]
name = "nightly-maintenance"
schedule = "0 2 * * *"
subject = "fq.trigger.m0-maintenance"
catch_up = "once"
[job.payload]
task = "Run maintenance for {{scheduled_time}}."

[[job]]
name = "heartbeat"
schedule = "@every 5m"
subject = "ops.fq-cron.heartbeat"
durable = false
payload_json = '{"job":"{{job}}","slot":"{{scheduled_time}}"}'
```

Schedules are five-field cron expressions or descriptors such as `@daily` and
`@every 5m`; intervals must be at least one minute. See
[DESIGN.md](DESIGN.md#configuration-reference) for all job fields and delivery
semantics.

## Run and manual smoke test

Start a local JetStream broker and, for durable jobs, create a stream covering
the configured subject. Then build and run:

```sh
go build -o fq-cron .
./fq-cron --check --config fq-cron.toml
./fq-cron --config fq-cron.toml --nats-url nats://127.0.0.1:4222
```

For a smoke test, use an `@every 1m` job and subscribe to its subject with a
NATS client. Confirm it fires at the next minute boundary. While fq-cron remains
running, edit its payload or add another job; the accepted-reload log appears
and the changed configuration applies without a restart. Invalid edits are
logged and the previous configuration remains active.

## Development

From the repository root:

```sh
just install-nats
just go-ci
```

Or from this directory:

```sh
gofmt -w .
go vet ./...
FQ_TEST_NATS_SERVER=../../.tools/nats-server go test ./...
go build ./...
```

Integration tests spawn their own private broker and never use a shared
development NATS instance.
