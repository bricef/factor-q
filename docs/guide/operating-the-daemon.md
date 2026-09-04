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
difference. Use `--now` only when the drain must be skipped.

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
