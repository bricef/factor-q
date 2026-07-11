# Operating the `fq run` daemon

Practical guide to the lifecycle of a long-lived `fq run` daemon: how to
**stop** it, how to **redeploy** it, and how to **hot-reload** agent
definitions — without reaching for a raw signal.

factor-q's runtime is a durable-execution engine: every in-flight
invocation's state is on the WAL, so stopping and restarting is a
*controlled* crash-and-recover, not data loss (ADR-0027). The commands
below drive that machinery cleanly and confirm what they did.

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

`fq down` publishes a control message on `fq.control.down` and then
**waits — bounded — for the daemon's `fq.system.shutdown` event** before
returning, so a zero exit means the daemon actually stopped (and, in a
normal stop, deregistered its worker so it is not left `alive` to age
into `stale`). A timeout is reported as an error pointing you at
`fq status` / `fq workers list`, rather than a false "stopped".

> Confirmation is scoped to the daemon's own clean-exit event, observed
> over NATS. There is no PID/supervisor registry yet — a supervised
> `fq up` story is future work — so `fq down` confirms *the daemon it
> reached said it stopped cleanly*, not an OS-level process check. If no
> daemon is listening, `fq down` is a no-op and times out.

Ctrl-C (SIGINT) in the daemon's own terminal remains a fast clean stop
for interactive use; SIGTERM (what `docker stop` / systemd send) runs a
graceful drain (ADR-0027). `fq down` gives you the same clean paths as a
scriptable, confirmable command from anywhere that can reach NATS.

## Redeploying: `fq drain`

For a **redeploy** — swap the binary, resume in-flight work under the new
one — use `fq drain`, not `fq down`:

```sh
fq drain     # suspend in-flight invocations to a step boundary, then exit
# ... deploy the new binary ...
fq run       # recovery resumes the suspended invocations, no lost/re-run work
```

`fq drain` is the deploy-oriented *suspend-for-handoff* command (ADR-0027);
`fq down` is the operator-facing *stop* command. `fq down` (drain mode)
reuses the same drain-to-boundary machinery, so a plain `fq down` also
leaves in-flight work recoverable — the difference is intent: `fq drain`
expects another binary to pick the work back up promptly, `fq down` means
"switch this daemon off".

## Hot-reloading agents: `fq reload`

To pick up edited agent definitions without a restart:

```sh
fq reload    # daemon re-reads the agents directory for the NEXT trigger
```

In-flight invocations keep the config they snapshotted at trigger time
(ADR-0020); the reload affects the next trigger only.

## Quick reference

| Goal | Command |
| --- | --- |
| Stop the daemon (clean, confirmed) | `fq down` |
| Stop now, skip the drain | `fq down --now` |
| Redeploy (suspend for the next binary) | `fq drain` |
| Hot-reload agent definitions | `fq reload` |
| Inspect daemon / worker health | `fq status`, `fq workers list`, `fq doctor` |

## See also

- ADR-0027 — graceful drain for deploys (the machinery `fq drain` and
  `fq down` drive).
- `fq status`, `fq doctor`, `fq workers list` — confirm the daemon and
  worker state after a stop or a deploy.
