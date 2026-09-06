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
| `--probe` | — | asks the running scheduler's `/healthz` and exits 0 on healthy — the container's `HEALTHCHECK`; needs no config |
| `--version` | — | prints `fq-cron <commit>` and exits; needs no config |

`/healthz` answers 200 while the NATS connection is up, 503 otherwise.
The scheduler loop is not age-checked: between fires it sleeps for as
long as the schedule says, and a loop that exits on error ends the
process, which the supervisor sees directly. The bind must be loopback.

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
