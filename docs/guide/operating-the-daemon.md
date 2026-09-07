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
recreates them at its schema version, resets the `fq-projector`
durable so the stream replays from the start of its retention, and
starts the consumer again. Cost-bearing event rows, invocation
summaries and trigger records are carried across before the replay
begins — no spend figure is lost — and everything the stream still
holds is re-derived whole, which is what fills in a column that was
NULL for history. `--yes` is required: without it the command explains
and stops.

The command answers when the consumer is running again, not when the
replay has finished. Until it catches up, reads answer over a partial
fold — `fq status`'s `projection rows` climbs back, and a spend figure
inside the stream's window can be short. `fq status` reports the
rebuild for as long as the file lasts:

```text
  projection rebuild: in progress (replaying to stream sequence 60744) — started 2026-09-07T10:00:00+00:00, operator request: backfill
```

and `complete` once the durable's acked position has reached that
sequence.

The same rebuild happens by itself when a new build's projection
schema version is higher than the file's: the daemon rebuilds on start
and the replay follows. Do **not** delete `projection.db` to force one
— that loses the cost rows older than stream retention, which exist
nowhere else; the verb keeps them.

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

A 429 is retried after the wait the provider's `Retry-After` names, up
to `max_retry_after_ms` (default 120 s); a provider asking for longer
fails the call at once, still naming the wait. Any other 4xx is the
request being refused and is not retried at all. The `llm.failure`
event's `error_kind` says which of these happened — `timeout`,
`rate_limited`, `rejected` or `request_failed`. Keeping the fleet under
a provider's limit in the first place is
[#278](https://github.com/bricef/factor-q/issues/278).

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
  fq-projector: ok (lag 0)
  fq-coordination: ✗ stuck — 37 redeliveries past its acked floor, lag 724
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
message counts and lag beside them. The thresholds are `[bus]` in
`fqd.toml`: `stuck_after_redeliveries` decides when retrying becomes
stuck, and the escalation and log rate are configured there too.

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
| See the stuck threshold this daemon derived | `fq status` (the `stuck after` line) |
| Find invocations that stopped making progress | `fq doctor` (the executions line names them), `fq events query --event-type invocation_stuck` |
| Clear stale workers | *nothing — the daemon sweeps them* |
| Find unresolved invocations | `fq invocation list --status=ambiguous` |
| Settle one, keeping progress | `fq invocation resume <id>` |
| Settle one, abandoning progress | `fq invocation drop <id>` |

## See also

- ADR-0027 — graceful drain for deploys (the machinery used by `fq down`).
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
