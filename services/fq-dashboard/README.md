# fq-dashboard

The operator dashboard: a read-only web view of a running factor-q
daemon (#105 layer 3). It is a standalone binary with its own crash
domain — it holds a client to the daemon and an HTTP server, nothing
else. It cannot touch runtime state and cannot take the runtime down;
if the daemon is unreachable every page still answers, with a 503 and a
"runtime unreachable" banner, and if this process dies the daemon never
notices.

## How it reads

**Over the daemon's authenticated edge, as a second principal.** It is
the first process other than an operator's own `fq` that needs an
identity of its own. Every page invokes declared operations with a token
*attenuated* to six read grants, so a compromised dashboard can read
exactly what it renders and command nothing.

That is also why this crate links no store. It depends on `fq-edge`
(transport, certificate pinning, token presentation) and `fq-ops` (the
declared shapes and view DTOs) — not on `fq-runtime` or `fq-daemon`, and
so not on sqlx, NATS, reqwest or rmcp. `tests/thin_reader_gate.rs`
enforces that from the manifest; `fq-ops`' own dependency gate closes
the transitive route.

Rendering is deliberately naive (v0, per the plan): each browser request
dials the daemon fresh — localhost TCP, microseconds, and it doubles as
reconnect logic — and renders server-side HTML. Liveness is a
[datastar](https://data-star.dev) poll using the vendored client at
`assets/datastar.js`; each page's `#main` region re-fetches its own URL
on a tick and the response is a single-event SSE patch morphed in place,
so open folds, scroll position and text selection survive. No-JS
browsers keep a full-page `<meta refresh>` via `<noscript>`.

## Pages

| Route | Reads | Grant |
|---|---|---|
| `/` | `control.status` + `control.doctor` | `read:control` |
| `/invocations` | `invocation.active` + `invocation.list` | `read:invocation` |
| `/invocations/{id}` | `invocation.get` | `read:invocation` |
| `/invocations/{id}/transcript` | `invocation.get` + `turn.list`, tailed live by `turn.stream` | `read:invocation`, `read:turn` |
| `/events` | `event.list` | `read:event` |
| `/costs` | `cost.summary` | `read:cost` |
| `/costs/{agent}` | `cost.by_agent` | `read:cost` |
| `/agents` | `agent.list` | `read:agent` |
| `/agents/{id}` | `agent.get` | `read:agent` |

## Configuration

Every flag has an environment-variable fallback; the deployed form sets
the environment and passes nothing.

| Flag | Env | Default | Notes |
|---|---|---|---|
| `--bind` | `FQ_DASHBOARD_BIND` | `127.0.0.1:9472` | must be loopback — a non-loopback bind is refused at startup |
| `--edge` | `FQ_EDGE` | `127.0.0.1:9472` | the daemon's `[edge] bind` — see the port collision below |
| `--edge-token` | `FQ_EDGE_TOKEN` | *(required)* | the attenuated token |
| `--edge-fingerprint` | `FQ_EDGE_FINGERPRINT` | *(required)* | the daemon's certificate SHA-256, hex |
| `--refresh` | `FQ_DASHBOARD_REFRESH` | `5` | poll interval, seconds |

There is no unauthenticated mode to degrade into: the edge refuses a
connection with no token, so a missing credential is a startup error
naming the fix, not a process that runs and renders "unreachable" on
every page.

## Setup

Three things, and the first one is a port collision most hosts hit.

### 1. Move the edge off 9472

`--bind` and `--edge` **default to the same port**. The dashboard serves
on `127.0.0.1:9472` and the daemon's `[edge] bind` defaults to
`127.0.0.1:9472`; whichever binds second fails, and for the daemon that
is fatal. Move one of them. Moving the edge is usually right, since a
reverse proxy in front of the dashboard is already pointed at 9472:

```toml
# fqd.toml — the DAEMON's config. Not fq.toml: that is the client's
# config, it understands `[daemon] addr` and nothing else, and an
# `[edge]` table there is read by nobody.
[edge]
bind = "127.0.0.1:9470"
```

Restart the daemon — `fqd` reads its config at startup, and `fq reload`
re-reads the agents directory, not the config — then set `FQ_EDGE` to
the same address. The launcher refuses to start without `FQ_EDGE` rather
than defaulting into the collision.

### 2. Pin the daemon

`FQ_EDGE_FINGERPRINT` is the SHA-256 of the daemon's self-signed
certificate — the pin that makes it an identity rather than an
encryption blanket. The daemon prints it when it provisions its identity
(the `edge: certificate fingerprint` line); `fq connect` also records it
in `~/.config/factor-q/connections.toml`. Rotating the daemon's identity
invalidates the pin, and every token issued under it.

### 3. Mint an attenuated token

`FQ_EDGE_TOKEN` must be an **attenuation** of the admin token, never the
admin token itself. Minting is offline — no daemon round-trip:

```sh
fq token attenuate --addr "$FQ_EDGE" \
  --grant read:agent --grant read:control --grant read:cost \
  --grant read:event --grant read:invocation --grant read:turn
```

Six grants, one per domain the pages render, all `read`. Deliberately
not `read:*`, which would additionally grant `worker`, `dead_letter`,
`trigger` and `operation` — four domains no page here renders.
Attenuation only ever narrows, so this token can read exactly what the
dashboard shows and command nothing; a dashboard holding the admin token
would make that machinery decorative. Re-mint only when the daemon's
identity is rotated.

The binary prints this exact invocation in its startup refusal, so a
missing token is a copy-paste rather than a documentation hunt, and a
test asserts the printed line names every grant the pages need.

## Reaching it

Localhost-only by construction. Either tunnel:

```sh
ssh -L 9472:127.0.0.1:9472 <host>
```

…or put a reverse proxy in front of it and let that terminate TLS and
authenticate. The dogfood instance does the latter — see
[`ops/dogfood/README.md`](../../ops/dogfood/README.md) for its Caddy
setup, launcher, and how `deploy.sh` keeps the dashboard in lockstep
with the daemon.

## Build skew

`fq-dashboard --version` prints this build's git SHA. If the daemon that
`control.status` describes came from a different build, every page
carries a **"⚠ build skew"** banner naming both SHAs (#168).

This is warn-and-continue, not fail: the edge is JSON in a stable
envelope, so an older dashboard reads a newer daemon that has merely
added a field, and pages render whatever decodes. The risk is a field
*removed* or renamed, which is a decode failure on one side — so the
remedy is always to redeploy both from one build. Once skew has been
observed, a read failure names it as the likely cause instead of
reporting a bare "runtime unreachable" (the 2026-07-14 incident, #154).
Unknown is not mismatch: a dashboard that has never reached the daemon
does not banner.

## Development

```sh
just build-dashboard        # cargo build -p fq-dashboard
just test-dashboard         # hermetic — spins a real edge in-process, no broker
just dashboard-screenshots  # PNG of every page, from fixtures
```

The tests serve a fixture surface over a **real** `fq-edge` with a token
attenuated to exactly the six grants, and drive the router end to end
with `tower::ServiceExt::oneshot`. Both sides of the wire use the
declared shapes, so a renamed field fails this compile. One test drops a
grant and asserts the page it serves is refused — the attenuation is
load-bearing, not decorative.

`fq-dashboard render-fixtures --out <dir>` writes every page as static
HTML from canned, fixed-timestamp data: no daemon, no broker, so a
visual diff is a rendering change and never the clock. That is what
`scripts/dashboard-screenshots.sh` screenshots over `file://`, and what
CI uploads as an artifact when dashboard code changes.
