# Operating the `fqd` daemon

Practical guide to the lifecycle of a long-lived `fqd` daemon: how to
**stop** it, how to **redeploy** it, and how to **hot-reload** agent
definitions — without reaching for a raw signal.

factor-q's runtime is a durable-execution engine: every in-flight
invocation's state is on the WAL, so stopping and restarting is a
*controlled* crash-and-recover, not data loss (ADR-0027). The commands
below drive that machinery cleanly and confirm what they did.

## Pairing a client: where the credentials are

Every `fq` verb answers over the daemon's authenticated edge, so a
client has to be paired with the daemon before any of them work. The
daemon provisions its identity on its first start and leaves two files
beside it under the **state** directory — `$XDG_STATE_HOME/factor-q`
or `~/.local/state/factor-q` by default; `FQ_STATE_DIR` or
`[state] directory` in `fqd.toml` move it:

| File | What | Mode |
| --- | --- | --- |
| `<state>/edge/admin.token` | the all-authority admin token, written once and **never printed** | 0600 |
| `<state>/edge/fingerprint` | the certificate's SHA-256, lowercase hex — the pin | public |

The token stays off stdout on purpose
([#545](https://github.com/bricef/factor-q/issues/545)): a token in
the daemon's output is a token in journald, `docker logs` and every
run log for the life of the file. The daemon prints the *path*, and
the fingerprint (`edge: certificate fingerprint`), so an operator at a
terminal can compare it.

Pair from a script — or from anywhere without a terminal — by naming
both:

```sh
fq connect 127.0.0.1:9472 \
  --token "$(cat ~/.local/state/factor-q/edge/admin.token)" \
  --fingerprint "$(cat ~/.local/state/factor-q/edge/fingerprint)"
```

Without `--fingerprint`, `fq connect` shows the fingerprint the daemon
presents and asks you to confirm it — trust-on-first-use — and only
from a terminal. With stdin redirected it refuses before dialling
([#544](https://github.com/bricef/factor-q/issues/544)): pinning
whatever the network presents and then sending the token to it is
exactly what the pin exists to prevent, and the daemon may bind
non-loopback. The pairing lands in
`$XDG_CONFIG_HOME/factor-q/connections.toml` (0600), once per daemon.
Narrower tokens — the dashboard's, a CI job's — are attenuated from the
admin token offline with `fq token attenuate`, never by handing the
admin token out.

Rotating the identity is deleting `<state>/edge/` and restarting: every
pinned client and every issued token, the dashboard's included, is
invalidated, and the next start mints and writes a fresh set.

## Stopping the daemon: `fq down`

`fq down` is the operator-facing **stop** verb. Do **not** stop the
daemon with `pkill -INT -x fq`: that matches by process name (it can
catch a stray `fq status` or the wrong instance, or miss the one you
meant), gives no confirmation the process actually exited, and does not
drain in-flight work.

```sh
# Clean stop: drain in-flight invocations to the next step boundary
# (bounded by drain_deadline_ms), tear down infrastructure, deregister
# the worker, exit — then confirm the process stopped.
fq down

# Stop immediately without draining: clean teardown + deregister + exit
# now, accepting that in-flight invocations become recoverable on the
# next start. The proper replacement for `pkill -INT`.
#
# Against a daemon that is ALREADY draining, this escalates that drain:
# the wait ends at once and the same clean teardown runs. That is what
# to reach for when a stop is taking longer than you can wait.
fq down --now        # alias: --no-drain
```

`fq down` invokes the `control.down` command on the daemon's
authenticated edge and then **waits — bounded — for that edge to stop
answering** before returning, so a zero exit means the daemon actually
stopped (and, in a normal stop, deregistered its worker so it is not
left `alive` to age into `stale`). A timeout is reported as an error
pointing you at `fq status` / `fq workers list`, rather than a false
"stopped".

> It needs a pairing, like every other verb that asks the daemon
> something (`fq connect`). Confirmation is the daemon's edge going
> away — the process itself, not a message it sent on the way out, which
> is what the retired `fq.system.shutdown` wait observed. There is still
> no PID/supervisor registry (a supervised `fq up` story is future work),
> so `fq down` confirms *the daemon it was paired with is no longer
> serving*, not an OS-level process check. With no daemon to reach, it
> fails at once rather than waiting out the drain deadline.

Ctrl-C (SIGINT) in the daemon's own terminal remains a fast clean stop
for interactive use; SIGTERM (what `docker stop` / systemd send) runs a
graceful drain (ADR-0027). `fq down` gives you the same clean paths as a
scriptable, confirmable command from anywhere that can reach the
daemon's edge.

The confirmation reads `requested mode=drain` rather than `mode=drain`,
and the wording is deliberate: a drain can be escalated while `fq down`
is still waiting — by a second SIGTERM on the daemon's host, or by
another operator's `fq down --now` — and the client cannot see that,
because the only channel it has is an edge that stops answering. What
actually ran is in the daemon's `system.shutdown` event.

### What the `system.shutdown` reason says

Every stop the daemon reaches on its own feet publishes one, and the
reason is the whole vocabulary:

| reason | what happened |
|---|---|
| `ctrl_c` | SIGINT — the interactive fast stop |
| `sigterm` | a drain that ran to completion, or to its deadline |
| `down` | `fq down`, likewise |
| `down_now` | `fq down --now` against a daemon that was not draining |
| `sigterm_escalated_by_…` | a SIGTERM drain cut short — the suffix is `sigterm`, `ctrl_c` or `down_now`, naming what escalated it |
| `down_escalated_by_…` | an `fq down` drain cut short, same three suffixes |
| `task_failed` | a supervised task died and took the runtime with it — a non-zero exit, and the worker row is deliberately left for the stale sweep |
| `signal_error` | no signal handler could be installed; also a non-zero exit |

The `clean` flag beside the reason is the short version: false for
`task_failed` and `signal_error`, true for everything above them. A
daemon killed with SIGKILL publishes nothing at all, which is the point
of never reaching for it.

### Cutting a drain short

A drain is bounded by `drain_deadline_ms` (default 120s), but you do
not have to wait it out. **A second SIGTERM escalates it**, and so does
`fq down --now`:

```sh
# The daemon is draining and you need it stopped now.
fq down --now                    # over the edge, from anywhere

kill -TERM <pid>                 # or a second signal, on its own host
docker kill -s TERM fqd          # under compose, where `stop` already sent the first
```

Escalating is **not** a hard stop. The drain wait ends, but the teardown
that follows it does not change: infrastructure tasks are joined, the
worker is deregistered, and `system.shutdown` is published. In-flight
invocations that had not yet reached a step boundary are resumed by
recovery on the next start, exactly as if the deadline had elapsed.

`SIGKILL` is the only thing that costs those guarantees — it leaves the
worker row `alive` to age into `stale` and publishes nothing — so it is
never the way to hurry a stop along.

## How long a stop takes

Worth knowing before setting an orchestrator's grace period. A stop is
bounded by:

- the drain wait, up to `drain_deadline_ms` (only for `fq down` and
  SIGTERM; Ctrl-C and `--now` skip it, and a second signal ends it
  early); plus
- up to five seconds for the infrastructure tasks, which are stopped
  together and joined concurrently *after* the drain — they have
  nothing to suspend, so they never eat into the drain's deadline; plus
- the MCP children's shutdown and two best-effort control-plane writes.

The worker keeps heartbeating for the whole drain, so a daemon that is
still executing steps is still `alive` on the roster rather than
looking stale to the sweep.

## Redeploying with `fq down`

For a **redeploy** — swap the binary and resume in-flight work under the
new one — stop cleanly, deploy, then relaunch:

```sh
fq down     # drain to a step boundary, confirm shutdown, and exit
# ... deploy the new binary ...
fqd      # recovery resumes suspended invocations without lost/re-run work
```

The default mode is the suspend-for-handoff mechanism specified by ADR-0027.
The same command also serves when switching the daemon off; intent is the only
difference. Use `--now` only when the drain must be skipped — or to escalate
one that is taking too long.

> **Give the old daemon its port back before starting the new one.** The
> daemon's bound edge listener is its single-instance lock: it is taken before
> anything else happens, so a second `fqd` on the same `[edge] bind` exits at
> once, naming the address, having registered no worker and published nothing.
> That is the desired outcome — two daemons on one state directory is the
> failure it prevents — but during a redeploy it means waiting for `fq down` to
> confirm before relaunching, which is what `fq down`'s bounded wait is for.

A second thing follows from the same ordering, and it looks like a hang:

> **A booting daemon accepts connections it cannot answer yet.** The socket is
> bound at the very start of startup and only begins serving once the runtime
> behind it is up, so between those two moments the kernel completes handshakes
> into the backlog and nobody reads them. A `fq` verb aimed at a daemon that is
> still booting therefore **hangs until the client's own timeout** rather than
> failing fast with a connection refusal — and a boot can be slow: a store
> migration, a recovery scan, an unresponsive MCP server. This is the price of
> the bind being the instance lock, and it is the right trade (the alternative
> leaves a window where a second daemon can start), but it is worth knowing
> before concluding a daemon is wedged. The daemon's own log says which step it
> is on; a stop signal is answered throughout, so a boot you have given up on
> can be stopped with `docker stop` or a plain `kill -TERM`.

## Hot-reloading agents: `fq reload`

To pick up edited agent definitions without a restart:

```sh
fq reload    # daemon re-reads the agents directory for the NEXT trigger
```

In-flight invocations keep the config they snapshotted at trigger time
(ADR-0020); the reload affects the next trigger only.

## Rebuilding the projection: `fq projection rebuild`

The projection (`projection.db`) is derived from the event stream, and
it can be re-derived on demand:

```sh
fq projection rebuild --yes                      # drop, recreate, replay
fq projection rebuild --yes --reason "backfill"  # with a note for `fq status`
```

The daemon stops its projection consumer, drops the projection tables,
recreates them at its schema version with every row carried across,
resets the `fq-projector` durable so the stream replays from its
**replay floor** — the first event whose envelope version this build
reads — and starts the consumer again. Rows at or above the floor are
dropped and re-derived whole from the stream, which is what fills in a
column that was NULL for history; rows below it stay as they were, and
so do cost-bearing event rows, invocation summaries and trigger records
wherever they sit — no spend figure is lost. `--yes` is required:
without it the command explains and stops.

The command answers when the consumer is running again, not when the
replay has finished. Until it catches up, reads answer over a partial
fold — `fq status`'s `projection rows` climbs back, and a spend figure
inside the stream's window can be short. `fq status` reports the
rebuild for as long as the file lasts:

```text
  projection rebuild: in progress (replaying from sequence 58000 to 60744; 12 older rows carried as-is) — started 2026-09-07T10:00:00+00:00, operator request: backfill
```

and `complete (replayed from sequence 58000; 12 older rows carried
as-is)` once the durable's acked position has reached the target.

**In the weeks after an envelope bump** the stream still holds events
in the version the previous build wrote, until they age out of
retention. A rebuild in that window replays only the part this build
reads — everything from the floor on — and carries the rest as-is: the
rows an older build projected from that history keep the shape they
had, and the line's "older rows carried as-is" count is how many. The
older events do not halt the projector; a halt during a replay (see
[below](#when-a-consumer-halts-on-an-event-it-cannot-read)) means an
unreadable event *above* the floor, which is a genuinely mixed stream.
A stream that holds nothing this build reads floors past its end, and
the line reads `complete (nothing to replay: …)` at once.

The same rebuild happens by itself when a new build's projection
schema version is higher than the file's: the daemon rebuilds on start
and the replay follows. Do **not** delete `projection.db` to force one
— that loses the cost rows older than stream retention, which exist
nowhere else; the verb keeps them.

## Event-stream retention

The payload-bearing `fq-events` JetStream stream is retained for 30 days by
default. Set a different positive duration in `fqd.toml` when disk pressure
requires a shorter trail or incident response requires a longer look-back:

```toml
[events]
max_age = "30d" # also accepts hours, minutes, and seconds, such as "12h"
```

The daemon creates new streams with this value and updates an existing stream
on restart. Zero, negative, malformed, and unitless values are rejected while
loading configuration, before the daemon connects to the broker. This setting
does not change the separate trigger, maintenance-command, or advisory stream
windows, nor `[state] retention_days` for SQLite rows.

## Stale workers: nothing to do

A worker that stops heartbeating for ~30s is marked `stale`. You will
see these after any crash or `pkill`, and after an unclean stop that
skipped the deregistration `fq down` does for you:

```sh
fq workers list --stale-only    # what died, and when it was last seen
```

**This is a diagnostic, not a queue of work.** There is no verb to
clear it, deliberately: each daemon run registers under a fresh id, so
the roster would grow by a row per restart if reclaiming it were
something you had to remember. The daemon collects stale registrations
itself, on the same scheduled sweep that bounds the invocation archive.

Two knobs, both under `[state]` in `fqd.toml`:

| Key | Default | Meaning |
| --- | --- | --- |
| `stale_worker_retention_days` | `7` | how long a stale registration is kept before the sweep deletes it; `-1` disables collection |
| `sweep_interval_seconds` | `3600` | how often the sweep runs |

`stale_worker_retention_days` is **not** the staleness threshold — that
is a fixed ~30s and is what `--stale-only` filters on. This is how long
the evidence sticks around afterwards, which is why it is measured in
days: the default leaves a Friday-night failure still on the roster on
Monday morning.

A stale worker that still owns unresolved (`in_flight` or `ambiguous`)
invocations is **never** collected, however old it is — its row is what
holds that work findable. If you see one persisting well past the
window, the daemon is logging a warning about it, and the real problem
is the unrecovered invocation rather than the leftover row.

## Triaging unresolved invocations

There is no `fq recovery` verb. (The daemon used to name one in a
couple of its warnings; those strings were corrected on 2026-08-27 and
now name the verbs below, with a test gate holding every `fq …`
reference in the source against the real CLI.) Triage is three verbs
under `fq invocation`:

```sh
# What is unresolved, and since when
fq invocation list --status=ambiguous
fq invocation list --status=in_flight

# The detail on one: owner row, archive row, recent events
fq invocation show <invocation-id>
```

`ambiguous` is the state that needs a decision: the daemon knows a tool
dispatch was issued but not whether it took effect, so it will not replay
on its own. Two verbs settle it, and which one you want depends on
whether the work so far is worth keeping:

```sh
# Keep the progress: durably close every stuck dispatch with an honest
# interrupted result, then re-drive normal SafeReplay recovery
fq invocation resume <invocation-id>

# Abandon it: an operator-issued terminal transition, published as
# `invocation.operator_recovered` so the audit trail distinguishes it
# from a worker-initiated ending
fq invocation drop <invocation-id>
```

`resume` refuses an invocation this daemon is actively driving; so does
`drop`, unless you pass `--live`, which halts it at its next step
boundary first. `fq doctor` composes the same signals into one report if
you would rather start from a summary.

## When a provider hangs or throttles

Every model call has a deadline — `[worker] llm_timeout_secs` (default
600) for the whole call, `llm_connect_timeout_secs` (default 10) for
the connection — so a provider that accepts the connection and never
answers cannot park an invocation, or at `max_concurrent_invocations =
1` the daemon, until someone restarts it. A call past the deadline
fails as a transient `timeout`; the retry policy under
`[worker.llm_retry]` tries it again under its own cap,
`timeout_max_attempts` (default 2, separate from the general
`max_attempts` because every attempt at a hang costs the whole budget),
so a provider that never answers holds a worker for at most twice the
budget — twenty minutes at the defaults — before the invocation fails
with `llm_error`. Every one of these numbers lives in `fqd.toml`, with
the reasoning behind the defaults.

A 429 is retried after the wait the provider names — `Retry-After`
where one is sent, else the `RetryInfo.retryDelay` Google writes into
the body — up to `max_retry_after_ms` (default 120 s); one that names
no wait is retried on the runtime's own backoff. Any other 4xx is the
request being refused and is not retried at all. The `llm.failure`
event's `error_kind` says which of these happened — `timeout`,
`rate_limited`, `rejected` or `request_failed`.

A 429 also does three things fleet-wide, through the per-model provider
throttle ([#278](https://github.com/bricef/factor-q/issues/278); design
in
[provider-throttle-and-deferral.md](../design/committed/provider-throttle-and-deferral.md)):

- **It pauses the model.** For the wait the provider named
  (`Retry-After`, or Google's `RetryInfo.retryDelay`), or an escalating
  default when it named none (`[worker.throttle] default_pause_ms`,
  30 s, doubling per consecutive 429 wave up to `max_retry_after_ms`,
  reset by a success). Every call on that model
  waits for the pause to end; a trigger for an agent on that model is
  *held* by the dispatcher — pulled, un-acked, kept alive — and started
  when the pause lifts, still as its first delivery. Nothing is
  redelivered and no trigger retry is consumed.
- **It halves the model's in-flight cap.** The cap is the most calls the
  model may have open at once, halved per 429 wave (floor 1) and raised
  by one after `success_window` (10) consecutive clean calls, up to
  `max_concurrent_invocations`. A fleet of four told no once runs the
  model two at a time until it has earned the permits back.
- **It defers, never fails, an invocation the retry layer gave up on.** A
  provider asking for longer than `max_retry_after_ms`, or a short ask
  the attempts ran out on, puts the invocation down at its step
  boundary: `invocation.deferred` is published, the WAL row stays in
  flight under `phase = "deferred"`, no `failed` follows, and the
  dispatcher resumes it after the wait — at least what the provider
  asked for and at least the model's escalating default. A daemon that
  stops first resumes it at startup like any in-flight row. The github
  watcher's retry count never moves.

`fq status` and `fq doctor` list the throttled models — the pause's end,
the permits against the ceiling, the 429s in the current window — and
the dashboard's health page shows the same line. It is not a doctor
issue: a throttled model is the runtime absorbing a provider's
backpressure, which is its job. `fq events query --event-type
invocation_deferred` lists the invocations put down.
`[worker.throttle] enabled = false` makes the throttle inert (every
permit granted at once, no pause, no hold); the deferral is not part of
the throttle and stays on.

An agent's own `max_concurrent`
([#718](https://github.com/bricef/factor-q/issues/718),
[agent definitions](agent-definitions.md#iteration-cap-concurrency-cap-and-reasoning-effort))
holds a trigger the same way, for the same reason and by literally the
same mechanism — one hold, one keepalive cadence, in the daemon's code
as well as in this description: an agent already running as many
invocations as its definition allows has its next trigger pulled,
un-acked, kept alive and started when a slot frees, still as its first
delivery. The two bounds are independent — the throttle protects the
provider, the per-agent cap protects the host — and a trigger can wait
on either. `fq doctor` lists the agents at their cap beside the
throttled models, and neither is a doctor issue.

**An invocation the throttle put down still counts against its agent's
cap.** A deferred invocation is sleeping, not finished: its WAL row is
still in flight, `fq doctor` still counts it, and it keeps the slot it
was admitted under until it ends for real — its resume runs on that
slot rather than taking a new one. So a persistently throttled model
cannot turn a capped agent's queue into a burst: whatever the pause
does to the timing, no more than `max_concurrent` of that agent's
invocations are ever in flight at once.

**A deploy during a cap hold costs the trigger nothing.** A cap hold is
bounded by the invocation ahead of it rather than by a pause, so it
routinely outlasts a restart — and a held delivery left un-acked comes
back charged one attempt, five of which dead-letter a trigger that was
never refused on its merits. So a drain or shutdown mid-hold **requeues**
the trigger under its own id: the same trigger, arriving at the next
binary as the first delivery it still is. Nothing is lost if the requeue
itself fails; the delivery is simply left un-acked, which is the old
behaviour.

**A waiting trigger occupies no worker permit.** A trigger for a paused
model or for an agent at its cap is pulled and parked, and it takes a
`max_concurrent_invocations` permit only once it is ready to run — after
the pause has lifted and its agent has a slot free. Nothing else takes
one: the dispatch loop itself never holds a permit while it waits for
work. So the worker cap is sized for *running* invocations and nothing
else: waiting work for one agent or one model never stops another agent
from starting, and a worker cap raised for the LLM-bound agents stays
available to them while a build-bound agent is full.

If every permit is busy when a parked trigger becomes ready, it waits
for one the same way it waited for the pause — kept alive, still its
first delivery — and the permits are handed out in the order the
triggers began waiting.

What bounds the waiting triggers instead is the consumer's ack-pending
window: the loop pulls without asking for a permit at all, so triggers
for a paused model or a full agent are pulled and parked until
`max_ack_pending` (twice the worker cap, or the NATS default of
**1000**, whichever is larger) is reached, after which the broker stops
delivering until acks arrive. Each parked trigger is one task and one
in-progress ack per keepalive tick.

## When a tool hangs

Every tool call has a deadline too, applied at the one place all of
them pass through. `[tools] default_timeout_secs` (default 120) bounds
a tool that manages none of its own — every built-in but `exec`, and
every tool an MCP server advertises. `[tools] max_timeout_secs`
(default 900) is the ceiling: a tool that asks for longer is clamped
to it, never refused. `exec` is the one built-in that times itself, so
`[tools.exec]` still decides its deadline within that ceiling; setting
`[tools] max_timeout_secs` *below* `[tools.exec] max_timeout_secs`
would silently cap exec while its own section still read the higher
number, so the daemon refuses to start and names both keys.

The deadline is what the tool is told and what the timeout message
quotes; the runtime's own backstop timer fires **five seconds later**.
The grace is there because every co-operative tool starts its clock
after the host starts its, so an equal timer would always fire first
and replace the tool's own answer — which names the command and
carries the output captured before the kill — with a bare "timed out".
A tool that ignores its deadline is therefore cut off at deadline plus
five, not at the deadline.

Those five seconds belong to the tool, and one thing moves them. While
a tool runs, the MCP server it came from may ask the host for
something — a sampling completion, an elicitation — and the host
answers it there and then, bounded only by `[worker] llm_timeout_secs`
(default 600). An answer that outlasts the backstop would otherwise
spend the whole grace before the tool is looked at again, leaving
`exec` no time to kill its child and hand back what it captured. So an
answer that ends past the backstop re-arms it — five seconds from when
the host turns back to the tool — and is the **last** request that
call services: from there the host is only waiting for the tool to
stop. Anything the server sends in the meantime stays queued, as it
does after any timeout. So one tool call takes at most its deadline,
plus five seconds, plus one server request; the tool's own deadline
does not move, and a chatty server buys it no extra running time.

A call past its deadline is abandoned and reported to the model as a
tool error with `error_kind: timeout`. What the model is told next
depends on who stopped: a tool that timed *itself* out killed the work
and hands over the output it captured, so the effect is bounded by
what that output shows. The host's backstop stopped nothing but the
waiting, so the model is told the work may still be running — a
deadline says nothing about whether the side effect happened, and an
agent that assumes it did not will happily do it twice. An MCP call is
additionally *cancelled*: the host sends `notifications/cancelled`, so
the server is asked to stop rather than finish a result nobody will
read.

`exec` is the strongest form of "killed the work", on both paths: its
own timeout and a call the host drops each end the child's whole
process group — the command and everything it started — so nothing
outlives the call. `[tools.exec] kill_grace_secs` and
`drain_grace_secs` (2s each) bound how long that teardown may take,
and their sum must stay under the host's 5s backstop or the daemon
refuses to start, because a backstop firing mid-teardown would leave
the group alive.

One timeout is something an agent can route around. A run of them is
not: against a dead MCP server every call times out, and an agent left
to keep trying spends its whole budget one deadline at a time.
`[tools] max_consecutive_timeouts` (default 3) ends the invocation
instead, with a terminal `failed` whose message names the count and
the setting; setting it to `0` disables the limit. Any call that
returns — success, or a tool-reported error — clears the run, because
it proves the machinery is alive. An `exec` that hits its own timeout
counts as a timeout, not as an answer: it is the tool most able to sit
for minutes at a time, so exempting it would exempt the case the limit
is for.

`file_read` and the discovery tools refuse a path that is not a
regular file, naming what it actually is. That is a deadline question
as much as a correctness one: opening a FIFO blocks in `open(2)` until
a writer appears, and before the check existed an agent could park an
invocation on a named pipe with nothing above it able to intervene.

A server that reports progress makes a long call visible: the host
logs one rate-limited line per call carrying the invocation, the tool
call and the numbers, and records when each call last reported.

## When an invocation stops making progress

Every individual call an invocation makes is now bounded — a model call
by `[worker] llm_timeout_secs` under its retry cap, a tool call by
`[tools] max_timeout_secs` plus the host's five-second backstop. What
none of those covers is an invocation that is inside none of them and
still not moving: a reducer looping, a step wedged between calls, a
worker alive and beating while the work it holds has stopped.

The daemon watches for that on the same ten-second tick that sweeps
stale workers. An in-flight invocation that has crossed no step boundary
within the **stuck threshold**, and has no tool or model call open
recently enough to explain the silence, is reported — once per stall —
as an `invocation.stuck` event, and counted on `fq doctor`'s executions
line with its id.

The threshold is derived, not configured:

```text
stuck_after = 2 x (timeout_max_attempts x llm_timeout_secs
                   + tools.max_timeout_secs
                   + 5s backstop grace)
```

Twice the longest a single reducer step can legitimately take, given the
deadlines above. At the shipped defaults that is **4210s** — a little
over an hour. Trim a deadline and the safety net under it moves with it.
Ask the daemon rather than working it out:

```console
$ fq status | grep 'stuck after'
  stuck after:      4210s
$ fq doctor
Current executions: 2 in-flight (1 working, 1 stuck after 4210s)
  -> 1 not advanced in >4210s: 019f534f-4b3c-7f42-a619-b5e43a64fd38
  -> `fq invocation show <id>` to inspect, `fq invocation drop <id>` to triage
  -> `fq events query --event-type invocation_stuck` for when each one was flagged
```

Nothing is done about it automatically. The event is a report: it says
which invocation, when it last moved, and against what threshold, and
leaves the decision to you. `fq invocation show <id>` says what it was
doing; `fq invocation drop <id>` abandons it. A stuck invocation that
starts moving again and stalls a second time is reported again — the
event's arrival rate is the rate of new stalls, not of sweeps, so it is
safe to alert on.

`[worker] stuck_threshold_override_secs` replaces the derivation
outright. It is unset by default and meant for an operator narrowing or
widening the report during a live incident; a workload that needs it
permanently is telling you the call deadlines are wrong, and those are
what should move.

## When an MCP server will not start

The daemon does not wait for one. Shared MCP servers start
concurrently at boot, each under `[mcp] startup_timeout_secs` (default
30) for its handshake and `discovery_timeout_secs` for its tool list,
and a server that misses either is marked **unavailable** while boot
carries on. `fq doctor` is where that shows:

```text
MCP servers: 2 declared, 1 unavailable
  github: ok (14 tools)
  internal-search: ✗ unavailable after 3 attempt(s) — failed to start MCP
    server 'https://search.internal/mcp': no initialize response within the
    30s start-up deadline ([mcp] startup_timeout_secs)
  -> agents declaring it are refused at dispatch; next retry in 2m
```

**An unavailable server is not a stopped daemon, and not a silent
degradation either.** Its tools are absent from the registry, and every
agent that declares it fails at once with a terminal `failed` event
whose message names the server and the reason — so the agent does not
run half-equipped and fail later for an unrelated-looking reason. Every
other agent runs normally.

**This is not only a start-up check.** Every connected server is
watched, so one that answered at boot and later loses its connection is
marked unavailable within a second of the transport ending, and dialled
again like any other failure — `fq doctor` going green is a claim about
now, not about start-up.

You usually do not have to do anything. The daemon dials an unavailable
server again on a doubling backoff (`[mcp] retry_initial_secs`, default
30, up to `retry_max_secs`, default 600), and the moment it answers its
tools are rebuilt into the shared registry — the same path a
`tools/list_changed` takes. No `fq reload`, no restart. Set
`retry_initial_secs = 0` if you would rather a server that is down stay
down until you restart.

What to fix is whatever the reason names. A `no initialize response`
means the peer accepted the connection and said nothing: check the
endpoint, or the stdio command's own logs, which reach the daemon log
under `mcp.server.stderr` with the server's name. A message naming
`max_discovery_pages` or `max_tools` means the server's tool list is
pathological rather than slow — a cursor that never advances, or a list
that never ends — and raising the cap is rarely the fix. A message
naming `max_line_bytes` means it wrote a single JSON-RPC message longer
than the transport will read.

## When a consumer stops making progress

`fq doctor` reports every durable consumer this daemon expects, by
name, and says which of them is stuck. The line to look for is

```text
Consumers: 5 checked, 1 unhealthy
  fq-projector: ok (pending 0)
  fq-coordination: ✗ stuck — up to 37 redeliveries past its acked floor, pending 724
  -> its handler keeps failing; check the daemon log for
     `consumer=fq-coordination` and free whatever it is blocked on
     (disk, store, broker)
```

**Stuck means retrying, not lost.** These consumers redeliver without
limit on purpose: an event dropped to a delivery bound is an event the
projection skips for good. When a handler fails transiently the daemon
NAKs with a delay that doubles from one second to a sixty-second cap,
so a fault that does not clear costs one broker round-trip a minute
rather than thousands a second, and the error line about it appears
once per escalation step and then once a minute. The counter in the
line — redeliveries past the acked floor — is the daemon delivering the
same message again while its acked position stays where it was.

What to do is not restart the daemon. The event is safe in JetStream
and the consumer will drain the moment the handler can succeed, so fix
what the handler is blocked on: `df -h` on the state directory first,
since a full disk is the common cause and reaches the projection as
`SQLITE_FULL`. A consumer reported `missing` is different — the durable
does not exist, which means the task that creates it never started, and
the daemon log at startup says why.

`fq status` reports the same consumers under their streams, with the
message counts and each consumer's backlog beside them. That backlog is
the broker's `num_pending` — the matching messages JetStream still has
to offer, counted behind the consumer's own subject filter — and not
the distance to the stream head, which for a filtered consumer counts
traffic it will never be offered and grows with every publish by
anybody. The thresholds are `[bus]` in
`fqd.toml`: `stuck_after_redeliveries` decides when retrying becomes
stuck, and the escalation and log rate are configured there too.

### When a consumer halts on an event it cannot read

Every event carries the envelope's `schema_version`, and every consumer
reads it before anything else
([#409](https://github.com/bricef/factor-q/issues/409)). An event in a
version this build does not read — the stream holds history written by
an older or newer daemon — **halts** the consumer: the message is left
unacked, nothing after it is consumed, and the consumer holds there
until the daemon stops. The daemon itself keeps running, so the report
can say what happened:

```text
Consumers: 5 checked, 1 unhealthy
  fq-projector: ✗ halted — event 01990000-0000-7000-8000-000000000002 on
     fq.agent.researcher.triggered (seq 4242) declares schema_version 2;
     this build reads [3]
  -> the message is unacked and nothing after it is consumed: run a build
     that reads schema_version 2 and restart; the daemon log has the line
     under `consumer=fq-projector`
  fq-coordination: ok (pending 0)
```

**Halted means kept, not lost.** The alternative — acking what the
build cannot read, as it does for malformed bytes — would erase that
history from every projection built from the stream, and a rebuild
would complete "successfully" with a hole in it. So the consumer stops
at the first such event and says which version it found, which versions
this build reads, and where the event sits. What to do is run a build
that reads that version: the unacked message is where the consumer
resumes, and nothing needs resetting. The daemon log carries the same
detail once, at error level, under `consumer=<name>`.

**Malformed is a different fact, and reported separately.** Bytes that
are not an event in any version — JSON that does not parse, or a
supported version whose body does not match its shape — are logged,
acked and skipped, as they always were, and now counted: the line reads
`fq-projector: ok (pending 0, 3 malformed acked)`, and `fq status` shows
`malformed acked: 3` under the consumer. A non-zero count is not an
issue on its own; it says poison was skipped, not that history was
lost. A halt and a malformed count never read as one thing, because
they call for opposite responses.

**A projection rebuild does not halt on the history below its replay
floor.** It starts at the first event this build reads and carries the
rows below that point as they were (see
[rebuilding the projection](#rebuilding-the-projection-fq-projection-rebuild)),
so the weeks after an envelope bump — when the stream still holds the
previous version's events — are not a window in which a rebuild is
unsafe. A halt reported *during* a replay means an unreadable event
above the floor. The other consumers have no floor: a daemon started
with no durables at all against a stream that still holds older
history — a fresh broker seeded from another, or durables deleted by
hand — halts them on it, as this section describes.

### Deleting a durable under a running daemon restarts it

Every consumer the daemon hosts is supervised: if one exits — for any
reason, including cleanly — the daemon publishes `system.task_failed`,
tears the rest down and exits non-zero, so a supervisor restarts it
into a working state rather than leaving it half-running. The
summariser joined that group, which closes a real gap (its stream
ending used to stop summaries silently) and has one consequence worth
knowing before you meet it: **deleting a durable consumer out from
under a running daemon now stops the daemon.** The deleted durable
ends its consumer's message stream, the supervised arm observes the
exit, and the process comes down. This is deliberate and matches every
other consumer; if you need to reset a durable, stop the daemon with
`fq down` first, delete it, and start again. The one exception is the
projector's: `fq projection rebuild` resets `fq-projector` under the
running daemon, because the daemon stops that consumer itself before
deleting the durable and starts it again after.

## Notifications and alerts

Use these terms consistently on dashboards, in runbooks, and in producer
names:

- A **notification** is handled during normal hours and looked at by the
  operator. It may go to Slack or a similar channel. A successful deploy and a
  pricing change refused by the drift bound are notifications.
- An **alert** reaches the operator out of hours because the system cannot
  recover on its own. It escalates and requires human intervention. A service
  that is blocked, no longer accepts requests, and has no graceful recovery
  path raises an alert.

To choose between them, ask: **can the system recover on its own, and can this
wait until morning?** “Yes” to both means notification; “no” to either means
alert.

### Current signal classification

“Current channel” describes where the signal is sent now, not where it should
ultimately be rendered. `notify.sh` uses the configured `FQ_NOTIFY_HOOK`
(Pushover on the dogfood host). Rows marked “once reported” name signals that
exist today but do not yet have an out-of-band producer.

| Signal | Class | Current channel | Recovery path |
| --- | --- | --- | --- |
| Successful deploy (`deploy.sh` via `notify.sh`) | Notification | Pushover | None needed; the new version is already serving. |
| Deploy deferred past `FQ_DEFER_WARN_HOURS` (`deploy.sh` via `notify.sh`) | Notification | Pushover | The hourly deploy retries; a new target clears the deferral, or the operator follows the continuous-delivery runbook. |
| Failed deploy whose rollback succeeds (`deploy.sh` via `notify.sh`) | Notification | Pushover | The script restores the previously serving version and the hourly deploy retries. |
| Failed deploy whose rollback also fails (`deploy.sh` via `notify.sh`) | Alert | Pushover | None; the operator must restore service. |
| Hygiene backup-age, disk, or build-cache report (`hygiene.sh` via `notify.sh`) | Notification | Pushover | The next scheduled check clears transient findings; `FQ_BUILD_CACHE_MAX_GB` pruning reclaims the build cache when no invocation is in flight. |
| Disk full / daemon or broker unable to continue | Alert | None | None; the operator must reclaim space and restore service. |
| Unattended backup failure (`backup.sh` via `notify.sh`) | Notification, except an unsuccessful stack restart is an alert | Pushover | The next nightly run retries ordinary backup or off-host-copy failures; a failed `docker compose up` has no automatic recovery. |
| Watcher `schemaVersionMismatches` (once reported; follow-up to #717) | Alert | None | None; an incompatible watcher skips every event it consumes and must be upgraded or rolled back. |
| `fq doctor`: stale workers | Notification | `fq doctor` | The daemon reclaims stale workers; inspect with `fq workers list --stale-only` if they persist. |
| `fq doctor`: stuck executions / `invocation_stuck` | Alert | `fq doctor` and event stream | None; inspect with `fq invocation show`, then resume or drop the invocation. |
| `fq doctor`: ambiguous invocations | Alert | `fq doctor` | None; inspect, then resume or drop each invocation. |
| `fq doctor`: permanent failures | Notification | `fq doctor` | The invocation has ended safely; inspect the failed invocation and fix its cause during normal hours. |
| `fq doctor`: stuck consumers | Alert | `fq doctor` | None; use the named durable and its reported error to restore consumption. |
| `fq doctor`: unavailable shared MCP servers | Alert when required work is blocked; otherwise notification | `fq doctor` | The supervisor retries startup; if required work remains blocked, fix the server or its configuration. |
| `fq doctor`: throttled models or agents at concurrency caps | Notification | `fq doctor` | Provider backoff and available agent slots release held work automatically. |
| `fq doctor`: dead letters | Notification | `fq doctor` | None for the exhausted trigger; inspect and re-submit it after fixing the cause. |
| Pricing change refused by the drift bound (#735) | Notification | None (producer in #735) | The last accepted pricing remains in use; review and accept the change during normal hours. |
| Pricing data stale (#735) | Alert | None (producer in #735) | None once the safe staleness limit is exhausted; restore the pricing source or provide an override. |
| Summary consumer `start`, `progress`, and `outcome` lines | Neither; these are the record | Event stream and dashboard | Not applicable; consumers display the durable record and do not page on it. |

Notifications belong in the dashboard notification pane (#736) and may also
go to Slack. Alerts go to Pushover today and to the alerting design in #342
when that replaces the current route.

**Rule for new producers:** choose the class first and name its recovery path;
if there is no recovery path, it is an alert.

## Scheduling maintenance with fq-cron

The daemon has no scheduler and is not getting one. Recurring
housekeeping — a pricing refresh, a CAS reachability audit, a TTL
sweep — is scheduled by [fq-cron](../../adapters/fq-cron/README.md),
the standalone adapter, and *run* by the daemon's **maintenance
consumer**: fq-cron publishes to `fq.maintenance.<task>` when a
schedule fires, and the daemon runs the named task in process
(<https://github.com/bricef/factor-q/issues/257>).

The split is deliberate. A maintenance task works on the daemon's own
state — its stores, its pricing table, its projection — which no
out-of-process scheduler can reach; and time-driven firing is a
scheduling problem the daemon should not re-solve. So fq-cron keeps
knowing nothing about factor-q beyond a subject and a payload, and the
daemon gains a consumer rather than a cron.

### The tasks this build knows

| Task | Subject | What it does |
| --- | --- | --- |
| `ping` | `fq.maintenance.ping` | Nothing. Succeeds and says `pong` — the way to prove the whole path is wired on an instance. |
| `pricing_refresh` | `fq.maintenance.pricing_refresh` | Fetches the live pricing document, puts it through the acceptance rules below, and swaps the result into the table this daemon is serving. See *Keeping the table fresh*, below. |

The registry is closed: a subject naming anything else is **refused**,
loudly and on the record, rather than dropped. Check what an instance
knows by scheduling `ping` and reading the outcome.

### A worked job

`fq-cron.toml` — on the dogfood instance this is instance state at
`/var/lib/factor-q/fq-cron.toml`, and editing it *is* the deploy:

```toml
[[job]]
name = "maintenance-ping"
schedule = "@every 1h"
subject = "fq.maintenance.ping"
# The payload is unused by `ping` and opaque to fq-cron; the two
# template variables make the log line and the run id legible.
payload_json = '{"job": "{{job}}", "slot": "{{scheduled_time}}"}'
```

The refresh that keeps prices current is the same shape, and **every six
hours** is the cadence to start from: LiteLLM's document changes a few
times a day, and four fetches a day is well inside an unauthenticated
GitHub budget while keeping the table no more than a quarter-day behind.

```toml
[[job]]
name = "pricing-refresh"
schedule = "@every 6h"
subject = "fq.maintenance.pricing_refresh"
payload_json = '{"job": "{{job}}", "slot": "{{scheduled_time}}"}'
```

On the dogfood instance this job lives in that instance's fq-cron config,
which is deployed from the `fq-dogfood` ops repository and is not in this
one — editing it there *is* the deploy.

Three fields of that job are load-bearing:

- **`subject`** must be a concrete `fq.maintenance.<task>` — one token
  after the prefix, matching a task name above. A dotted tail is not a
  task name with a dot in it, and is refused.
- **`durable`** is left at its default `true`. A durable publish sets
  `Nats-Msg-Id: fq-cron/<job>@<slot>`, which is the **run id** the
  daemon dedupes on; a `durable = false` job is a core-NATS publish
  that no stream captures, so nothing would ever run it.
- **`catch_up`** stays at its default `skip` unless the task is worth
  running late. `once` fires one missed slot on startup, which is the
  right setting for a nightly sweep and the wrong one for anything
  whose moment has passed.

**Deploy the daemon first, then add the job.** The order is
load-bearing in both directions:

1. A build with the maintenance consumer has to have *connected* to
   this broker at least once, because connecting is what creates the
   `fq-maintenance` stream (the daemon's maintenance consumer then
   creates the durable on it). Any factor-q process that connects the
   event bus creates the stream — a `fq` CLI call against this broker
   counts — but in practice it is the daemon's start, and fq-cron is
   the one process that never does it, because it publishes without the
   bus. A durable publish to a subject no stream matches is a
   configuration error on fq-cron's side, not a transient one: it is
   logged loudly and **the
   job is marked unhealthy until a reload** ([fq-cron
   D5](../../adapters/fq-cron/DESIGN.md)). Adding the job first does
   not queue a tick, it breaks the job.
2. Even once the stream exists, the durable is created at the end of it
   (see *What never runs*, below), so a tick that fires between adding
   the job and the daemon's first start is dropped rather than run
   late.

So: deploy the daemon, confirm `fq doctor` lists the `fq-maintenance`
consumer, and only then add the `[[job]]` block. On an instance where
maintenance is switched off, the stream still exists — a disabled
daemon creates it and consumes nothing — so fq-cron's publishes keep
succeeding and the commands age out.

### Reading the outcome

Every message the consumer resolves produces one `maintenance_run`
event on `fq.system.maintenance` — successes, failures, and refusals
alike:

```console
$ fq events query --event-type maintenance_run
maintenance.run task=ping run_id=fq-cron/maintenance-ping@2026-09-13T02:00:00Z succeeded: pong (0ms)
```

A refusal prints `refused:` with the name that was asked for and the
names this build knows; a failure prints `FAILED:` with the task's own
error. **A failed run is over** — the message is acked and nothing is
retried inside the ack loop, because the schedule is the retry and a
redelivered run would stack up behind the next fire.

A failure also raises a `maintenance.run_failed` operator signal — a
notification, since the next fire tries again — carrying the task, the
run id and the error. It is what makes unattended work that stopped
working visible to somebody who is not reading the log:

```sh
fq events query --event-type operator_signal
```

A *refusal* raises nothing. A task name this build does not know is a
`fq-cron.toml` error, and the operator who wrote that file is the one
reading the refusal on the maintenance log.

The durable is `fq-maintenance` on the `fq-maintenance` stream, and
`fq doctor` lists it beside the others, so a consumer that has stopped
keeping up is read the same way as any other (see *When a consumer
stops making progress*, above).

### Delivery, and what runs twice

Delivery is at-least-once. The consumer holds a bounded ledger of the
run ids it has resolved and answers a redelivery from the ledger
instead of running the task again — proven by a test that makes
JetStream redeliver a command underneath the run in flight. The ledger
is in-process, so a daemon restarted between a run and its ack will
run that task once more; every registered task is therefore convergent
by construction (a refresh that overwrites, an audit that recomputes),
never accumulative.

### What never runs: a tick the daemon was not there for

The durable is created **at the end of the stream**, so a command
published before it existed at all is *dropped, not queued*. Three
things follow, and an operator can rely on all three:

- **A missed tick stays missed.** If the daemon is down when a schedule
  fires, that fire does not run when it comes back. fq-cron's schedule
  is the retry: the next tick is the recovery, and for an hourly job
  that is an hour away. Nothing catches up, and nothing is queued
  waiting to.
- **An ordinary restart misses nothing.** That rule is about the
  durable's *first creation*, not about every start. Once
  `fq-maintenance` exists on the broker it keeps its acked position, so
  a command published during a redeploy runs as soon as the consumer is
  back.
- **A first deployment never stampedes.** A durable created fresh — a
  new instance, a broker restored from a message-only export,
  `enabled` flipped back to true — would otherwise run up to a day of
  accumulated sweeps in one burst. Cost is a first-order safety concern
  (design principle 4), and this is the consumer-side mirror of
  fq-cron's own rule that a job with no recorded state never catches
  up.

Both halves are asserted against a real broker, because the tempting
answer to "a scheduled command went missing" is to replay the stream
from its beginning — which buys the stampede back.

### Turning it off

```toml
[maintenance]
# Default true. Set false on every daemon but one when several run
# against a single broker, or the sweep runs once per daemon.
enabled = false
# Ack window for the maintenance durable, ms. Default 60000 — sized
# for a task that fetches over the network, not for a SQLite write.
ack_wait_ms = 60000
```

A disabled daemon creates no durable and consumes nothing; the stream
still exists, so fq-cron's publishes succeed and the commands age out
after 24 hours. `fq doctor` stops expecting the consumer, rather than
reporting a permanent `Missing` nobody can clear.

## Pricing: the live table, and what the daemon accepts from it

Prices come from the [LiteLLM
table](https://github.com/BerriAI/litellm)'s `main` branch, fetched at
startup and again on a schedule
([#344](https://github.com/bricef/factor-q/issues/344); see *Keeping the
table fresh*, below). **The live table is the source, and the discipline
is on acceptance**
([#735](https://github.com/bricef/factor-q/issues/735)).

Pinning a commit would look safer and is not. LiteLLM adds models
weekly, and under [ADR-0004](../adrs/accepted/0004-cost-controls-from-day-one.md) a
model with no price is a daemon that refuses to start — so a pinned
table turns "prices drift" into "the daemon will not run a new model
until a human bumps a SHA", which is an automated process made manual,
and manual processes go stale. The threat was never that upstream
changed; that is what the source is for. The threat is upstream
changing *badly*: a price zeroed by mistake or by compromise, a nonsense
multiplier, a malformed file. So the daemon judges every table it is
offered, model by model, against the last one it accepted.

### The rules

1. **A price that moves by more than 5× in either direction is not
   accepted for that model.** The prior price stays, every other change
   in the table lands, and the refusal is recorded. The bound is a
   nonsense detector, not a change detector: model prices move a lot
   and quickly, and 5× is the margin that separates a repricing from a
   mistake.

   A repeated refusal is one notification until that refused change stops.
   To accept the change, set a per-model `[pricing]` override, as decided in
   [#757](https://github.com/bricef/factor-q/issues/757).

   This bound applies to each load and is measured against the last
   **accepted** price. A sequence of accepted, in-bound moves is therefore
   unbounded in aggregate: four accepted 4.9× moves compound to about 576×,
   which the six-hour refresh cadence can reach within a day. This is a
   [deliberate acceptance](https://github.com/bricef/factor-q/issues/746):
   the bound guards against a single bad upstream change, not how far a price
   can eventually move. The mitigation is the `pricing.change_refused`
   notification trail and an operator reading it.
2. **A model priced at zero where it was not is refused the same way.**
   A *new* model priced at zero is refused at admission: it never
   enters the table, so ADR-0004's at-use backstop refuses the dispatch
   rather than letting it run and track as $0.
3. **A new model is admitted only if every token category it reports
   carries a positive price.**
4. **A price of zero is never a prior.** A cached entry that would not
   pass the floor today is not a price to measure a move against — there
   is no ratio to a zero — so the model is judged at admission instead.
   Still priced at zero, and it is dropped, exactly as it would have been
   on an empty cache; carrying a real price at last, and it is admitted
   on plausibility alone. A model that was free and starts charging
   therefore lands at the new price rather than billing at $0 for ever.
   **No accepted table holds a price of zero**, whichever route the model
   took into it.
5. **A refused model reverts whole.** Half a model's prices from one
   document and half from another is not a price list, so the refusal
   names the first field that failed and the model keeps all of its
   prior figures.

A model the source has stopped listing is not carried over **at
startup**, which is a safe boundary because nothing is running yet. A
*refresh* keeps it; see *Keeping the table fresh*.

### Keeping the table fresh

A daemon that runs for weeks would otherwise serve the prices it booted
on. The `pricing_refresh` maintenance task fetches the document again on
whatever cadence `fq-cron.toml` fires it at (*Scheduling maintenance with
fq-cron*, above; six hours is the suggested start), runs the same
acceptance rules, and swaps the result into the table the daemon is
serving — including the invocations already in flight. The refresh reads
and writes the same `[pricing] source` and the same cache file the
startup load used, so each document is judged against the one before it.

**A refresh only ever widens the priced set.** New models and accepted
price changes land immediately. A model that upstream has stopped listing
**keeps its price**: under
[ADR-0004](../adrs/accepted/0004-cost-controls-from-day-one.md) a model
with no price is a *refused dispatch*, so a table that narrowed under a
running invocation would break it mid-flight, which is the failure that
issue exists to prevent.

**Removals are applied at the next daemon start.** The cache a refresh
writes is the accepted document, so a model upstream dropped is already
absent from it; the next start therefore neither compares against it nor
serves it, and retired prices do not accumulate across restarts. Start is
the boundary because it is the only moment at which "nothing is using
this model" is a fact rather than an estimate — and it is where the
coverage guarantee is enforced with an operator in front of it, since a
start that would leave a declared model unpriced refuses to run and names
it. A daemon holds a retired price for at most one lifetime; restart it
if that matters.

Prices layered over the accepted table — OpenRouter's catalogue for the
models routed there, and `[providers.<name>.pricing]` overrides — are
configuration, and a refresh does not move them. An override is your
answer to a price the source gets wrong; it changes when you change it,
not on a timer.

What an operator sees:

| What happened | Where it shows |
| --- | --- |
| The refresh ran | one `maintenance_run` event: `N entries (N new, N repriced, N refused, N not admitted)`, plus a count of models no longer listed upstream |
| A change was refused | one `pricing.change_refused` notification per model, exactly as at startup — model, field, old, new, ratio, rule |
| The fetch did not land | one `pricing.fetch_failed` notification, on the **first** refresh that fails and not again until one succeeds. The run still **succeeds**: serving the last accepted table is a working state, and the outcome line says `fetch failed; still serving the last accepted table` |
| The table is past `[pricing] max_age` | one `pricing.stale` alert, raised **once** when the table crosses the window. The first refresh that lands a document publishes a `pricing.stale` notification naming that alert in `resolves`, which is what closes it |
| The task itself failed | `maintenance.run_failed` (see *Reading the outcome*, above) |

```sh
# What the last refresh did
fq events query --event-type maintenance_run

# What it refused, and why
fq events query --event-type operator_signal
```

Turning the schedule off is removing the job from `fq-cron.toml`; the
daemon then behaves exactly as it did before — prices are fetched at
startup and not again.

### Where a refusal shows up

Each refused model raises one `pricing.change_refused` operator signal
per load — a notification, not an alert, because the daemon carries on
at the prior price and nothing is broken. Its `detail` names the model,
the upstream field, the old and new prices (per token, as the upstream
file states them), the ratio, and which rule refused it:

```sh
# What has been refused, and why
fq events query --event-type operator_signal
```

`refused` on the outcome line counts refused **changes** — one per
`pricing.change_refused` notification, so the line and the pane agree.
Models that were never admitted are counted separately as `not admitted`
and raise nothing; on the live table that is several hundred on every
run, which is why the two are not one number.

Models refused **at admission** raise nothing, and deliberately: the
live table lists several hundred free, local and embedding entries
priced at zero, so every start refuses every one of them. That is a
standing property of the source rather than something that happened, it
is identical on every load, and there is nothing to do about it — so it
is a count in the daemon's log line (with the names at `debug`) rather
than three hundred lines in a pane. The moment one of them matters —
something declares it — ADR-0004's startup guarantee refuses to run and
names it, which is louder than any notification.

A fetch that does not land raises `pricing.fetch_failed` and the daemon
serves the last table it accepted. A table that has not refreshed
within `[pricing] max_age` raises `pricing.stale` — an **alert**, since
no further attempt recovers a source that has stopped answering, and
prices silently older than the models they price is the failure
ADR-0004's guarantee exists to prevent.

Both are **conditions, not events**, so both are raised on the edge:
once when the condition begins, and never again while it holds. The
first load that fetches and accepts a document publishes a notification
of the same kind naming the signal it closes, and the pane's "open
alerts" count falls by one. Without that, a refresh every six hours
turns a week of a broken upstream into twenty-eight open alerts that
nothing can ever close.

The memory is the running daemon's. Restart it while the table is stale
and the startup load raises a fresh alert — a new episode, naming the
age it found — while the alert the previous run raised stays open,
because the run that would have resolved it is gone. Resolve it by
reading the log: the newer alert is the live one.

### What is on disk, and what is on the record

The cache under `[cache] directory` holds **accepted tables only**:
`pricing.json` is the table the daemon accepted, models in name order,
and `pricing.provenance.json` beside it says which document it came
from — the upstream commit and a SHA256 digest of exactly those bytes.
That pair is what "the last accepted table" means, and it is what the
next load's 5× bound is measured against. Editing `pricing.json` by
hand is not forbidden and is not hidden either: the digest stops
matching, and the daemon serves the file while claiming nothing about
where it came from.

A daemon upgrading from a build that predates acceptance finds the *raw*
upstream document in that file, with no sidecar beside it — several
hundred free, local and embedding entries at $0 among the real prices.
Rule 4 is what stops those becoming accepted prices: the first load
judges each of them at admission and drops it, so the rewritten cache is
an accepted table like any other. Nothing has to be deleted by hand.

The same provenance rides the `system.startup` event, and every cost
row cites the short version derived from it
(`litellm-main@3f9a1c0b2d4e`) — on the event and on the projected row,
which outlives the event under the retention sweep. A spend figure is
traceable to the prices that produced it for as long as the figure is
kept:

```sh
# Which table this daemon is running on
fq events query --event-type system_startup --limit 1
```

The version names the accepted LiteLLM table. Prices layered over it —
OpenRouter's catalogue for models routed there, and
`[providers.<name>.pricing]` overrides — are configuration, and are
reported at boot rather than folded into the digest.

### Configuration

```toml
[pricing]
# Where the table comes from: "litellm-main" (default, the live
# document) or "pinned:<sha>", which fetches that commit's copy
# forever. Pinning is for an air-gapped or regulated deployment that
# reviews the diff itself; it must not become the default, for the
# reason above.
source = "litellm-main"

# How far a price may move, in either direction, and still be accepted.
# Default 5.0.
max_drift_ratio = 5.0

# How old the accepted table may be before a load raises the
# `pricing.stale` alert. Default "7d"; also accepts hours, minutes and
# seconds ("36h", "90m", "30s").
max_age = "7d"
```

A setting that does not parse — a source that is neither spelling, a
bound of 1 or less, a window that is not a duration — refuses the
start. An operator who asked for a pin and silently got the live
document has the opposite of what they configured.

## Seeing what needs a person: the notifications pane

The dashboard's **notifications** pane is where the daemon's components
say an operator should look at something. It reads a resource of its
own — `operator_signal` — folded from the `operator_signal` events on
the log, so what the pane shows is a record rather than a message that
was sent somewhere and forgotten.

Two severities, and the difference is a human's hours:

- A **notification** is handled during normal hours and looked at by
  the operator. A refused pricing change is one: the daemon carries on
  at the prior price, and someone should know.
- An **alert** reaches the operator out of hours, escalates, and names
  something the system cannot recover from on its own.

The pane marks the two apart in form as well as colour — an alert wears
a stripe down the row and the word `alert`, a notification wears a chip
— because an out-of-hours page is read on a phone, in the dark, and a
hue is not a distinction to bet that on.

### What it shows

- **`/notifications`** — newest first, one row per signal, with its
  source, its kind, when it was raised and its one-line summary.
  Filters for severity (`all` / `notifications` / `alerts`) and for
  source; the filter rides the query string, so a link to a filtered
  pane is a link somebody else can open.
- **`/notifications/{id}`** — one signal in full: the structured
  particulars its producer sent, links to the invocation, agent or page
  it concerns, the envelope of the event it rode in on, and the signals
  either side of it from the same source.
- **The home page** carries the count: *N in the last 24h · M open
  alerts*, linking into the pane. It is red only while an alert is
  open, and amber saying *unknown* if the count could not be read —
  which is not a count of zero, and most often means the dashboard's
  token predates the `read:operator_signal` grant.

From a terminal, `fq notifications list` is the same listing and
`fq notifications show <event-id>` the same detail page; both take
`--json`.

### Retention: alerts are kept, notifications age out

A notification lives as long as the event it was folded from — the
event log's 30-day window, or whatever `[state] retention_days` says.
**An alert is never swept.** The record that the system could not
recover on its own and a person had to intervene is worth more than the
log it arrived on, so the pane can still show what needed a human last
quarter, whole, particulars included.

**Neither is the signal that resolved one.** A recovery is a
notification (see below), but an alert is closed by that row existing,
so letting it age out would re-open an alert answered a month earlier —
and nothing could ever close it again. An alert's record is the pair, so
retention keeps the pair: a notification naming another signal in
`resolves` is kept for as long as the alert it closed. An ordinary
notification, which closes nothing, still goes with the log.

That means the two counts on the home page are not symmetrical, and
deliberately: notifications are counted inside a day, alerts are not
counted inside a window at all. What bounds them is resolution.

### Open and resolved: what closes an alert

**An alert is open until a later signal resolves it.** A signal may name
the one it closes — the earlier signal's event id, in the payload's
`resolves` — and a recovery normally does, as a *notification* carrying
the same `kind`, because the end of an alarm is not itself alarming. A
recovery closes every open alert of its kind, including alerts raised by
a previous daemon run. The home page's *M open alerts* is that fold:
alerts with no later recovery of their kind. It falls when the component
that raised the alert says the condition has passed.

The pane shows both ends. A resolved alert loses its stripe, reads
`▲ alert · resolved`, and carries a link to the signal that closed it;
the resolving signal carries a link back to the alert it closed. Nothing
is deleted — a resolved alert is history and stays listed, and
`fq notifications show` prints `resolved-by` (or `state open`) for it.

This is **not** an acknowledgement. There is no seen-mark in this
version: reading a signal does not close it, and only another signal
does. A per-operator seen-mark is a later change, once there is more
than one operator to have seen anything.

### What this pane is not

It does not page anybody. Fan-out to Slack and Pushover is a separate
concern with no consumer yet: `ops/dogfood/notify.sh` keeps sending what
it sends, and until a fan-out consumer exists an alert reaching this
pane has not, by itself, woken anyone. Watch it, or watch the count on
the home page.

## Quick reference

| Goal | Command |
| --- | --- |
| Pair a client, from a terminal (confirms the fingerprint) | `fq connect <addr> --token "$(cat ~/.local/state/factor-q/edge/admin.token)"` |
| Pair a client, from a script (no prompt; the pin is required) | `fq connect <addr> --token "$(cat ~/.local/state/factor-q/edge/admin.token)" --fingerprint "$(cat ~/.local/state/factor-q/edge/fingerprint)"` |
| Stop the daemon (clean, confirmed) | `fq down` |
| Stop now, skip the drain | `fq down --now` |
| Redeploy (suspend for the next binary) | `fq down` |
| Hot-reload agent definitions | `fq reload` |
| Re-derive the projection from the event stream | `fq projection rebuild --yes` (`fq status` reports the replay) |
| Inspect daemon / worker health | `fq status`, `fq workers list`, `fq doctor` (all three ask the daemon; `fq status` reports its absence as a finding rather than failing) |
| See which consumers are keeping up | `fq doctor` (names every durable and any that is stuck) |
| See whether a consumer halted on an event this build cannot read, and how many malformed messages it skipped | `fq doctor` (the `halted` line names the version found and the versions read; `malformed acked` is the skip count), `fq status` |
| See the stuck threshold this daemon derived | `fq status` (the `stuck after` line) |
| Find invocations that stopped making progress | `fq doctor` (the executions line names them), `fq events query --event-type invocation_stuck` |
| See which models a provider is throttling | `fq status`, `fq doctor` (the throttled-models block: pause end, permits, 429s) |
| Find invocations put down for a rate limit | `fq events query --event-type invocation_deferred` (they resume on their own) |
| Schedule recurring maintenance | an `fq-cron.toml` job publishing to `fq.maintenance.<task>` (see *Scheduling maintenance with fq-cron*) |
| See what maintenance has run | `fq events query --event-type maintenance_run` |
| See which pricing table this daemon accepted | `fq events query --event-type system_startup --limit 1` |
| Refresh prices without restarting | an `fq-cron.toml` job publishing to `fq.maintenance.pricing_refresh` (see *Keeping the table fresh*) |
| See what pricing changes were refused | `fq events query --event-type operator_signal` |
| See what has asked for a person's attention | `fq notifications list`, or the dashboard's notifications pane |
| See only what could not recover on its own | `fq notifications list --severity alert` (alerts are never swept, so this reaches back past everything else) |
| Read one signal in full | `fq notifications show <event-id>` |
| Clear stale workers | *nothing — the daemon sweeps them* |
| Find unresolved invocations | `fq invocation list --status=ambiguous` |
| Settle one, keeping progress | `fq invocation resume <id>` |
| Settle one, abandoning progress | `fq invocation drop <id>` |

## See also

- ADR-0027 — graceful drain for deploys (the machinery used by `fq down`).
- `adapters/fq-cron/DESIGN.md` — the scheduler that fires the
  maintenance jobs above: its durability, reload and missed-fire rules.
- ADR-0006 Appendix E — why stale-worker reclamation is a daemon sweep
  and not an operator verb.
- `fq status`, `fq doctor`, `fq workers list` — confirm the daemon and
  worker state after a **deploy**. Not after a stop: all three ask the
  daemon, so once it is down the answer is that it is down. `fq status`
  is the one that says so usefully — it names the absence as the
  finding, still prints the edge address it resolved locally, and exits
  non-zero so a deploy script can branch on it. It deliberately does
  **not** print store paths in that state: those are the daemon's, and a
  client printing its own guess would be describing this machine rather
  than the runtime.
