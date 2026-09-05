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
600) for the whole call, `llm_connect_timeout_secs` (default 5) for
the connection — so a provider that accepts the connection and never
answers cannot park an invocation, or at `max_concurrent_invocations =
1` the daemon, until someone restarts it. A call past the deadline
fails as a transient `timeout`; the retry policy under
`[worker.llm_retry]` tries it again, so a provider that never answers
holds a worker for at most `max_attempts` times the budget before the
invocation fails with `llm_error`. Both live in `fqd.toml`, with the
reasoning behind the defaults.

A 429 is retried after the wait the provider's `Retry-After` names, up
to `max_retry_after_ms` (default 120 s); a provider asking for longer
fails the call at once, still naming the wait. Any other 4xx is the
request being refused and is not retried at all. The `llm.failure`
event's `error_kind` says which of these happened — `timeout`,
`rate_limited`, `rejected` or `request_failed`. Keeping the fleet under
a provider's limit in the first place is
[#278](https://github.com/bricef/factor-q/issues/278).

## Quick reference

| Goal | Command |
| --- | --- |
| Pair a client, from a terminal (confirms the fingerprint) | `fq connect <addr> --token "$(cat ~/.local/state/factor-q/edge/admin.token)"` |
| Pair a client, from a script (no prompt; the pin is required) | `fq connect <addr> --token "$(cat ~/.local/state/factor-q/edge/admin.token)" --fingerprint "$(cat ~/.local/state/factor-q/edge/fingerprint)"` |
| Stop the daemon (clean, confirmed) | `fq down` |
| Stop now, skip the drain | `fq down --now` |
| Redeploy (suspend for the next binary) | `fq down` |
| Hot-reload agent definitions | `fq reload` |
| Inspect daemon / worker health | `fq status`, `fq workers list`, `fq doctor` (all three ask the daemon; `fq status` reports its absence as a finding rather than failing) |
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
