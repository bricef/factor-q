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

An invocation that is still being set up when the drain lands (its workspace
being provisioned, its MCP servers starting) has taken no step and written
nothing to resume, so it is not suspended. It stops, its workspace is removed,
and its trigger is left unacked with the broker. The next start receives the
trigger again once that delivery's ack window has lapsed (30 seconds for a
first delivery, longer for a redelivered one: 2, 5, then 12 minutes) and runs it
from the beginning, once, as its next attempt and under a new invocation id.
The first invocation's `triggered` event stays in the log with no outcome after
it. Like a drain that lands between a trigger's pull and its dispatch, this
costs the trigger one of its five deliveries. A trigger caught in setup on its
fifth and last delivery is not redelivered: when that delivery's window lapses,
the advisory watch dead-letters it as `trigger_exhausted`, although it never
ran, and `fq dead-letters requeue <agent>` puts it back
([#974](https://github.com/bricef/factor-q/issues/974)).

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
> migration, a recovery scan and its wait for the coordination backlog
> (bounded by `[worker] recovery_backlog_wait_secs`), an unresponsive MCP
> server. This is the price of
> the bind being the instance lock, and it is the right trade (the alternative
> leaves a window where a second daemon can start), but it is worth knowing
> before concluding a daemon is wedged. The daemon's own log says which step it
> is on; a stop signal is answered throughout, so a boot you have given up on
> can be stopped with `docker stop` or a plain `kill -TERM`.

Stopping a boot mid-migration is safe. Each store's migration on start is one
transaction: a daemon stopped or killed part-way leaves the store at the
version it had, and the next start runs the whole migration again. If another
process holds a store's write lock when the daemon opens it, the daemon waits
up to 30 seconds for it and then fails to start with `database is locked`,
naming the store. One exception to "a stop signal is answered throughout":
nothing reads one while the stores open, so neither that wait nor a long
projection rebuild on start ends on `SIGTERM`. `SIGKILL` (what `docker stop`
sends once its grace period runs out) ends them at once, and is safe there
for the reason above: nothing they have done is committed.

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

# The detail on one: owner row, archive row, the trigger delivery that
# started it, recent events
fq invocation show <invocation-id>
```

`fq invocation list` reads the control plane's ownership rows, and a run
started from a trigger gets its row when it ends, or when something
else happens to it first: startup recovery restores it, it is reported
ambiguous, or you drop it. **A run still running is therefore not in the
list**, `--status=in_flight` included. `fq doctor`'s current executions
count it, and `fq invocation show <id>` shows its live execution
([#721](https://github.com/bricef/factor-q/issues/721) is the open
question of a row from the run's start). A run that has ended reads
`completed` or `failed` from the moment its ending is folded, whether or
not its archive has reached the control plane yet, and the list's
`archive` column says which: `pending` until it has, `held` while the
worker's archive sweeper is holding it back (see
[When the archive sweeper holds a terminal row](#when-the-archive-sweeper-holds-a-terminal-row)),
`yes` once it has, and `-` for a run that has not ended. `fq invocation
show` says the same, with the hold's reason and remedy, and the JSON of
both, and of `fq workers show`'s owned rows, carries it as
`archive_state`. The column replaced `arch`, which said something else:
whether the row was read from the archive table alone, because no
ownership row the list included names the run. The JSON still carries
that as `archived`, so a row can read `"archived": false` beside
`"archive_state": "archived"`.

The list is ordered by the time each run's row was first written,
newest first: when the run ended, for a run started from a trigger, or
the run's start, for one startup recovery restored or whose ending the
archive sweeper held. A later move keeps
that time, except recovery's restore, which sets it to the run's start
again. So a resumed run that ends, or an old ambiguous run you drop,
keeps its place rather than rising to the top, and with `--limit` it
can fall outside the list; `fq invocation show <id>` finds any run.

If the coordination consumer reads the event stream again from its
start, because its durable was deleted and recreated, it meets the
`failed` events of builds before
[#1024](https://github.com/bricef/factor-q/issues/1024) too. Some of
those name a phase other than `setup` for a run that never started,
and each such run gets a `failed` row whose archive never arrives,
which reads as a start on record.

A row this build cannot read, a status or archive state written by hand
or by a newer build, is left as it is. Events for it are logged at warn
and passed over, startup recovery logs it at error and decides from the
worker's WAL whether to resume the run, and the stuck-invocation sweep
goes on past it. Other reads fail on it: `fq invocation list` with no
`--status`, `fq invocation show` of that run, `fq workers list` and
`show`, and `fq doctor`, each with a malformed-row error, and the
retention sweep once a stale worker owns the row. `fq invocation drop`
of the run works: it stops the run, writes the archive record and marks
the worker's row finished, though the command then says that reading the
run back failed, since `show` fails on the row. The ownership row itself
keeps reading as it
did, and only a hand edit of `coordination_invocation_owner` clears it
([#1082](https://github.com/bricef/factor-q/issues/1082)). Such an edit
sets `status` and `archive_state` together, to a pair a row may hold:
`in_flight` or `ambiguous` with `not_due`, `completed` with `pending` or
`archived`, `failed` with `pending` or `archived`. A run that ended and
whose archive the control plane holds is `failed` (or `completed`) with
`archived`. A row whose status was edited alone, so that its archive
state does not go with it, is corrected when the daemon first starts
with the termination record: the archive state becomes the one its
status implies (`not_due` for a run in progress, `archived` for one that
ended), and the daemon logs `an ownership row's archive state did not go
with its status` at error, with the invocation, the status and the
archive state before and after.

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

A drop ends the run as `failed` from whatever its ownership row says, a
finished run included, and writes the run's archive record itself, with
no state. A worker's archive that arrives after the drop keeps the
drop's ending. What that leaves inconsistent, a dropped completed run
whose archive record still says `completed`, and a run whose real final
state is lost when the drop lands before the worker's archive, is
[#1070](https://github.com/bricef/factor-q/issues/1070).

A step already running when you drop the run is not interrupted. A tool
call in flight finishes, and its result is recorded; a model call in
flight returns, and its response, with what it cost, is recorded. But
once the drop has reached the worker's record of the run, the run
publishes no ending of its own: no `completed`, even if that last model
call gave a final answer, and no `failed` if that call or tool fails
([#1070](https://github.com/bricef/factor-q/issues/1070)). The drop is
the outcome. Whichever marks the worker's record finished first wins:
the run writes its own ending into the record before it publishes it,
and only while the record is unfinished, so a failure in the instant
the drop reaches the worker either ends the run itself, its `failed`
going out with its own error and the drop finding the record already
finished, or loses to the drop and publishes nothing of its own.

Startup recovery also reports an invocation `ambiguous` when it resumed
one on its own and the resume could not be carried out, leaving the
invocation in flight (or its worker row unreadable). The event carries
`stuck_entity: recovery` and the error in its `note`, and the daemon
logs `resume failed; emitted invocation.ambiguous`. The event goes out
once per invocation, however many starts fail the same way: on each
later start the line ends `invocation.ambiguous was reported on an
earlier start and is not emitted again`. Two things differ from a stuck
tool dispatch:

- **It reads `ambiguous` only until the next start.** That start's
  recovery puts the ownership row back to `in_flight` before it resumes
  the invocation again, and the event that would turn it back is not
  sent a second time, so the invocation leaves `fq invocation list
  --status=ambiguous` and `fq doctor`'s ambiguous count while still
  unsettled ([#1025](https://github.com/bricef/factor-q/issues/1025)).
  The daemon log line above is what still says so, on every start.
- **`resume` cannot settle it.** There is no stuck dispatch to
  reconcile, so `fq invocation resume` refuses it: as `is not Ambiguous
  (it may already have been resumed)` while the row reads `ambiguous`,
  and as `is live; only Ambiguous invocations can be resumed` once it
  reads `in_flight`. `fq invocation drop` ends it. So does removing the
  cause and restarting, since every start resumes it again.

A resumed run that ends is not one of these, even when it ends in
`failed` and the resume returns an error. Its `failed` is published,
and the daemon logs `resume returned an error, but the invocation has
ended; not reporting it ambiguous` at warn, with the error. Its
ownership row reads `failed` from the moment the `failed` is folded,
whether or not the archive is ever delivered, and `fq invocation list`
shows its archive `pending`, or `held` while the archive sweeper holds
it ([#1024](https://github.com/bricef/factor-q/issues/1024)). Before
[#1005](https://github.com/bricef/factor-q/issues/1005) such a run was
reported ambiguous as well, and left for an operator to triage.

A run that fails part-way through a step marks its WAL row finished,
with its `failed` stored in it, before it publishes the `failed`
([#1070](https://github.com/bricef/factor-q/issues/1070)). A daemon that dies between the two leaves the row
finished with its ending owed: the next start does not resume the run,
and the archive sweeper publishes the stored `failed` on its first pass.
Builds before that published first and marked after, so a run could
publish its `failed` and die before its mark. The next start's
coordination consumer folds such a `failed` before recovery reads
anything, whether or not the daemon that died had folded it (see
[When startup recovery holds a run](#when-startup-recovery-holds-a-run)),
so the ownership row records the ending, and recovery, which reads the
ownership rows before it classifies anything, marks the WAL row
terminal and hands it to the archive sweeper: the first ending stands.
Before [#1024](https://github.com/bricef/factor-q/issues/1024) such a
run was always resumed, and could end a second time. Until
[#1070](https://github.com/bricef/factor-q/issues/1070) it still was
when the daemon had died before folding its `failed`: the resumed run
went on under a row that read `failed`, and if it then completed, its
archive record said `completed` under that row.

A second ending that reaches the control plane is refused and logged at
error, `two endings published for one run; the ownership row keeps the
ending it recorded`, with the ending `recorded`, the one `reported`, and
the event type. When the row records `failed` with its archive already
written, which is what a drop leaves, as when you drop a run as it
finishes, the same refusal is logged at warn instead: `an ending reached
a run recorded failed with its archive written, as an operator's drop
leaves it`. A worker's own failure leaves the same state once its archive
arrives, so a second ending after that is logged at warn too, although
no drop was involved.

A report that reaches the control plane after the run is recorded
`completed` or `failed`, a drop included, leaves the ownership row as it
was: the run has ended, and the report came late. One way it happens:
you drop a run with `--live` while it is inside a tool call, the daemon
dies before the step boundary and before it has folded the drop, and
the next start's wait for the coordination consumer runs out before the
consumer reaches the drop; that start's recovery then finds the run in
flight with the tool handed off and reports it ambiguous, after the drop
on the event stream. (When the consumer does reach the drop in time, the
drop marks the run's WAL row terminal first, and recovery has nothing to
report.) The
daemon logs `event for a run already recorded finished; the ownership
row keeps its status` at warn, with the `invocation_id`, the `agent_id`,
the `event_type` (`invocation_ambiguous`), the state it kept
(`recorded`, as `status/archive_state`) and the late report's
`stuck_entity` and `note`, which say whether it came from startup
categorisation (a dispatch kind) or from a resume that could not be
carried out (`recovery`, with the error). There is nothing to triage.
Before
[#1024](https://github.com/bricef/factor-q/issues/1024)'s first change
such a run went back to `ambiguous`.

An invocation waiting for a worker permit is not being driven yet, even
though every surface shows it as in flight (or, for one you resumed, as
still ambiguous), so a plain `drop` takes it with no `--live` needed.
For a run startup recovery queued and a `resume` waiting its turn, by
the time `drop` returns the run can no longer start in this daemon, and
a drop that fails leaves it waiting as before. A drop that errors or is
cut off (a timeout, a cancelled client) may still have published its
event, and then the run, left waiting, can start before the event is
folded.

An invocation deferred for a provider's rate limit is not being driven
either, and a plain `drop` takes it the same way, whether it is still
sleeping out its wait or the wait is over and it is waiting for a
permit: it is not resumed in this daemon, makes no further model call
and publishes no ending of its own, so the drop's event is its ending,
after its `invocation.deferred` on the stream. One still sleeping is
woken when the drop goes through and frees its agent's slot just after
`drop` returns (within a few tens of milliseconds), not when the wait
would have ended: `fq doctor` stops counting it under "Agents at cap",
and a trigger held behind it starts. One waiting for a permit leaves the
queue within a quarter of a second.
Before [#1070](https://github.com/bricef/factor-q/issues/1070) the drop
did not reach a deferred invocation: it kept its slot until its wait
ended, and its resume then ran until it found the drop folded, at once
if the fold had landed and otherwise after one more model call.

In the instant a queued or deferred run's permit arrives it is starting
rather than waiting, and `drop` refuses it like a live run (`starting to
run; retry with --live`). A drop that lands in the instant a run is
being put down, as its model's rate limit comes back, holds it too: the
daemon remembers the drop for an hour, and the run is not put down. Its
slot is freed at once and nothing resumes it. One case is left open: two
drops of the same run at once in that instant, where the second holds
the run just as the first is remembered and then fails. The run then
stays put down and is resumed when its wait runs out. A run dropped with
`--live` while its model call was in flight, and then put down by that
call's rate limit, announces its `invocation.deferred` after the drop on
the stream, since no step boundary came between for it to see the halt;
nothing of its own follows that announcement.

An accepted `resume` re-drives the invocation under the worker cap like
any other run: if `max_concurrent_invocations` runs are already in
flight, it waits for one of them to end rather than starting beside
them. The command answers once the interrupted results are recorded, so
`Resume accepted` means running or waiting its turn, and the daemon log
says which (see
[Every run takes a worker permit](#when-a-provider-hangs-or-throttles)).
The agent's own `max_concurrent` does not hold it back: the run counts
against that limit but does not wait for a slot under it, so the agent
can be over its limit until one of its invocations ends (the same
section says why, and what to do if that matters).
A daemon that is already draining or stopping refuses the re-drive and
says so, but the recorded results stand, so the next start resumes the
invocation on its own.

### When startup recovery holds a run

A daemon can stop with events on the event stream that its
coordination consumer has not folded yet: a drop, or a run's own
`failed`, published just before it died. So each start begins its
coordination consumer first, and waits for it to fold everything that
was on the stream when the start began before recovery decides which
runs to resume ([#1070](https://github.com/bricef/factor-q/issues/1070)).
Normally that takes a moment. Events that aged out of the stream's
retention while no daemon ran are not waited for: once the broker has
nothing left to deliver to the consumer, it counts as caught up. The
wait is bounded by `[worker] recovery_backlog_wait_secs`, 30 seconds by
default, and the boot, the edge included, waits with it.

When the bound runs out, the daemon boots anyway and resumes none of the
runs it would have resumed, since an event it has not folded may have
ended any of them. It logs each at error, with its `invocation_id`, the
`boot_point` (the stream's last sequence when the start began) and
`folded_to` (how far the consumer had got):

```text
not resuming this invocation at startup: the coordination consumer had not
folded the events on the stream within [worker] recovery_backlog_wait_secs,
and one of them may end it. It stays in flight until a later start finds
them folded: restart the daemon once the consumer has caught up. ...
```

A consumer that halts on an event this build cannot read ends the wait
at once, since it folds nothing more however long the start waits: the
daemon logs `the coordination consumer halted on an event this build
does not read; startup recovery stops waiting for it` at warn, with
`halted_at` (that event's sequence) and its `schema_version`, and the
line for each held run says the consumer halted instead of naming the
bound. The start then goes on as when the bound runs out.

**A held run waits for the next start.** Nothing in this daemon resumes
it ([#1089](https://github.com/bricef/factor-q/issues/1089)). Restart
the daemon once the consumer has caught up (`fq doctor` and `fq status`
show its backlog among the consumers): that start's recovery decides
over the folded events as usual, and resumes the run or finds it ended.
Behind a halted consumer no restart helps until a build that reads the
event it halted on runs; every start holds the run again until then.

Only the log names a held run. What each command shows for one:

- `fq status` ends `All clear.`
- `fq doctor` counts it among the current executions, in flight. The
  stale-worker and stuck-invocation sweeps wait only until recovery has
  decided and then run as usual, so once a held run is past the stuck
  threshold `fq doctor` reads it stuck and the sweep reports it, as for
  any run that has not moved.
- `fq invocation show` reads `in_flight`, with a live-execution block,
  though nothing is executing the run.
- `fq invocation resume` refuses it as live: it takes only ambiguous
  runs, and recovery has put the held run's ownership row back to
  `in_flight`.
- `fq invocation drop` drops it as it drops any run nothing is driving:
  the drop takes effect once the consumer folds it, and no later start
  resumes the run. Behind a halted consumer the command returns a
  receipt and then fails its read-back (`invocation.get` lagging), since
  nothing folds the drop; the run stays held, and the drop is folded
  when a build that reads the halting event runs.

A surface that names held runs is
[#1089](https://github.com/bricef/factor-q/issues/1089)'s too.

The consumer is behind for one of these reasons:

- **It halted on an event this build cannot read.** `fq doctor` says so,
  and it folds nothing until a build that reads that event runs (see
  [When a consumer halts on an event it cannot read](#when-a-consumer-halts-on-an-event-it-cannot-read));
  every start holds the runs again until then, and stops waiting for the
  consumer as soon as it halts, so a longer bound changes nothing.
- **Its durable was deleted and recreated**, so it reads the whole
  stream again from the start
  ([Deleting a durable under a running daemon restarts it](#deleting-a-durable-under-a-running-daemon-restarts-it)).
  Restart once the replay has caught up, and that start resumes the held
  runs.
- **The backlog is long, or the store is slow.** A backlog that is long
  and still moving is what a longer `recovery_backlog_wait_secs` is for.
  A slow or contended disk slows every fold, and a longer bound only
  waits longer for it (see
  [#1090](https://github.com/bricef/factor-q/issues/1090)).

A delivery the stopped daemon took and never acknowledged is the usual
reason a start waits more than a moment: the consumer takes one delivery
at a time, and the broker gives it back only when its ack window lapses.
A clean stop can leave one too
([#1090](https://github.com/bricef/factor-q/issues/1090)). The
coordination consumer's window is five seconds, where other consumers
take `[bus] ack_wait_ms` (30 seconds by default), so that wait stays
well inside the bound.

### When the archive sweeper holds a terminal row

A finished invocation's row stays in `worker.db` until the control plane
acknowledges its `invocation.archived`. Since worker schema v12 the row
also keeps the event the invocation ended in — `completed`, or the
`failed` of a run that had started — until that event is known to have
reached the bus. If the publish was lost, the archive sweeper
republishes it first
([#823](https://github.com/bricef/factor-q/issues/823),
[#946](https://github.com/bricef/factor-q/issues/946),
[#1070](https://github.com/bricef/factor-q/issues/1070)). Until it has, the control plane has not heard that the
run ended: its ownership row reads as it did before the ending, `in_flight`
for a run startup recovery restored and absent for a run started from a
trigger, while the worker's row is finished, so nothing resumes the run.
The sweeper's first pass, within `archive_retry_interval_ms` (10 s by
default) once the broker takes events again, or at the next start,
publishes it.

A row the sweeper cannot publish, it **holds**, for one of the three
reasons below. It publishes nothing more of the row's own, leaves the
row in `worker.db` exactly as it is, and carries on with every other
row. Until the hold is lifted, the control plane sees that invocation
as not archived. It does see it as finished, once the run's terminal
event is folded: the ownership row reads `completed` or `failed` with
its archive `pending`, `fq invocation list` shows `held` in its
`archive` column, and `fq invocation show` prints the reason and the
remedy.

When the hold is of the run's terminal event itself, because this build
will not republish it or the broker refuses it for its size, that event
never reaches the control plane. The sweeper then publishes
`invocation.ending_held` in its place: the run's outcome, when it
started and ended, and its model-call count, tool-call count and cost
as the worker's rows record them, with nothing copied from the stored
event
([#1071](https://github.com/bricef/factor-q/issues/1071)).
The coordination consumer folds it as the ending it stands for, so the
run reads finished and held like any other, and `fq invocation show`
lists the event among the run's recent events. The task status,
summary or error message are inside the stored event only, so `show`
has none of them, and the readers that key on a run's own `completed`
or `failed`, such as the summariser's outcome line, do not read the
held ending yet
([#1100](https://github.com/bricef/factor-q/issues/1100)). The daemon log says `published the held row's
ending, so the control plane reads the run as ended` once per daemon
run for each such row. No cost report counts the event: every model
call's cost is already on record from its own `llm.response` or
`llm.failure`.

Two kinds of held run still do not read finished. A row whose ids
cannot go on an event can publish nothing at all, this event included,
so its ownership row stays as it was, absent or `in_flight`. So does a
run whose terminal event went out, and whose archive was then held,
before the daemon was upgraded to a build that moves the row on a run's
ending: the older build consumed that ending without moving the row,
and it is not folded again. `fq doctor` lists every held row whatever
its ownership row says. The daemon log reports a held row once per
daemon run, at error level, and `fq doctor` and `fq status` list every
held row with its invocation, the reason and the remedy. `fq doctor`
counts each one as an issue, because none lifts on its own:

```text
Archive holds: 1 — finished runs the archive sweeper will not archive until an operator acts; the control plane sees each as not archived
  019a7c2e-5b1d-7f30-9a4e-2c8d1e6f0a42 (researcher): `invocation_archived` is 1252461 bytes, over the broker's max_payload of 1048576
  -> raise the broker's max_payload above 1252461 bytes, then restart fqd: it reads the limit when it connects, and offers the row again at start (docs/guide/operating-the-daemon.md, "When the archive sweeper holds a terminal row")
```

The list is the daemon's memory of its own sweeps, so it is not known
until the sweeper has finished its first pass after a start, one
`archive_retry_interval_ms` (10 s by default) after it. Until then both
verbs say `Archive holds: not yet known` rather than none, and in
`--json` the `archive_sweep` block has no `last_pass_at_ms`. `fq doctor`
does not count not yet known as an issue, so a `fq doctor
--fail-on-issues` straight after a restart passes before any hold can
have been found; wait one interval if the gate is meant to catch them. A
row held by an earlier run is offered again when the daemon starts, and
is listed again once that first pass has held it again.

**An event too large for the broker.** The broker refuses any message
larger than its `max_payload`, 1 MiB on a stock NATS server, and the
daemon checks before it sends. `invocation.archived` carries the run's
whole state: the conversation, every tool result in it included, as
base64 text, four bytes on the wire for every three bytes of state. A
state of more than 785,949 bytes makes the archive larger than a stock
broker's 1 MiB, and one of more than 12,582,429 bytes larger than the
dogfood broker's 16 MiB. Both figures count the message header the
broker counts, and are for an agent id of eleven characters; a longer
id lowers them slightly. In tool output that means a run that read two
results at the default 256 KiB cap archives on a stock broker and one
that read three does not (see
[When a tool returns too much](#when-a-tool-returns-too-much)). The
stored terminal event can be refused the same way, and then the
sweeper publishes `invocation.ending_held` in its place, as above. When
the limit is raised and the stored event goes out, it folds as the
same ending. The sweeper used to
stop its pass at such a row, on every pass, so no row behind it was archived
either ([#981](https://github.com/bricef/factor-q/issues/981)). Now it
holds the row, and the log line is `an event this row must publish is
larger than the broker's max_payload; holding the row unarchived until it
fits`, with the `invocation_id`, the `event_type`, its `size_bytes` and
the `limit_bytes`.

To release it, raise the broker's `max_payload` above the size the line
reports and restart `fqd`. The size is what the broker counts: the
event's JSON and the message-id header the daemon sends with it
([#828](https://github.com/bricef/factor-q/issues/828)), so a limit at
or above it takes the event. The daemon reads the broker's limit once,
when it connects, so a daemon that is already running keeps refusing at
the old limit. The restart is also what offers the row again: a held row
is not retried during a run, because the same event would be refused the
same way, but a new run's sweeper tries every row once. With its other
settings at their defaults, `nats-server` will not take a `max_payload`
above 64 MiB, because the limit may not exceed `max_pending`, which is
64 MiB by default; going higher means raising that too. The dogfood
broker runs at 16 MiB (`ops/dogfood/infra/nats.conf`). A row held by a
build from before the archive's state became base64, which wrote it as a
JSON array of integers at about four bytes for each byte, may fit after
an upgrade without any change to the broker: the same archive is about
three times smaller, and the upgraded daemon offers the row again when
it starts. There is no release that archives the row without its
state: until it fits, it stays held, and it costs its space in
`worker.db`.

**A stored event this build will not republish.** The sweeper will not
republish a stored event that this build cannot read, or one that could
not be that row's outcome. The case to expect is an event an older
daemon stored and had not yet published when you upgraded across an
event-schema bump. Republished, it would halt every durable consumer on
the stream (see
[When a consumer halts on an event it cannot read](#when-a-consumer-halts-on-an-event-it-cannot-read)).
So the sweeper holds the row instead, publishing neither the stored
event nor `invocation.archived`, but `invocation.ending_held` in its
place, so the control plane reads the run as ended. The log line is
`stored terminal event cannot be republished; holding the row
unarchived until an operator releases it`, with the `invocation_id` and
the reason (for example, the `schema_version` it found and the versions
this build reads).

Nothing releases such a row on its own. No later build reads an older
event version, so the hold lasts until you release the row. Releasing
archives the invocation **without** its outcome, so keep a copy of the
stored event first, because it is the only copy of the run's task status,
summary and cost (or, for a `failed`, of why it failed and what it had
spent):

```sh
db=~/.cache/factor-q/worker.db   # the `worker db:` line fqd prints at startup
id=<invocation-id>               # from the error line

# Keep the outcome: this is the only copy.
sqlite3 -cmd '.timeout 5000' "$db" \
  "SELECT terminal_outcome FROM invocation_state WHERE invocation_id = '$id'" \
  > "held-$id.json"

# Release the row: the next sweep archives it without its outcome.
sqlite3 -cmd '.timeout 5000' "$db" \
  "UPDATE invocation_state SET terminal_outcome = NULL WHERE invocation_id = '$id'"
```

The sweeper reads every row afresh on each pass (every 10 s by default),
so the next pass archives the released row, and the control plane's ack
deletes it as usual. The ownership row keeps the ending the held ending
gave it and records the archive.

**A row whose ids cannot go on an event.** Every event names its agent
and its invocation, so nothing can be published for a row whose
`agent_id` is not a valid agent id (non-empty, with no dots, wildcards
or whitespace) or whose `invocation_id` is not a UUID. The runtime never
writes either, so such a row was edited by hand or written by a build
with other rules. It used to end the sweeper's pass on every pass, as an
oversized archive did; now it is held, and the log line is `this row's
ids cannot go on an event; holding the row unarchived until an operator
corrects it`, with the `invocation_id` and a reason that names the field
and its value. Keep a copy of the row, then correct the field in
`worker.db` with `sqlite3`, as above; the next pass archives the row.

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
  watcher's retry count never moves. A plain `fq invocation drop` ends a
  deferred invocation instead: it is not resumed, and its slot is freed
  at once (see
  [Triaging unresolved invocations](#triaging-unresolved-invocations)).

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
slot rather than taking a new one. An operator's drop is such an end:
the slot goes back when the drop goes through. So a persistently throttled model
cannot turn a capped agent's queue into a burst: whatever the pause
does to the timing, a deferral and its resume never take that agent past
`max_concurrent`. (An operator's `fq invocation resume` can: see
"An operator's resume is not held at its agent's cap" below.)

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

**Every run takes a worker permit, resumes included.**
`max_concurrent_invocations` bounds everything the daemon runs, not only
what the dispatcher pulls: a deferred invocation's resume, the resumes
startup recovery queues, and `fq invocation resume` all wait for a
permit in the same queue as triggers
([#994](https://github.com/bricef/factor-q/issues/994)). After a
restart, recovery queues every invocation it found before the dispatcher
pulls its first trigger, so they go first: the first
`max_concurrent_invocations` of them resume at once, the rest wait in
turn, and a new trigger waits behind all of them. At the default cap of
1 that also means a resumed run and a new one never share the one
workspace directory at the same time.

A resume that has to wait is not silent. The boot banner counts the
recovered ones (`resumes waiting:  N for a worker permit`), and the
daemon log names each one by `invocation_id`, with `resume=recovered` or
`resume=operator`, at each of these points:

- when an operator's resume is accepted (`resume accepted; it is
  running`, or `… so it waits for one`);
- when a resume starts waiting (`worker permits exhausted; the resume
  waits for one`);
- when it gets its permit;
- if a drain or stop comes first (`left in flight for the next start's
  recovery`);
- if an operator drops it first (`it is not resumed`).

No other surface yet tells a waiting run from a running one, and the two
kinds show differently:

- **A recovered run** is in flight everywhere. Recovery restored its
  ownership row and its WAL row is in flight, so `fq invocation list
  --status=in_flight`, `fq invocation show` and `fq doctor`'s in-flight
  count all show it. Its last step is from before the restart, so if it
  waits past the stuck threshold `fq doctor` counts it stuck and
  `invocation.stuck` is published once, exactly as for a deferred
  invocation that waits that long.
- **A run `fq invocation resume` queued** still shows as `ambiguous`:
  under `fq invocation list --status=ambiguous`, in `fq invocation show`,
  and among `fq doctor`'s ambiguous invocations. That lasts for the whole
  wait, and the whole run, until it ends, because nothing moves its
  ownership row on a resume yet. `fq doctor` counts it in flight as well.
  Resuming it again is refused as `not Ambiguous (it may already have
  been resumed)`, which is the truth: it has been.

Either way, while it waits it counts against its agent's
`max_concurrent`, so that agent's triggers are held behind it. And
either way it is not being driven, so a plain `fq invocation drop` takes
it (see [Triaging unresolved invocations](#triaging-unresolved-invocations)).

**An operator's resume is not held at its agent's cap.** It counts
against `max_concurrent` from the moment it is accepted, but the only
thing it waits for is a worker permit, never a free slot for its agent.
An ambiguous invocation holds no slot while it waits for triage, so if
its agent is already running as many invocations as `max_concurrent`
allows, the resumed run starts beside them as soon as a worker permit is
free, and the agent is over its limit until one of its invocations ends.
That can be a long time: a slot is held for a whole run, through a
deferral too, so an agent at `max_concurrent: 1` with a long build can
run two builds side by side for the whole of the shorter one. Running
more than its limit at once needs `max_concurrent_invocations` above 1:
at the default of 1 the resumed run waits for the one worker permit, so
`fq doctor` can show the agent at `2/1` while only one of them runs.
`fq invocation resume` says this in its human-readable output when it
accepts. This is deliberate
([#1009](https://github.com/bricef/factor-q/issues/1009)): you asked for
that run by id, and holding it at the cap would mean giving it priority
over the agent's held triggers, because a cap hold is a race between
waiters, not a queue. If the limit matters for that agent, resume when
`fq doctor` does not list it under `Agents at cap`; otherwise accept the
overshoot. Startup recovery's resumes are counted the same way, but
before the dispatcher admits anything, so no trigger joins them past the
cap.

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

A chatty server is bounded by count as well, because every request it
gets answered is a model call on the agent's budget. `[worker]
max_server_requests_per_tool_call` (default 8) caps the sampling and
elicitation requests one tool call admits, and
`max_server_requests_per_invocation` (default 100) caps them across the
invocation, both counted together. A request over either is declined
before any model call — evaluators included — and the invocation
carries on: the server sees a decline, nothing fails, and the daemon
logs one warning per invocation, naming the setting, the first time a
cap refuses. Zero is refused at startup; to keep a server from asking at
all, leave sampling and elicitation ungranted in the agent definition.
The counts belong to one run of the invocation and are not carried
across a resume, which services no server requests at all
([#831](https://github.com/bricef/factor-q/issues/831)).

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

## When a tool returns too much

What a tool returns is recorded three times: written to the worker's
WAL, published as a `tool.result` event, and appended to the
conversation the model is sent again on every later turn. The broker
refuses any single message over its `max_payload`, 1 MiB on a stock
NATS server, so before results were capped one `file_read` of a large
log, or one MCP server returning a large document, left the invocation
unable to publish anything further
([#827](https://github.com/bricef/factor-q/issues/827)).

`[tools] max_result_bytes` (default 262144, which is 256 KiB) bounds
every tool's result at the one place all of them come back, before
anything records it, so the WAL, the event and the model all see the
same text. It applies to built-in and MCP tools alike, and to error
messages too. The number counts the whole result, the note that says
it was cut included. Longer output keeps its start, cut on a character
boundary, and ends with a note like this one:

```text
(truncated at the 256.0 KiB tool-result cap: kept the first 262031 of 52428800 bytes; the rest was dropped)
```

The number is the space the result takes in the event, not its length
as text, because that is what the broker measures. JSON writes most
text as it is, but a control character takes six bytes (`\u0000`) and
a quote, a backslash, a newline or a tab takes two. Ordinary text is
cut at the number; text full of control characters, quotes or
backslashes is cut shorter. A file that is mostly NUL bytes, such as
an uncompressed tar, keeps about a sixth of the number. The counts in
the note are bytes of text, which is what the model can reason about.

The note says what was dropped and offers no way to get it back,
because no tool can return the rest yet: an agent that needs a later
part of a large file has to ask for less, for instance with `exec` and
`tail_lines` where it holds `exec`. A ranged `file_read`
([#947](https://github.com/bricef/factor-q/issues/947)) and large
output stored by reference
([#948](https://github.com/bricef/factor-q/issues/948)) are the
follow-ups.

The default is exactly a quarter of a stock `max_payload`, so at most
three capped results fit in one `llm.request` alongside the rest of
the history. The cap bounds each result, not a turn: a turn of four
parallel `file_read` calls on large files still produces a request the
stock broker refuses. A value under 1024 is refused at startup.

An event the broker refuses anyway ends the invocation
([#828](https://github.com/bricef/factor-q/issues/828)): that turn's
request, a result under a cap set above the broker's `max_payload`, or
a `triggered` whose trigger payload and system prompt together pass it.
Nothing is truncated. The run ends with one `failed` of kind
`payload_too_large`, whose `detail` names the event refused, its size
on the wire and the limit.

Once the run has started, the worker's row is marked terminal and the
workspace reclaimed, so a restart has nothing to resume, and the
github-watcher does not retry the issue, since the same work would
build the same event. Before this the refusal left the row in flight,
and every restart resumed the run into the same refusal. A run that
ends this way on a resume, rather than live, is not reported
ambiguous: the resume returns an error, but recovery finds the run
ended, logs `resume returned an error, but the invocation has ended;
not reporting it ambiguous` at warn, and publishes nothing (see
[Triaging unresolved invocations](#triaging-unresolved-invocations)).

A refused `triggered` leaves no row and no `triggered` event, so the
`failed` is the only record, and it names the trigger in its
`trigger_id` and `trigger_subject` annotations. The dispatcher consumes
the trigger. The github-watcher binds a run to its issue through
`triggered`, so it never sees this failure: the issue stays in progress
until the watcher's reconcile timeout re-queues it, without counting an
attempt, so this repeats until the trigger, the agent's prompt or the
broker's limit changes.

The default also sits above `exec`'s own cap of 100 KiB per stream,
whose note names `max_lines` and `tail_lines`, so ordinary `exec`
output is cut there, once. Output with many invalid bytes or control
characters is the exception: `exec` caps raw bytes, and each invalid
byte renders as a three-byte replacement character, so that output
can exceed the runtime cap and be cut again from the end, losing
`exec`'s note and the stderr section
([see #985](https://github.com/bricef/factor-q/issues/985)).

One event is outside the cap. `invocation.archived` carries the
reducer's state blob, which holds the whole conversation, as base64
text: four bytes on the wire for every three bytes of state
([#981](https://github.com/bricef/factor-q/issues/981); before that it
was a JSON array of integers, about four bytes for each one). A state
blob of more than about 786,000 bytes therefore makes the archive
event larger than a stock 1 MiB `max_payload`, and more than about
12,580,000 bytes makes it larger than the dogfood broker's 16 MiB. A
run that read three results at the default cap cannot archive on a
stock broker, since base64 makes them a whole 1 MiB before the rest of
the conversation is counted, and a run that read two is most
of the way there. When the archive is over the limit, the invocation
completes, the archive publish fails, and the archive sweeper holds that row
unarchived, lists it in `fq doctor` and `fq status`, and goes on to the
rows behind it (see
[When the archive sweeper holds a terminal row](#when-the-archive-sweeper-holds-a-terminal-row)).
A broker serving agents that read large outputs still needs a
`max_payload` above the stock 1 MiB
([see #981](https://github.com/bricef/factor-q/issues/981)).

A run that ended because its `llm.request` was refused is held this
way too. Its state holds the messages that request carried, and as
base64 they take four bytes for every three, so the archive is over
the same limit unless the rest of the request, its tool definitions
mostly, took more than a quarter of it. A run that ended at a refused
`tool.result` or terminal event does not hold that event in its state,
so it archives unless the conversation before it was already too
large. Such a run reads `failed` in `fq invocation list` once its
`failed` is folded, with `held` in its `archive` column, and `fq
invocation show` and `fq doctor` give the reason and the remedy
([#1024](https://github.com/bricef/factor-q/issues/1024)).

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

**After a restart, a delivery the stopped daemon left unacknowledged
comes back only when its ack window lapses.** A daemon often stops with
one outstanding: it died handling it, or was stopped while it was on its
way. The projector and the coordination consumer take one delivery at a
time, so nothing behind that one is folded until then, and a gated read
waits with them (`fq invocation drop` reads its drop back that way). The
coordination consumer's window is five seconds
([#1070](https://github.com/bricef/factor-q/issues/1070)); the others
take `[bus] ack_wait_ms`, 30 seconds by default.

The short window has a cost when the coordination consumer's store is
slow. One fold that takes longer than five seconds is harmless: its
event is delivered again and folded twice, which leaves the records as
once. But while every fold takes longer than that, as on a slow or
contended disk, each event is handled once more than the one before it
(three events took nine handlings), and the control plane's records fall
further behind the event stream for as long as it lasts. A fold that
fails rather than runs slowly is retried with a growing delay instead.
The cure is tracked in
[#1090](https://github.com/bricef/factor-q/issues/1090).

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

A halted `fq-coordination` holds startup recovery too: each start stops
waiting for it as soon as it halts, and resumes none of the runs it would
have resumed until a build that reads the event has folded past it (see
[When startup recovery holds a run](#when-startup-recovery-holds-a-run)).

**Malformed is a different fact, and reported separately.** Bytes that
are not an event in any version — JSON that does not parse, or a
supported version whose body does not match its shape — are logged,
acked and skipped, as they always were, and now counted: the line reads
`fq-projector: ok (pending 0, 3 malformed acked)`, and `fq status` shows
`malformed acked: 3` under the consumer. A non-zero count is not an
issue on its own; it says poison was skipped, not that history was
lost. A halt and a malformed count never read as one thing, because
they call for opposite responses.

**The dispatcher counts the copies of a trigger that meet a claim
already taken, three kinds apart.** It arbitrates every delivery against
the trigger's claim (see
[Which trigger started a run](#which-trigger-started-a-run-and-claimed-deliveries-that-never-settled)),
and a copy the claim refuses starts nothing: the count and its warn
line in the daemon log are all it leaves. `fq doctor` names the counts
on the dispatcher's line, and `fq status` prints each on a line of its
own under the consumer. The two kinds of duplicate are named by when
the copy arrived, against the start recorded for its run. (On the same
page "held" means a trigger waiting in an admission hold, which is
something else.)

```text
  fq-dispatcher: ok (pending 0, duplicates dropped since this daemon
     started: 5 after their run started, 2 before a start was recorded,
     abandoned claims taken again since this daemon started: 1)
  -> "before a start was recorded" is a copy that met a claim this
     daemon holds with no start on it and was left unacked: usually a
     copy of the trigger was still handling the claim and settles it
     itself; otherwise the claim could not be settled from the records
     just then (another copy got to it first, or a read or the re-take
     failed) and a later copy asks again; the daemon log has a warn line
     for each with its subject, sequence, attempt, the claim's age and
     why, and such a claim whose run is on record is listed under
     "Stranded trigger claims" below
  -> "taken again" counts claims this daemon held that a copy took
     again, under a new invocation id, to run the trigger: no copy was
     handling the claim any more and its run had no start on record,
     because the copy that took it ended before its first WAL write
     without settling the delivery (its task died; giving the claim back
     failed or was given up on before a NAK, or before an ack that was
     then lost; or a drain left it); the copy then runs the trigger unless
     a drain or the
     trigger's own checks stop it first; the daemon log has a warn line
     for each naming the abandoned invocation id, and the failure before
     it
```

- **After their run started.** The run's first durable write is on
  record, so the copy is acked and dropped: a redelivery that crossed
  the run's start, the redelivery of a run a previous process began and
  this one's recovery resumed, or a replay of the stream. Most such
  copies find the start recorded on the claim. A copy that meets a claim
  this daemon holds, with no start on it and no copy of the trigger
  handling it any more, finds the start in the run's own records
  instead, the worker's WAL or the control plane's ownership table: the
  run's start was refused on every attempt and the ack the daemon sent
  in its place was lost (the run itself carries on; see "Started, never
  marked" below), or the run's task panicked in the step that made its
  first durable write. A copy stops handling its claim once its start is
  marked or its mark has given up, so this holds while the run is still
  going as well as after it ends. That copy is acked too, the claim is
  marked, and the trigger is not run again
  ([#967](https://github.com/bricef/factor-q/issues/967)). If the lost
  answer was the trigger's last delivery's, no copy comes back to be
  acked, and the dead-letter watch settles it instead: see below.
- **Before a start was recorded.** The claim is this daemon's, records
  no start, and a copy of the trigger is still handling it, so the copy
  is dropped and left unacked for that one. That first copy may be
  waiting behind a paused model, a full agent or the worker cap, where
  an in-progress ack every 250 ms keeps it alive and it is redelivered
  only if those stop landing for a whole ack window. It may be starting:
  from its worker permit to its first durable write (its workspace, its
  MCP servers, its first step) the same in-progress acks keep it alive,
  so a slow start is not redelivered either unless they stop landing. Or
  it may be marking its start, which the same in-progress acks cover. Or
  it may be settling the delivery, from its run's return or its refusal
  until its NAK or ack is published: giving its claim back and, on a
  fifth and last delivery, publishing its dead letter first. The same
  in-progress acks cover that too
  ([#1057](https://github.com/bricef/factor-q/issues/1057)). Either way
  the trigger runs at most once, and that copy settles the delivery
  itself. A
  copy is counted here too when no copy is handling the claim but it
  cannot be settled from the records just then, and is left for a later
  copy: another copy that met the same claim settled or took it first,
  or reading the records, or taking the claim again, failed (the worker
  or the claim store erred).
- **Taken again.** The claim is this daemon's, records no start, no copy
  of the trigger is handling it any more, and nothing of its run is on
  record: the copy that took it ended before its run's first durable
  write without settling the delivery, because its task panicked, or
  because giving the claim back before a retry failed, or was given up
  on after 40 seconds and had not landed by then. The copy takes
  the claim again under a new invocation id, by a compare-and-set on the
  old one, to run the trigger once. The count is of claims taken again,
  not of runs: the copy then runs the trigger unless a drain under way
  stops it first, which leaves the claim for the next binary again, or
  the trigger is refused after its claim as any delivery would be (its
  agent no longer loaded, a payload that does not parse). It is not a
  duplicate, and not an issue. A claim a drain left behind is taken
  again the same way, if a copy reaches the daemon before it exits. One
  double fault lands here too: a run that failed for good before its
  first durable write, whose claim could not be given back, or was
  given up on, and whose ack was lost, is run once more.

The warn lines are `dropping duplicate trigger delivery after durable
start`, `acking a redelivered trigger whose claim this daemon holds`,
`dropping a redelivered trigger whose claim this daemon already holds`
and `taking again a trigger claim this daemon holds`. All carry the
stream sequence and the attempt (`delivered`). The last three add the
subject, which names the agent, the claim's age (`claim_age_ms`) and the
invocation id the claim names: `claim_invocation_id`, or, on a re-take,
`abandoned_invocation_id` beside the new `invocation_id`. The failure
before a re-take is logged at error: `failed to release unfinished
trigger claim`, with the error, `stream_seq` and `invocation_id`, or,
when settling stopped waiting for the release after 40 seconds and
answered the delivery without it, `gave up waiting for the claim store
to release a trigger's claim`, with `stream_seq`, `invocation_id`, the
subject, `trigger_id`, what the delivery was being answered for
(`context`) and how long it waited (`waited_ms`); a task that died, or a
drain, leaves no such line. Either line's `invocation_id` is the
re-take's `abandoned_invocation_id`. A release given up on can still
land after the re-take, and then removes nothing: a release removes only
the claim of the invocation giving it back
([#1061](https://github.com/bricef/factor-q/issues/1061)); nothing
watches a release given up on, so its landing is not logged. A release
settling waits out to its end that finds no claim of its own to give
back is logged at warn, `released no trigger claim`, with `stream_seq`,
`invocation_id`, the subject, `trigger_id` and `context`, as the NAK or
ack after it carries them, and the claim as it stands now:
`claim_state`, `claimant` and `claim_invocation_id`. Every release
answers a delivery whose run has not started, and the copy giving it
back holds its claim, so the line means something changed the claim
underneath it; the ack after a run's start is marked keeps the claim
and gives nothing back
([#1055](https://github.com/bricef/factor-q/issues/1055)). The claim's
fields say what changed it. `durably_started`: the dead-letter watch or
a requeue found a run under `claim_invocation_id` in the worker's WAL
and marked the claim, so check that run with `fq invocation show`.
`claimed` under another claimant or invocation: a second process is
claiming from the same store, which this design does not support. `gone`:
something deleted the row, which no part of a running daemon does
before the claim window has passed, so a hand edit. The `invocation_id`
itself never started, so `fq invocation show` finds nothing under it.
The
drop says
why it dropped the copy, and on the trigger's fifth and last delivery
it says that no copy follows. A drop before a start was recorded costs
the trigger one of its five deliveries. The same sequence dropped again
on a later delivery means its first copy is still waiting or starting
and its in-progress acks are not landing, or that the run's records
still cannot be read, and each line says which; a trigger that runs out
of deliveries that way is dead-lettered. If the claim's invocation id
has a run (`fq invocation show <id>`), that run started and its start
has not reached the claim yet, and the stranded-claims block of
`fq doctor` lists it as started and never marked once a minute has
passed. None of the counts is a doctor issue. All are kept in the
daemon's memory and start again at zero when it restarts; the log lines
are the lasting record.

One more refusal is logged and counted in none of these: a delivery that
meets a claim left by a process that has exited, and finds the claim changed
as it adopts it (`the claim it met changed before this daemon could
adopt it`). The same fields name it. One daemon has nothing that
changes such a row in that moment, so the line is not expected.

**A last delivery whose dead letter cannot be published goes to the
watch.** On a trigger's fifth and last delivery, a run that fails
transiently before its first durable write is dead-lettered by the
dispatcher itself: it publishes the `trigger_exhausted` dead letter,
gives the claim back and acks the delivery, and the ack keeps the broker
from reporting the trigger exhausted. When that publish fails, because
the events stream is unavailable or does not acknowledge it within five
seconds, the dispatcher does not ack. It gives the claim back and NAKs
the delivery with no delay, and the broker, having no delivery left to
make, reports the trigger exhausted as soon as the dispatcher's consumer
next asks for a message. While the dispatcher is consuming it keeps such
a request waiting, bar the instants between one and the next and the
half second it pauses after a failed one, so that is at once. A daemon
that drains or shuts down while a last delivery is being settled has
stopped asking: its NAK lands with no request waiting, or only the
loop's last one until that expires, and the broker reports the trigger
at the next start's first pull. Nothing is lost either way. The
dead-letter watch (below) finds nothing on record for the trigger and
writes the dead letter, trying again on the bus's escalating delay (one
second, then two, doubling up to a minute by default) until the events
stream takes it ([#1062](https://github.com/bricef/factor-q/issues/1062)).
A daemon that stops before the watch has written the dead letter leaves
the advisory in its stream, which keeps it for a day, for the next
start's watch. If the events stream stays unusable for longer than that
day, the advisory ages out and the trigger is left with no dead letter.

The dispatcher logs `could not publish the dead letter of a trigger whose
last delivery failed` once, at error, with the agent, the subject, the
sequence (`stream_seq`), `trigger_id`, the attempt, why the publish
failed (`error`) and the error that ended the delivery (`last_error`).
That line is the one record of the last error that names the trigger
(the run's own error lines, `executor returned an error for
NATS-triggered run` among them, name no trigger): the watch's dead
letter says only that it was surfaced from the MAX_DELIVERIES advisory.
Match the two by the sequence: the line's `stream_seq` is the dead
letter's `seq=`. The line's `trigger_id` is also the dead letter's
trigger id when the trigger was published with one. A trigger published
without one, as `fq-cron` publishes them
([#1080](https://github.com/bricef/factor-q/issues/1080)), is given an
id by the dispatcher for each delivery; the line carries it and no dead
letter does. The watch does not invent one, so its dead letter carries
no trigger id, `fq dead-letters requeue` refuses it, and the way to run
the work again is as new work, `fq trigger <agent> '<payload>'` with the
payload the dead letter records. Had the dispatcher's own publish
succeeded, its dead letter would have carried the id and could have been
requeued.

While the events stream refuses the watch's dead letter, nothing in
`fq status` or `fq doctor` names the trigger: `fq-advisory-watch` has
the advisory in hand, and shows as stuck once it has been redelivered at
least `[bus] stuck_after_redeliveries` times, and the watch logs
`advisory handling failed; nak for retry` at warn. After the hand-over,
`fq-dispatcher` itself can read as stuck in `fq status` and `fq doctor`
while its next trigger is in hand, although it is consuming: the rule
counts the last delivery, which was never acked, until an ack with
nothing else pending moves the consumer's floor. The same follows any
trigger whose deliveries ran out through the broker's advisory
([#1079](https://github.com/bricef/factor-q/issues/1079)).

**A trigger can get two dead letters.** If the dispatcher's publish did
reach the events stream and only its acknowledgement was lost, the watch
finds that dead letter among the agent's failures since the trigger was
published, however many came in between, and writes no second one,
logging `a dead letter naming this trigger is already on the log` at
info; a read it cannot make decides again, as below. It writes a second
one, under the same sequence, when the dispatcher's publish reached the
stream only after the watch had looked, or when its check could not be
made by its limit. The same holds, as before, for a dead letter the
dispatcher published whose delivery's ack never reached the broker.
`fq dead-letters list` shows both, and `fq doctor` counts both; what a
requeue then does is below
([#1091](https://github.com/bricef/factor-q/issues/1091)).

**The dead-letter watch decides by the records, and counts the triggers
it keeps.** When a trigger's fifth and last delivery lapses with no
answer reaching the broker, or is handed over as above, the broker
reports the trigger exhausted and `fq-advisory-watch` decides whether
to dead-letter it. It looks at the trigger's claim, and through it at
what is on record for the run: the claim marked started, or the run in
the worker's WAL or on the control plane's ownership table. With
nothing on record it writes the
`trigger_exhausted` dead letter as it always has. With a start on
record it writes none, since requeuing that dead letter would run the
work twice: it marks the claim, logs `not dead-lettering a trigger
whose deliveries ran out` at warn with `consumer=fq-advisory-watch`,
the subject (empty when the trigger's message cannot be read), the
sequence, the invocation, where the start is on record and what became
of the claim (`claim=marked started now`, `already marked started`, or
`not marked` with why), and counts it on its own line
([#967](https://github.com/bricef/factor-q/issues/967)). The watch asks
the records before anything else, so this holds even when a dead letter
for the trigger is already on the log, written before the start was on
record: it stays there, and the line says one may be.
The count moves once per verdict, so an advisory whose ack was lost
comes back and counts its trigger twice. A run no task is driving,
because its last delivery's task died after its first WAL write, stays
in flight until this daemon next starts and resumes it, and shows
meanwhile under `Current executions` as stuck.

```text
  fq-advisory-watch: ok (pending 0, exhausted but started since this
     daemon started: 1)
  -> "exhausted but started" is a trigger whose deliveries ran out with
     no answer reaching the broker while its run had a start on record
     (its last delivery's task died after its first WAL write, or its
     ack was lost): no `trigger_exhausted` was written, since requeuing
     one would run the work twice; a run no task is driving stays in
     flight until this daemon next starts and resumes it, and shows
     meanwhile under `Current executions` as stuck; the daemon log has a
     warn line for each verdict naming the invocation, where its start
     is on record and what became of its claim
```

Three cases are neither, and the watch holds its verdict on them,
NAKing the advisory to decide again when it comes back: the claim store
or the worker's WAL cannot be read, so a dead letter now could be false;
a copy of the trigger is still handling its claim, so its run may still
start, and the verdict waits until that copy's start is marked or it
ends; or the claim changed between the watch's two reads of it (marked
started, given back, or taken again under another id). On the last, the
watch reads the claim once more, and a start it finds is a verdict: the
claim marked started, or a run it had already read in the WAL. A start
the watch has found is never held. A copy's delivery is renewed until
its start is marked or, if it does not start, until the delivery is
answered, so a copy is still handling a claim when its deliveries run
out only if its renewals stopped before either: a drain or shutdown let
it go while it waited or set up, its renewals did not land, or its setup
blocked its thread. A setup that waits, such as one
hung on an MCP server
([#1040](https://github.com/bricef/factor-q/issues/1040)), is kept
alive, so its deliveries do not run out and the watch never sees it.
The watch logs `holding the dead-letter verdict on an exhausted
trigger` at warn with `consumer=fq-advisory-watch`, the trigger's
sequence and subject (empty when its message cannot be read), the
reason and the invocation the claim names:
on each held advisory's first delivery, and after that on the
escalation this section describes, which all the watch's advisories
share. A verdict held for long shows `fq-advisory-watch` as
redelivering, and then as stuck.

**A held verdict ends within about an hour.** The advisory stream keeps
an advisory for a day, and one dropped while held would leave no dead
letter at all, so once the advisory is an hour old, and the watch has
looked at it at least three times, the watch takes a last look: the
verdict again and, if that still cannot be taken, the worker's WAL read
once for the run the claim names. A start found is a verdict as always;
an empty read is not taken for "never started", since a copy still
handling the claim can make its first WAL write just after it. If the
last look decides nothing, the watch writes the dead letter anyway,
saying that the trigger's start could not be checked and why: the claim
could not be read; a copy of the trigger is still handling its claim;
the worker could not say whether its WAL holds the run; the claim could
not be read with its ownership row; or the claim changed while the
run's records were read. The trigger's message ages out of its stream a
day after its publish, before the advisory does, so the dead letter
carries the trigger's subject, payload and id only if the deliveries ran
out within about 23 hours of the publish; otherwise it is written for
the system with none of them, and `fq dead-letters requeue` refuses it.
The watch logs it once at error (`dead-lettered an exhausted trigger
without checking whether its run started`) with the sequence, the
subject, the reason and the invocation, and `fq dead-letters list`
gives it lines of its own:

```text
  2026-11-17 09:12:40  researcher  seq=12 via advisory
      trigger exhausted after 5 deliveries (limit 5) — surfaced from the
      MAX_DELIVERIES advisory (consumer fq-dispatcher)
      payload: {"input":"hi"}
      start not checked: a copy of the trigger is still handling its
      claim; its run may have started
      -> `fq dead-letters requeue` checks the run's records first and
      refuses one that started; if it cannot tell, it refuses unless
      given --checked-not-started, which is for once
      `fq invocation show 019a…` shows the run did not start
```

Such a dead letter can be false: the run may have started already,
when the records could not be read, or a copy still handling the claim
may yet start it. `fq dead-letters requeue` does not take the mark's
word either way. Before it reserves or publishes anything, it asks the
records the watch asks, for this dead letter as for any other, marked
or not
([#1059](https://github.com/bricef/factor-q/issues/1059)):

- **A start on record is refused**, naming the invocation, whatever you
  pass. What is read: the trigger's claim, which may be marked durably
  started, and the worker's WAL and the control plane's ownership table
  for the run the claim names; and, when those cannot tell and the dead
  letter's mark names a run, the WAL and the ownership table for that
  run, by its own id, which is how a run that finished and was archived
  after the watch gave up is found once its claim is gone. Requeuing it
  would run the work twice. A start found on a claim that still reads
  `claimed` is marked on the claim, as startup recovery would mark it,
  and the refusal says so; one found by the run's own id marks nothing.
  That is also how a dead letter the watch wrote unmarked is caught when
  a copy it did not see, a replay of the stream, starts the run
  afterwards.
- **Nothing on record is requeued**, marked or not: the claim is still
  there, no copy of the trigger is handling it, and neither record holds
  the run. This is the check this section used to ask you to make by
  hand.
- **When it cannot tell, it refuses** and says why: the records cannot
  be read, a copy of the trigger is still handling its claim, or the
  claim changed while it was read, which can clear, so asking again
  later can let the daemon decide; or the dead letter is marked, no claim
  of the current trigger stream names its delivery any more, and nothing
  is found under the run its mark names. A claim given back before its
  run started leaves no row, and so does one the retention sweep deleted
  after its run started, so the two look the same; a dead letter written
  before the trigger stream was last created has no claim of this stream
  to consult either, since the claim the stream holds at its sequence, if
  any, is another trigger's. Which stream a dead letter belongs to is
  told by comparing its timestamp, this daemon's clock, with the
  stream's creation, the broker's, so a skew between the two clocks moves
  that line by as much
  ([#1077](https://github.com/bricef/factor-q/issues/1077)). A marked
  dead letter whose claim is given back while you wait is "cannot tell"
  again on the next ask. An unmarked dead letter whose claim is gone is
  requeued: its writer found nothing on record, and no delivery follows
  the last one. A run that started after such a dead letter was written
  is refused while its claim is kept and not after. With the defaults the
  dead letter is gone first: it is an event, kept for `[events] max_age`
  (30 days), and a claim is kept for
  `[state] trigger_claim_retention_days` (90 days); a claim window set
  below the event log's, or an event log kept longer than the claims,
  leaves that gap open
  ([#1076](https://github.com/bricef/factor-q/issues/1076)).

What still needs a person is the last case. The refusal names the
invocation to check. In `fq invocation show <id>`, the run started if
the output has any of these: a `Live execution:` block (the worker's
WAL holds the run, in flight or finished and not yet archived), an
`archived:` line, a `status:` other than `(no coordination row)`, or a
`trigger:` line saying `durably started`. A run still going in this
daemon has no coordination row, since ownership rows are written only
when a run ends (a `completed`, or a `failed` outside `setup`), turns
ambiguous, is archived or is dropped, and by startup recovery: for it the output says `status:   (no coordination row)` and
then shows the `Live execution:` block, so that status alone does not
mean the run never started. The run has not started if the command
answers `no invocation found with id=…`, or shows none of the four,
only a `trigger:` line saying `claimed, start not recorded` or
`none recorded`, and its events: its `triggered` event at most, and a
`failed` one if it was refused in setup. Then requeue it with
`--trigger-seq <N> --checked-not-started` (`checked_not_started` on
`dead_letter.requeue`, which needs `trigger_seq`); the refusal prints
that command. The flag is your word that the run of that one dead letter
did not start, which is why it needs the sequence: without one, a
requeue takes whichever dead letter is the agent's newest when it
arrives. It lets a requeue through only when the daemon cannot tell, and
never past a start on record. A copy still handling the claim can start
the run after you look. With no invocation named, no claim naming one
could be read and the dead letter's mark names none: list the
agent's runs with `fq events query --agent <agent> --event-type triggered`,
read each with `fq events get <event-id>` to find the one whose trigger
payload is this dead letter's, and check that run with
`fq invocation show <id>`. `fq doctor`'s `Current executions` only
counts the runs in flight in this daemon and names the stuck ones, so
it can say whether any run is going, not which; `fq invocation list`
reads only ownership rows and so misses a run still going. Without
`--trigger-seq`, `fq dead-letters requeue` takes the agent's newest
dead letter, and a refusal says which it took: its sequence and event
id, and that it was the newest.

The hour is measured from the advisory's own timestamp against this
daemon's clock, so a restart does not reset it; a daemon clock ahead of
the broker's ends the hold early by the difference, and one behind ends
it late. An advisory first seen after its hour, because the daemon was
down for longer, is still looked at three times, about three seconds, so
a reason that clears in a moment does not become an unchecked dead
letter. An advisory with no timestamp the watch can read is held only
for those three looks, since its age cannot be told, and is then
dead-lettered the same way. The hour is a trade between telling you
sooner and writing fewer of these dead letters, not a point after which
a held verdict cannot resolve: a copy still handling its claim can start
its run after it, which is why the dead letter is marked.

**A trigger whose message cannot be read is held too.** To name the dead
letter, the watch reads the trigger's message at the advisory's
sequence. If the broker answers that it holds no message there, which it
does for one that aged out a day after its publish or was deleted, and
also for one its store failed to load, which the watch cannot tell
apart, the dead letter is written for the system with no subject,
payload or trigger id, without holding for the message, and
`fq dead-letters list` says why: `names no trigger: the broker answered
that the trigger stream held no message at this sequence when the watch
last read it, at …, for an advisory emitted at …: the stream keeps a
message 24h after its publish, so it most likely aged out, or was
deleted sooner; the broker answers the same for a message it cannot
load`. The time is the read the dead letter was written on, which for a
verdict held on the records is the last of them. Every dead letter this
build's watch writes naming no trigger says why on such a line, and
`--json` gives its kind as `unattributed_kind`: `message_gone`,
`earlier_incarnation` or `message_unreadable`, or `unknown` to a client
older than the build that wrote it. One an older build wrote for a
trigger that had aged out carries no reason, and stays on the log for
the events stream's 30 days as a bare `system` row. Under a page whose
rows all name no trigger, the listing's last line says
`fq dead-letters requeue` refuses each. If the read fails for another
reason,
such as the broker not answering in time or the trigger stream being
missing, the message may yet be read, so the watch holds its verdict
and reads again when the advisory comes back. It asks the records
first, since they key on the advisory's sequence and need no message: a
run that started is found at once, as above, and a verdict the records
cannot give yet is held for their reason; only with nothing on record
does the unreadable message hold it. That hold line's `reason=` says
`the trigger's message could not be read` with what the read answered,
and its `subject=` is empty, since no message was read
([#1066](https://github.com/bricef/factor-q/issues/1066)). A missing
stream does not last: deleting the trigger stream normally stops the
daemon within a second, and its next start takes the advisory as an
earlier incarnation's. The hold ends as above, about an hour after the
advisory and after three looks: unless a last look at the records finds
the run started, the watch writes the dead letter for the system, and
`fq dead-letters list` gives it `names no trigger: the trigger's message
could not be read off the trigger stream by the watch's limit (the last
read answered: …), so the trigger is not named, though its message may
still be on the stream`, or, when the last read found the trigger stream
missing, `the trigger's message could not be read by the watch's limit:
the trigger stream was missing (…)`. `fq dead-letters requeue` refuses
it. An advisory first seen after its hour still gets its three looks,
about three seconds, or about half a minute when each read times out,
and is then written off this way. A verdict held on the records ends
instead in a dead letter marked `start not checked`, which names its
trigger when its message was read. When the records cannot be decided
and the message is gone or cannot be read either, one dead letter is
both, naming no trigger and marked unchecked, and the listing's check
says it cannot be requeued: with the run named, the `fq invocation
show` check for whether it started, and that a copy can still start it
after you look; with no run named, that if `fq doctor` lists a stranded
trigger claim at its `seq=`, that claim names the invocation id its
delivery reserved and says whether a run is on record.

**An advisory from an earlier incarnation of the trigger stream names no
trigger.** A recreated trigger stream numbers its messages from 1 again,
and the advisory stream keeps an advisory for a day, so the watch can be
handed an advisory the broker emitted before the stream was last
created. Its sequence names a message of that earlier stream, gone with
it, and the message the stream holds at the same sequence now is another
trigger. Before it reads anything, the watch compares the advisory's
timestamp with the creation of the stream the daemon connected to. For
an earlier incarnation's advisory it reads neither the message nor the
claim at that sequence, logs `an advisory from an earlier incarnation of
the trigger stream` at info, and writes the dead letter for the system
with no subject, payload or trigger id, as for a trigger that aged out
([#1054](https://github.com/bricef/factor-q/issues/1054)).
`fq dead-letters requeue` refuses it, and `fq dead-letters list` says why
it names nothing:

```text
  2026-11-17 09:12:40  system  seq=7 via advisory
      trigger exhausted after 5 deliveries (limit 5) — surfaced from the
      MAX_DELIVERIES advisory (consumer fq-dispatcher)
      payload: null
      names no trigger: its advisory came from an earlier incarnation of
      the trigger stream (emitted 2026-11-16T22:03:11Z, before the stream
      was created at 2026-11-17T08:00:00Z), so its sequence is that
      stream's, whose messages went with it; `fq dead-letters requeue`
      refuses it
      start not checked: its sequence is an earlier stream's, so the
      watch consulted no claim; its run may have started
```

Its `seq=` is the earlier stream's and says nothing about the trigger at
that sequence today, and since the claims were not consulted, the
trigger it records may have started its run before the stream was
recreated, as its `start not checked` line says. An advisory with no
timestamp the watch can read is taken as the current incarnation's,
though the broker always stamps one.

The watch learns the stream's incarnation when the daemon connects.
Deleting the trigger stream under a running daemon normally stops the
daemon within about a second, its dispatcher's consumer gone with the
stream, and its next start creates the stream again and learns the new
incarnation. A stream deleted between two of the dispatcher's pulls
instead leaves the daemon running and consuming nothing, logging
`error reading next JetStream trigger` at warn about every 35 seconds,
and the watch then attributes an advisory emitted before the recreate
to the message at its sequence in the new stream
([#1065](https://github.com/bricef/factor-q/issues/1065)).

**Every dead letter says which incarnation its trigger was on.** Since a
recreated trigger stream numbers its messages from 1 again, a trigger
sequence names one message only within one incarnation. Every dead
letter this build writes for a trigger of the stream the daemon is
connected to carries `trigger_stream_epoch`: that stream's creation
time, in nanoseconds since 1970, as the daemon read it when it
connected, the value its trigger claims key on. The dispatcher's own
dead letter carries it, and so does the watch's, including one for a
trigger whose message is gone or could not be read. It is absent from a
dead letter for an earlier incarnation's advisory, whose incarnation
the watch cannot know, and from one an older build wrote. An advisory
with no timestamp the watch can read is presumed the current
incarnation's, and its dead letter is stamped so. The value is the one
the daemon read when it connected, and it keeps it for its whole life:
a daemon left running over a deleted and recreated trigger stream (see
above, [#1065](https://github.com/bricef/factor-q/issues/1065)) stamps
the old stream's value on the dead letter of a trigger of the new one.
`fq events get <event-id> --json` shows it; `fq dead-letters list` does
not ([#1066](https://github.com/bricef/factor-q/issues/1066)).

**One exhaustion gets one dead letter, whichever form came first.**
Every dead letter the watch writes carries the broker's id for its
advisory: `fq events get <event-id> --json` shows it as the
`advisory_id` annotation, the event id being the `event_id` that
`fq dead-letters list --json` gives each row (the text listing does not
show it). Before writing one, the watch looks for one
already written for the same exhaustion
([#1066](https://github.com/bricef/factor-q/issues/1066)):

- **One it wrote itself since it started.** The watch remembers the
  advisories it has written a dead letter for, so a copy of one, such as
  a redelivery that waited in its buffer while it handled others, logs
  `this watch already wrote a dead letter for this advisory` at info
  and writes none, without reading the log. A dead letter that names
  the trigger may still follow a nameless one it wrote.
- **One that names its trigger.** It reads the trigger's agent's
  `failed` events since the trigger's message was published, for a dead
  letter at the trigger's sequence that records the incarnation of the
  stream the daemon is connected to (`trigger_stream_epoch`, above) and,
  when the trigger was published with an id, names that id. One that
  records no incarnation, written before dead letters recorded it, is
  never taken, so at worst a second is written. That finds the
  dispatcher's own dead letter (its publish landed and only its
  acknowledgement was lost) and one the watch wrote before it started
  again, however many other failures of the agent came in between. A
  trigger published without an id is matched by its sequence and
  incarnation alone, since the dispatcher gives such a trigger an id of
  its own for each delivery; before it trusts that, the watch asks the
  broker whether the trigger stream is still the incarnation it holds,
  for a daemon left running over a recreated stream holds the old one,
  and its dispatcher can write the old message's dead letter at a
  sequence the new stream reuses. That is one request, made only for
  such a match. So an earlier stream's dead letter at the same sequence
  is not taken for a new trigger's, with one exception: a match on the
  trigger's id is not asked about, so a daemon left running over a
  recreated stream takes an earlier stream's dead letter that names an
  id for that id published again at the same sequence of the new
  stream, and writes none. `fq dead-letters requeue` is keyed on the
  id, so the work stays requeueable from the earlier one. When it finds
  one, the watch logs `a dead letter naming this trigger is already on
  the log` at info and writes none.
- **One filed for the system**, naming no trigger in any of the forms
  above, or naming a trigger whose subject names no legal agent. It
  reads every agent's `failed` events since the advisory was emitted for
  one carrying the advisory's id: its own, and one that named the
  trigger, written before the trigger's message went. When it finds one,
  it logs `a dead letter was already written for this advisory` at info
  and writes none. An advisory with no id, or an empty one, cannot be
  told apart from another, so its dead letter is written every time and
  says its check could not be made (`duplicate_check`, below). A dead
  letter that
  names the trigger may still follow one written because the trigger's
  message could not be read, if the advisory comes back and the read
  then succeeds, since that is the one that can be requeued.

Wherever a check cannot be certain, the watch writes the dead letter: a
second one is the cheaper error, and no answer the checks give loses
one. Both reads pass over any event this build cannot read. Their cost
follows how many `failed` events came since the trigger's publish (that
agent's) or since the advisory (every agent's), at most the advisory
stream's day of them: a release build read 100,000 in about half a
second, and ran out of its ten seconds at two million. A read starts at
the subject's first event instead, costing more and answering the same,
for an advisory with no timestamp the watch can read and when the
client has to set up its read again before the first message. A backlog
of advisories met after a restart costs about the square of its size,
each read passing the dead letters written for those before it; nothing
is lost by it while the backlog clears within the advisory stream's
day, after which an advisory still waiting ages out with no dead letter
([#1087](https://github.com/bricef/factor-q/issues/1087)).
A read the watch cannot make, or one that takes longer than ten seconds,
leaves the advisory to be decided again: the watch logs `advisory
handling failed; nak for retry` at warn with `event log read error: …`
(for one out of time, `reading the log for a dead letter already
written took longer than 10s`), or with `trigger stream read error:
reading its creation: …` when the broker could not say which
incarnation the trigger stream is, and `fq-advisory-watch` shows as stuck
once the advisory has been redelivered `stuck_after_redeliveries` times.
Past the hour and three looks that end every held verdict, the dead
letter is written anyway, and says so: it carries a `duplicate_check`
annotation, `could not be made: …` with why, and its error line a
`duplicate_check=` field with the same text. `fq events get <event-id>
--json` shows the annotation; `fq dead-letters list` does not show it
yet. On seeing it there is nothing to recover: no dead letter was lost,
but this one may be a second for its exhaustion, one written before the
daemon last started or the dispatcher's. Look in `fq dead-letters list`
for another at the same `seq=`. For a trigger published with an id,
both carry it, and `fq dead-letters requeue` runs a trigger at most
once, so requeuing either does not run the work twice. For a trigger
published without one, the dispatcher's dead letter carries the id it
gave the trigger and the watch's carries none: `fq dead-letters requeue
--trigger-seq N` takes the most recent dead letter at that sequence,
the watch's, and refuses it, and nothing selects the dispatcher's
([#1091](https://github.com/bricef/factor-q/issues/1091)); publish the
work again as new work with `fq trigger`. A pair that names no trigger
cannot be requeued at all. The events stream keeps an event for 30 days
by default, longer than the advisory stream keeps an advisory; with
`[events] max_age` set below a day, a dead letter can age out of the
events stream before its advisory does, and a redelivery after that
writes it again.

No known path loses a dead letter to these checks. These cases still
write a second one
([#1091](https://github.com/bricef/factor-q/issues/1091)):

- **The dispatcher publishes its own dead letter after the watch has
  looked.** If the dispatcher's dead letter reaches the events stream
  only after the watch has read the log for it, both are written.
- **A nameless one after the dispatcher's own named one.** The
  dispatcher's dead letter carries no advisory id, so if the trigger's
  message is already gone when the watch first looks at its advisory
  (the watch down for about a day, or the message deleted), the watch
  adds a dead letter that names no trigger.
- **A check that cannot be made in time.** Past the hour and three
  looks the dead letter is written anyway, so one that a run of the
  daemon before this one wrote is written again.
- **An advisory with no id.** Its nameless dead letter is written each
  time the advisory comes back to a watch that does not remember it.
- **A daemon left running while its trigger stream is recreated**
  ([#1065](https://github.com/bricef/factor-q/issues/1065)). It goes on holding
  the earlier stream's incarnation. For a trigger published without an
  id whose dead letter is already on the log, stamped with that earlier
  incarnation, its watch writes a second; a watch started after the
  recreate takes neither for the trigger's and writes a third.
- **Clocks.** A skew between the servers' clocks, or the broker's clock
  stepping back, can make a dead letter look older than where a read
  starts.

What the checks take on trust is which message the advisory names: an
advisory with no timestamp the watch can read is taken as the current
incarnation's, and a daemon left running over a recreated stream reads
an earlier stream's advisory at its sequence in the new one (above,
[#1065](https://github.com/bricef/factor-q/issues/1065)). In both, the
check looks for the dead letter of the message it takes the advisory to
name.

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
`fq down` first, delete it, and start again. A reset `fq-coordination`
reads the whole stream again at that start, before recovery decides, so
the start is likely to hold its recovered runs, and the first start after
the replay has caught up resumes them (see
[When startup recovery holds a run](#when-startup-recovery-holds-a-run)).
The one exception is the
projector's: `fq projection rebuild` resets `fq-projector` under the
running daemon, because the daemon stops that consumer itself before
deleting the durable and starts it again after.

## Which trigger started a run, and claimed deliveries that never settled

Before the dispatcher does anything with a trigger delivery, it claims
it: a row in the control plane's `trigger_claim` table, keyed on the
message's place on the trigger stream (the stream, which incarnation of
it, and the sequence), naming the invocation id the run will have. The
claim is marked started when the run makes its first durable write, and
a redelivery of the same message that meets a started claim starts
nothing ([ADR-0032](../adrs/accepted/0032-exactly-once-trigger-dispatch.md)).
That row is the only record linking a delivery to the run it started.
The worker's WAL row is deleted once the run is archived, and the archive
row is swept after `[state] retention_days`; the claim row is kept for
`[state] trigger_claim_retention_days`, 90 days by default, or
indefinitely when that is negative (see
[How long a claim is kept](#how-long-a-claim-is-kept)).

`fq invocation show <id>` reads it:

```text
  trigger:  fq-triggers seq 1 (durably started; claimed 2026-01-02T03:04:04Z by 019a4c0e-…)
```

The claimant is the worker id of the daemon process that claimed the
delivery; every start mints a new one, so an id that is not the running
daemon's is a process that has exited. `--json` adds the stream's
incarnation as `stream_epoch`, its creation time in nanoseconds as a
decimal string: a recreated stream numbers its messages from 1 again, so
a sequence names a message only together with it. A finished, archived
run still shows its trigger.

A run no claim names reads `none recorded`, and that says less than it
seems. A delivery that ends before its run's first durable write gives
its claim back, yet the run may already have published events under the
claim's id: an agent refused because a shared MCP server is down
publishes `triggered` and `failed` and keeps no claim. An adoption also
writes the adopter's id over the one an exited process reserved, and the
retention sweep deletes a claim once it is older than its window. So the
line reads "not started from the trigger stream, its claim was released
or taken over before the run's first durable write, or the claim is
older than its retention window".

A run that did start from the trigger stream can be on no claim too,
and the line above does not name it. One daemon does not reach it: its
one route, a release that settling gave up waiting for, which stalled
about 70 seconds outside SQLite's lock waits and then landed after a
copy took the claim again, closed when a release was scoped to its own
invocation ([#1061](https://github.com/bricef/factor-q/issues/1061)).
When the dispatcher marks a run's start, the mark can find no
claim naming the run, because something changed the claim underneath
it. It then records the start by what the claim holds, acks the
delivery keeping the claim, and logs one line at error
([#1046](https://github.com/bricef/factor-q/issues/1046)):

- `the claim of a trigger whose run started was gone when its start was
  marked`: a claim recording the start was written, so the run has its
  claim.
- `the claim of a trigger whose run started named another invocation,
  not started`: the start was recorded on it in that invocation's place
  (`claim_invocation_id`), so the run has its claim and that invocation
  has none.
- `the claim of a trigger whose run started already records another
  run's start`: the trigger ran twice, and the claim is left naming the
  other run (`started_invocation_id`); this run is on no claim.
- `could not record the durable start of a trigger whose claim does not
  name its run`: the claim store refused the write on every attempt,
  with its last `error`, how many attempts it was offered (`attempts`)
  and over how long (`elapsed_ms`), and this run is on no claim. A replay
  of the trigger, or a copy a lost ack brings back, would start it
  again.
- `the claim of a trigger whose run started named the run on a second
  look`, at warn: the record found the claim naming the run after all,
  and marked it, so the run has its claim. Something changed the claim
  between the mark and the record; nothing in one daemon is expected to.

The record is offered again on the mark's own schedule, keeping the
delivery alive meanwhile. Each line carries `stream_seq`,
`invocation_id`, `trigger_id`, the subject and how many attempts at the
mark the store refused before the one that found no claim naming the
run (`refused_marks`); every line but the refused record's also says how
many attempts at the record it refused before the one that landed
(`refused_records`).
A mark refused and then finding no such claim is not logged as marked.

`fq invocation list` does not carry it. The claim table has no index on
the invocation id, so a lookup per listed row would read the whole table
(every claim inside the retention window) once per row; the link is read
one invocation at a time, newest claim first. There is no lookup from a
sequence to its run yet, other than for the claims `fq doctor` reports
below.

### Stranded claims in `fq doctor`

A claim normally settles within seconds: it is marked when the run
starts, or deleted when the delivery is acked or NAKed without starting.
`fq doctor` lists the claims that have done neither and will not without
help:

```text
Stranded trigger claims: 3 (2 issues) — claimed trigger deliveries that have not settled
  fq-triggers seq 1003: claimed 2026-11-10T09:12:00Z by 019a4c0e-…, a process that has exited; reserved invocation id 8b000000-…
  -> this delivery never started a run and will not be redelivered: its message is older than the stream keeps messages (1d)
  -> work requeued or published again runs under a new sequence this claim cannot show, so this line stays until the claim is 90d old
  fq-triggers seq 7 (an earlier incarnation of the stream, created 2026-01-02T03:03:05Z): claimed 2026-11-12T06:12:00Z by 019a4c0e-…, a process that has exited; reserved invocation id 9c000000-…
  -> this delivery never started a run and will not be redelivered: its message belonged to an earlier incarnation of the stream
  -> work requeued or published again runs under a new sequence this claim cannot show, so this line stays until the claim is 90d old
  fq-triggers seq 1002: claimed 2026-11-12T08:12:00Z by 019a4c0e-…, a process that has exited; reserved invocation id 7a000000-…
  -> not started; if its message is still deliverable, this daemon adopts it and starts a run when the broker redelivers it, which is due within 19m30s of this daemon starting
```

Each claim is judged by who holds it, whether its run is on record (a
WAL row or a coordination row for the invocation id it names), and
whether its message can still be delivered. A message is deliverable
when it is on the incarnation of the stream this daemon consumes and was
claimed more recently than the stream keeps messages (a day); a claim is
never older than its message, so an older claim names a message the
broker has dropped. The **redelivery window** is the sum of the trigger
retry ladder (19m30s today), counted from when this daemon started. The
**horizon** is `[state] trigger_claim_retention_days`, the age at which
the retention sweep deletes a claim an exited process left. There is
none when that setting is negative: claim rows are then kept
indefinitely, and a never-started claim's line says so where it would
name the horizon.

- **Not started, awaiting redelivery** (not an issue). A process that has
  exited claimed a deliverable message, nothing records a run, and this
  daemon has not yet run for the redelivery window. If the message is
  still there, the broker redelivers it and this daemon adopts the claim
  and starts a run. A doctor run straight after a restart can list some.
- **Never started** (an issue). This delivery never started a run and
  will not be redelivered: its message belonged to an earlier
  incarnation of the stream, is older than the stream keeps messages, or
  is deliverable and did not come back within the redelivery window, in
  which case its last delivery was most likely dead-lettered. The claim
  cannot see whether the same work ran under another delivery. Requeue
  it from `fq dead-letters list` if it is there: `fq dead-letters
  requeue` checks the run's records first, refuses a run that started,
  and when it cannot tell refuses until you have checked the run it
  names and pass `--checked-not-started`; a dead letter is
  requeued at most once (a second requeue answers with the trigger the
  first made), while a fresh `fq trigger` publishes new work every time,
  so check before publishing it again. Either way the work runs under a
  new sequence and a new claim, so this line stays until the claim is
  older than the horizon, and the report stops listing it then. The
  retention sweep deletes the row on its next tick after that.
- **Started, never marked** (an issue). The run the claim names is on
  record, the claim still says it has not started, and its message can
  still be redelivered. The usual cause is the claim store refusing to
  record the run's start. The dispatcher offers the mark again for up to
  two seconds (one attempt can take longer: it waits up to the store's
  five-second busy timeout for a lock, and longer still for a connection
  when every one is in use). When every attempt is refused it acks the
  delivery without releasing the claim and lets the run continue,
  logging `could not mark trigger durably started` at error with the
  invocation and the trigger
  ([#967](https://github.com/bricef/factor-q/issues/967)). The other
  cause is a run whose task panicked in the very step that made its
  first durable write, before the dispatcher saw it start: then nothing
  was marked and nothing acked.

  Either way the row lasts only until a redelivery reaches the running
  daemon or the daemon next starts. A redelivery comes when nothing
  acked the delivery or its ack was lost: it meets the claim with no
  copy of the trigger handling it, while the run is still going or after
  it ended, finds the run's start on record, and is acked and marks the
  claim without starting the work again, logging `acking a redelivered
  trigger whose claim this daemon holds` at warn and counted as a
  duplicate dropped after its run started.

  **No redelivery comes after the trigger's last delivery.** If the
  fifth delivery's answer never reaches the broker while its run has a
  start on record, nothing is left for the dispatcher to settle: that
  delivery started the run and its task then died, or its mark landed
  or gave up and its ack was lost, or it was a copy of a run started
  earlier and its ack was lost. Once that delivery's ack window lapses,
  the advisory watch looks at the claim and the run's records before it
  calls the trigger exhausted. It finds the start, marks the claim, and
  writes no `trigger_exhausted`, and counts it as exhausted but started
  (see [the dispatcher's counts](#when-a-consumer-stops-making-progress)).
  Before [#967](https://github.com/bricef/factor-q/issues/967)'s last
  change it wrote a false dead letter here; one already in
  `fq dead-letters list` for a run that `fq invocation show` says
  started is that, and requeuing it starts the work twice. So can be a
  dead letter the listing marks `start not checked`, which the watch
  writes when it could not take its verdict within an hour (see
  [the dispatcher's counts](#when-a-consumer-stops-making-progress)).
  `fq dead-letters requeue` refuses either while the run's claim is
  kept, and `--checked-not-started` is never for one.

  If no redelivery comes, startup recovery, before the dispatcher
  consumes anything,
  records the start on every `claimed` row whose run is on record, in
  the worker's WAL (in flight or finished) or in the control plane's
  ownership table, and logs `recorded a durable start missing from its
  trigger's claim` at warn for each one; so after a restart neither a
  redelivery nor a copy the stream replays starts the trigger again. A
  run whose task panicked stays in flight in the worker's WAL with
  nothing driving it until that start's recovery resumes it;
  `fq invocation resume` does not take it sooner, since it takes only a
  run recovery found ambiguous. The report cannot tell whether the
  delivery was acked, because it judges a message deliverable by its age
  and stream incarnation, not by the consumer's ack state; the daemon
  log can, by the lines above for that invocation. There is nothing to
  repair from the CLI: read the run with `fq invocation show`. If the
  daemon cannot read the run's records at any of its redeliveries, each
  is dropped as one before a start was recorded, with the read's error
  on its warn line, and once the deliveries run out the watch holds its
  verdict until the records can be read, rather than write a dead
  letter that could be false.
- **Cannot tell** (not an issue). The claim names no invocation, because
  a build from before claims named their run wrote it, so the report
  cannot look for a run.

The issues make `fq doctor --fail-on-issues` exit non-zero.

**What the check does not list.** A claim this daemon holds for a run
that has not started, however old: a trigger can wait behind a paused
model, a full agent or the worker cap for hours (`Throttled models` and
`Agents at cap` say why), and nothing on the claim records whether a
copy of the trigger is still handling it. The dispatcher knows that, in
memory, and the report does not ask. A claim no copy is handling any
more, left by a run that panicked before its first durable write or by
a release that failed, or was given up on and had not landed, before a
retry, looks the same to the report, and
is settled by the trigger's next delivery instead: the dispatcher takes
the claim again to run the trigger, counting it on the `fq-dispatcher`
line as an abandoned claim taken again and logging it with its subject,
its sequence, the claim's age and the abandoned invocation id (see
[the dispatcher's counts](#when-a-consumer-stops-making-progress)). If
that was the trigger's last delivery, no copy comes, and the trigger is
a `trigger_exhausted` dead letter to requeue. A release that failed,
or was given up on and never landed, before an ack leaves a claim no
delivery comes back to, since the ack consumed it. The check does not
list it while this daemon runs. After a restart it is a claim an exited
process left, with no run on record: listed as not started, awaiting
redelivery, until the redelivery window has passed, then as never
started, an issue, until the horizon, although its delivery was
consumed rather than dead-lettered. The release's error line in the
daemon log (above) names its sequence. A started run whose
message is gone,
since no redelivery can follow. And a claim an exited process left that
is older than the horizon.

**Where the window can be wrong.** It counts this daemon's time since it
started, not time spent consuming, and it reads the wall clock. A broker
outage longer than the window, or the clock stepping forward, can call a
deliverable claim never started before its redelivery came. A restart
also starts the window again, so a deliverable claim (one under a day
old) that was already reported never started reads as awaiting
redelivery again for the first window of every daemon run; a claim whose
message is gone is reported the same way whenever the daemon started.

### How long a claim is kept

A claim row does two jobs. Marked started, it is the record that stops
a redelivered or replayed copy of its trigger from starting the run a
second time. And it is the only record of which delivery started which
run: the `trigger:` line of `fq invocation show`, and the claims
`fq doctor` reports. The daemon's retention sweep deletes claim rows on
the same hourly tick (`sweep_interval_seconds`) as the archive and
stale-worker windows:

```toml
[state]
trigger_claim_retention_days = 90 # the default; -1 keeps every claim row indefinitely
```

The sweep deletes a claim marked started once it is older than the
window, whichever incarnation of the stream it was on, and a claim
still `claimed` by a process that has exited once it is older than the
window. It never deletes a claim this daemon holds, however old: that
may be a trigger it is still holding behind a paused model or a full
agent. Age is counted from when the delivery was claimed (or adopted).

**The floor is the stream's message age plus a day, two days today, and
a non-negative value below it is refused at startup.** A negative value
is not a window at all: it keeps every claim, as described below. The
trigger stream keeps a message for a day after it was published,
whether or not it has been acked: a trigger held for ten hours and then
acked stays fourteen hours more. So a durable consumer that is deleted and recreated, or
whose state is lost in a broker restore, replays the triggers published
in the last day, and the started claims are what keep those from
running twice. A claim is never older than its message, so a claim
older than a day names a message the stream has dropped; the floor is
that day rounded up, plus a day of margin for the two clocks the
argument reads.

**What a lower number costs.** Nothing about duplicates, as long as it
is at or above the floor. It shortens how long a finished run can show
the delivery that started it: past the window, `fq invocation show`
reads `none recorded` for a run that did come from the trigger stream.
The default, 90 days, keeps the link three times as long as the default
`retention_days` keeps the archived run: once the archive row is swept,
`fq invocation show` still finds the run by its row in the ownership
table, which no window sweeps, and names its trigger until the claim
goes. Set the two windows equal and the claim always goes first: its
window runs from when the delivery was claimed, the archive's from when
the finished run was archived. The difference is the run's length plus
the archive's lag, so a run that sat parked for three days reads
`trigger: none recorded` for the last three days of its archived life.
A higher number keeps the link longer, at two costs. The table is
larger, and it has no index yet
([#989](https://github.com/bricef/factor-q/issues/989)), so the reads
that look a claim up by its run scan the rows the window keeps. And a
claim that never started a run (a delivery that died with a daemon)
stays on the `fq doctor` report for longer: its line, and the non-zero
exit of `fq doctor --fail-on-issues`, last until the claim is as old as
the window, 90 days at the default where it was 30 before
[#1014](https://github.com/bricef/factor-q/issues/1014). No command
clears such a claim sooner. Requeuing its dead letter or publishing the
work again runs it under a new claim and leaves this one where it is,
and no verb deletes a claim row: only the sweep does. What an operator
can do is lower the window, to no less than the floor, and restart the
daemon, which reads the window only when it starts (`fq reload` does
not). The report stops listing such claims older than the new window as
soon as the daemon is up, and its first sweep, one interval later,
deletes them, taking with them the trigger link of every run whose claim
is older than the new window. Raising the number keeps rows from then on;
it does not bring back a claim a shorter window has already deleted.

**Keeping claims indefinitely.** Set the window to `-1` (any negative
value does the same) and the sweep never deletes a claim row. The link
from every run to the trigger delivery that started it is then never
dropped, and the table grows by a row for every trigger that starts a
run, without bound: the reads that look a claim up by its run slow down
as it grows, since the table has no index yet. The floor does not come
into it, because nothing is deleted.

`fq doctor` has no horizon then. It says so on the line of a claim that
never started a run, which reads `this line stays while the claim row
exists, and claim rows are kept indefinitely`. With no such claim the
report says nothing about retention: on a healthy table it prints
`Stranded trigger claims: none`. `fq doctor --json` always carries the
block's `horizon_ms`, as `null`. Since the row is never swept, a
never-started line never leaves the report: `fq doctor --fail-on-issues`
stays non-zero until the window is set to a number again and the daemon
restarted, as above.

**The first sweep after an upgrade.** One sweep interval after a daemon
that has this window starts (an hour by default), its first sweep
deletes every started claim older than the window and every claim an
exited process left that is older than it, and the `trigger:` line of
those runs reads `none recorded` from then on. An instance that set
`retention_days = -1` to keep its history also needs
`trigger_claim_retention_days = -1` to keep this link.

**One constraint for later work.** Three things here depend on the
control plane's ownership table (`coordination_invocation_owner`)
having no retention, which it has none of today. The first is the
serious one, because what breaks is a run, not a report. Startup
recovery marks the claim of a run that finished while its claim still
said `claimed`, and once the run is archived and its WAL row deleted,
the run's row in that table is the only record recovery finds it by
([#967](https://github.com/bricef/factor-q/issues/967)). If that table
ever gains a window shorter than this one, such a claim is no longer
marked at the next start once the run's row is gone, and a redelivered
or replayed copy of its trigger, while the stream still holds the
message, is adopted and runs the work a second time. `fq doctor` tells
a claim whose run finished from one that never started by the same
row: with it gone, an old claim whose run finished loses its record of
the run, and the report would turn "started, never marked" into "never
started". And once a finished run's archive row is swept,
`fq invocation show` finds the run by that same row (or by its cost
rows, if it made a model call), which is what lets it show the trigger
this window keeps past the archive's window: a shorter window on the
ownership table could leave such a run unfindable while its claim, and
so its trigger link, is still kept. Keep any such window at or above
this one; for startup recovery and the report, the alternative is to
give the claim its own record of the start.

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
| `fq doctor`: stuck consumers | Alert | `fq doctor` | None; use the named durable and its reported error to restore consumption. A stuck `fq-advisory-watch` can be a held dead-letter verdict instead, which ends about an hour after its advisory, once the watch has looked at it three times. |
| `fq doctor`: unavailable shared MCP servers | Alert when required work is blocked; otherwise notification | `fq doctor` | The supervisor retries startup; if required work remains blocked, fix the server or its configuration. |
| `fq doctor`: throttled models or agents at concurrency caps | Notification | `fq doctor` | Provider backoff and available agent slots release held work automatically. |
| `fq doctor`: dead letters | Notification | `fq doctor` | None for the exhausted trigger; inspect and re-submit it after fixing the cause. One `fq dead-letters list` marks `start not checked` may be false: `fq dead-letters requeue` checks the run's records and refuses a run that started, and when it cannot tell, it refuses unless you pass `--checked-not-started` after checking the run its refusal names (a run still going shows in `fq invocation show` only as a `Live execution:` block). |
| `fq doctor`: stranded trigger claims | Notification | `fq doctor` | One awaiting redelivery is adopted if the broker redelivers it, and a never-started line ages out at `[state] trigger_claim_retention_days`, when the sweep deletes its claim; otherwise none: requeue or re-publish the work of a delivery that never started a run. A run that started is not dead-lettered while the watch can read its records; a dead letter for one that `fq invocation show` says started (a `Live execution:` block counts) was written before the watch consulted the records, or when it could not and marked it `start not checked`, and is not to be requeued: `fq dead-letters requeue` refuses it while the run's claim is kept. |
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
| See which trigger delivery started a run | `fq invocation show <id>` (the `trigger:` line) |
| Find claimed deliveries that never started a run, or runs whose claim was never marked | `fq doctor` (the stranded-trigger-claims block) |
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
