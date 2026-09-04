# Dogfood deploys — hermetic, versioned, reversible

Deploy tooling for the dogfood instance (issue #102). The contract:

- **CI builds, the host fetches.** Every merge to main,
  [main-artifacts.yml](../../.github/workflows/main-artifacts.yml) builds
  static musl binaries (`fq`, `fqd`, `fq-dashboard`, `fq-cas`,
  `github-watcher`, `fq-cron`) plus the launchers in this directory,
  packages them with a sha256
  ([package.sh](../../scripts/package.sh)), and publishes the bundle to
  the rolling `main-latest` pre-release. The dogfood host never compiles.
- **Every deployed build is kept; `current` picks the active one.**
  [deploy.sh](deploy.sh) verifies the checksum and the embedded commit
  SHA (`fq`, `fqd`, `fq-dashboard` and `github-watcher` must all report
  the same one), installs into `releases/<sha>/`, drains the daemon
  (ADR-0027 — `fq down`, escalating to a confirmed `fq down --now` and
  then SIGINT, #63), atomically flips the `current` symlink, relaunches,
  and confirms every process runs from the new release dir
  (`/proc/<pid>/exe`, not log grepping). Exit 0 means you are on the
  target SHA.

  **Known defect: a clean drain is reported as a failure.** `fq down`
  exits 0 as soon as the daemon's edge stops answering — which happens
  while the daemon is still tearing down (closing stores, deregistering
  its worker, publishing `fq.system.shutdown`), a moment before the
  process actually exits. `deploy.sh`'s next `kill -0` runs with no
  grace period at all, so it usually still sees a live pid and prints
  `graceful stop failed — requesting immediate stop (fq down --now)` on
  a drain that in fact succeeded. That `--now` then fails against an
  edge socket that is already closed, and the script fires SIGINT at a
  pid that is already gone. Read those two lines as noise, not as a
  daemon that resisted: the escalation is unconditional, not
  deadline-governed, and there is no wait between the drain and the
  check for one to govern. The tell that the drain worked is the
  worker's terminal state — `shutdown`, not `stale` — and that is what
  the 2026-08-25 deploy recorded. The deadline that *is* real is the
  daemon's own `drain_deadline_ms` (default 120s), which bounds how long
  `fq down` waits before reporting a timeout; `deploy.sh` does not
  consult it. Fixing this means giving the `kill -0` a grace window,
  which is a change to the script and wants its own PR.
- **Rollback is local and instant**: `deploy.sh <previous-sha>` — no
  network, no rebuild, just a symlink flip through the same
  drain/verify path. `deploy.sh` keeps the newest `KEEP_RELEASES`
  (default 5) dirs and prunes the rest.
- **The environment is declared, not ambient.** All four launchers
  (`run.sh`, `watcher.sh`, `dashboard.sh`, `cron.sh`) source exactly one
  file, `.secrets/env` ([template](env.example)) — nothing else reaches
  the processes' environment. The dashboard, the one web-facing
  process, gets less than the file: `dashboard.sh` reads it and starts
  the binary under `env -i` with only `FQ_EDGE`, `FQ_EDGE_TOKEN`,
  `FQ_EDGE_FINGERPRINT` and its `FQ_DASHBOARD_*` tuning
  ([#545](https://github.com/bricef/factor-q/issues/545)) — the
  provider keys and `GH_TOKEN` never enter it.

## Host layout (`~/fq-dogfood`, override with `FQ_DOGFOOD`)

```text
fq-dogfood/
├── current -> releases/<sha>/   # the active build (symlink)
├── releases/<sha>/              # fq, fqd, fq-cas, fq-dashboard, github-watcher, fq-cron + launchers
├── fqd.toml                     # daemon config — host-side; read at STARTUP, so a change
│                                #   needs `deploy.sh --force`, not `fq reload`
├── fq-cron.toml                 # scheduled jobs — host-side; editing deploys/reloads them
├── agents/                      # agent definitions — host-side; canonical copies of
│                                #   repo-tracked ones live in ops/dogfood/agents/ —
│                                #   install with scp (declare the model first, below)
├── .secrets/env                 # the single declared environment (chmod 600)
├── .secrets/nats-auth.conf      # the broker token, included by infra/nats.conf (#542)
├── .secrets/caddy.env           # dashboard basic-auth + session secret for Caddy
├── infra/                       # NATS + Caddy compose and config (copied from ./infra)
├── logs/                        # fq-run.log, watcher.log, dashboard.log, cron.log
└── workspace/ cache/            # runtime state
```

**Installing a repo-tracked agent is two steps, and the order matters.**
An agent definition names a model, and the daemon refuses to start when
any loaded agent names a model no `[providers.<name>] models = [...]`
entry in `fqd.toml` declares (the ADR-0004 pricing guarantee —
`deploy.sh` greps for `registry validation failed` and aborts). The
registry is read at **startup only**: `fq reload` re-reads the agents
directory, not the config. So:

1. add the model to the right provider's `models` list in `fqd.toml`,
   and restart the daemon (`deploy.sh --force`) so the new registry
   takes;
2. then `scp` the definition into `agents/` and `fq reload`.

Doing it the other way round leaves an agent the running daemon rejects
and a `fqd.toml` whose next restart is a crash. `ops/dogfood/agents/`
currently holds `backlog-groomer.md` (weekly groom: cron
`fq trigger backlog-groomer` until #257), which declares
`model: claude-fable-5` — **not** in the live instance's registry, so it
needs step 1 before it can be installed at all.

The launchers (`run.sh`, `watcher.sh`, `dashboard.sh`, `cron.sh`) ship
*inside* the artifact bundle so they are versioned with the binaries
they launch and roll back with them. `deploy.sh` itself runs from a repo
checkout — it is the bootstrap, and can't live inside the thing it
swaps.

## Bootstrap (one-time per host)

```sh
mkdir -p ~/fq-dogfood/{releases,logs,agents,.secrets} && chmod 700 ~/fq-dogfood/.secrets
cp -r ops/dogfood/infra ~/fq-dogfood/
install -m 600 ops/dogfood/env.example ~/fq-dogfood/.secrets/env  # then edit
# Broker token (#542): one value in four places — the include file the
# broker reads, and the three variables env.example describes.
tok="$(openssl rand -hex 32)"
printf 'authorization { token: "%s" }\n' "$tok" > ~/fq-dogfood/.secrets/nats-auth.conf
chmod 600 ~/fq-dogfood/.secrets/nats-auth.conf
#   .secrets/env: FQ_NATS_TOKEN=$tok; GHW_NATS_URL and FQCRON_NATS_URL
#                 as nats://$tok@127.0.0.1:4223
#   fqd.toml:     [nats] url = "nats://127.0.0.1:4223" (no credential — the
#                 daemon refuses one) and token_env = "FQ_NATS_TOKEN"
# .secrets/caddy.env: DASH_USER / DASH_HASH / DASH_COOKIE (dashboard section)
# fqd.toml: copy an existing instance config. `fq init` does write one,
#           but it is a fresh-project starter — one provider, one model,
#           the dev broker's URL, and no [edge] bind — so a daemon
#           started on it would not load this instance's agents.
#           (`fq init` writes fq.toml too; that is the *client's* config,
#           a different file whose only setting is `[daemon] addr`.)
# fq-cron.toml: install the host job schedule BEFORE the first deploy —
#           deploy.sh launches cron.sh iff this file exists, and skips it
#           silently otherwise.
ops/dogfood/deploy.sh
```

`deploy.sh` starts all four processes itself, cron included. Do **not**
also launch `cron.sh` by hand afterwards: that leaves two `fq-cron`
processes on the same schedule, each publishing every job. A subsequent
real deploy does clean them up — bring-down loops over every matching
pid — but a no-op `deploy.sh` on the build already running will not: its
early-exit check looks at one cron pid, sees the right release, and
reports "already running". If you added `fq-cron.toml` after a deploy,
`deploy.sh --force` is the way to pick it up.

Migrating an existing in-place instance (pre-#102: host-built binary,
untracked `run.sh`/`watcher.sh`/`cron.sh`/`redeploy.sh`): fold any local secrets
into `.secrets/env`, delete the legacy scripts, `bin/`, and `fq.rollback`,
then run `deploy.sh`. State (`fqd.toml`, `fq-cron.toml`, `agents/`, `cache/`, `workspace/`,
the NATS volume) is untouched by deploys.

Note (#362): the daemon's edge identity — self-signed certificate plus
biscuit token root, whose loss orphans every pinned client and issued
token — lives in the **state** directory, not `cache/`. With no
`[state] directory` in the instance `fqd.toml` and no `FQ_STATE_DIR` in
`.secrets/env`, that resolves to `$HOME/.local/state/factor-q`, i.e.
*outside* the dogfood tree. Set `[state] directory = "./state"` to keep
it project-local like `cache/`. Either way a daemon whose state
directory is empty adopts an identity still sitting at `cache/edge`
rather than minting a new one, and says so on startup.

Two files sit beside the identity in `<state>/edge/`
([#545](https://github.com/bricef/factor-q/issues/545)): `admin.token`,
the all-authority token, written once when the identity is minted,
mode 0600 and **never printed** — the daemon logs where it is, not
what it is, so `logs/fq-run.log` never holds it — and `fingerprint`,
the certificate's SHA-256 in hex, the pin every client and the
dashboard verify. Pair the operator's `fq` from the host with both
(the default state dir shown; substitute `[state] directory` if set):
`fq connect 127.0.0.1:9470 --token "$(cat ~/.local/state/factor-q/edge/admin.token)"
--fingerprint "$(cat ~/.local/state/factor-q/edge/fingerprint)"`. Without a terminal
`fq connect` refuses to pair unless `--fingerprint` is given
([#544](https://github.com/bricef/factor-q/issues/544)); from a
terminal it shows the fingerprint and asks.

## Routine operations

```sh
ops/dogfood/deploy.sh              # upgrade to the newest main build
ops/dogfood/deploy.sh --force      # redeploy/restart the same build (e.g. env change)
ops/dogfood/deploy.sh 1a2b3c4      # roll back / pin (sha prefix ok)
ls ~/fq-dogfood/releases           # deploy history on this host
```

**Agent-definition** changes don't need a deploy: `fq reload` re-reads
the agents directory and hot-swaps the registry (Design Principle 8),
affecting the next trigger. **Config** changes do need one. `fq reload`
reads the agents directory and nothing else — `fqd.toml` is read once, at
startup — so a `[providers]`, `[edge]`, `[summary]`, `[worker]` or
retention edit takes effect on `deploy.sh --force`, not on a reload. A
new provider key is the same story for a different reason: only launch
reads `.secrets/env`.

### Before any restart

`deploy.sh`, `deploy.sh --force` and the two procedures below all stop
the daemon. Two checks first, every time:

1. **In-flight work.** `fq invocation list` and look for anything not in
   a terminal state. `fq status` showing dispatcher lag 0 only means no
   *pending* triggers; an already-dispatched invocation can be
   executing. The drain suspends in-flight invocations at a step
   boundary and the next start resumes them, but a run that is mid
   tool-call when the deadline expires becomes ambiguous and can never
   be resumed.
2. **Disk.** `df -h /`. Per-invocation workspaces and cargo target dirs
   fill it; a full disk killed the daemon on 2026-07-20.

After the flip, the known-defect noise above aside: `fq status` answers
with the new version and the agents loaded, the projector consumer is
caught up, and the previous worker's terminal state is `shutdown`, not
`stale`. The `fq` client prints tarpc INFO spans to stderr on every call
(#535); `2>/dev/null` is safe when reading its output.

A deploy that crosses an event `SCHEMA_VERSION` bump (2 → 3 with #510)
does not rebuild the projection — the projector continues from its
durable position and only new events are projected — but transcripts of
invocations recorded under the old version may not render until #409
is done. Do **not** delete `cache/projection.db` across such a bump: a
rebuild replays every event and silently drops the ones it cannot parse
(#409).

### Broker token (#542)

The instance broker on 4223 requires a token: `infra/nats.conf`
includes `auth.conf`, which compose mounts from
`.secrets/nats-auth.conf`. Every client presents the same value — the
daemon through `[nats] token_env` (`FQ_NATS_TOKEN`), the watcher and
cron as URL userinfo in `GHW_NATS_URL` and `FQCRON_NATS_URL`, each in
the process environment and never in argv. Set or rotate it in one
window, because every consumer restarts:

1. Write the new value into `.secrets/nats-auth.conf` and the three
   variables in `.secrets/env` (the bootstrap block above).
2. `./current/fq down` (the drain), then SIGTERM the watcher, cron and
   dashboard — the bring-down `deploy.sh` performs, done by hand here
   because the broker has to restart while nothing is connected to it.
3. `docker compose -f infra/docker-compose.yml up -d --force-recreate nats`,
   then wait for `curl -sf http://127.0.0.1:8223/healthz`.
4. `deploy.sh --force` (or a plain `deploy.sh` if a newer build is due):
   it finds no daemon, installs, and relaunches all four processes with
   the new environment.
5. Verify: `fq status` answers; `logs/watcher.log` and `logs/cron.log`
   show no authorization errors; and an unauthenticated publish is
   refused —

   ```sh
   exec 3<>/dev/tcp/127.0.0.1/4223; printf 'PUB x 1\r\na\r\n' >&3; timeout 2 cat <&3; exec 3>&-
   ```

   answers `-ERR 'Authorization Violation'` after the broker's `INFO`
   line. This is the Phase 0 exit criterion for the broker
   ([#554](https://github.com/bricef/factor-q/issues/554)).

### Caddy (#543)

The admin API is off (`admin off` in the Caddyfile's global block), so
no process on the host can read or replace the running config through
`localhost:2019`; `curl localhost:2019/config/` must be refused. The
cost is that `caddy reload` is gone: after any Caddyfile or `caddy.env`
change, recreate the container
(`docker compose -f infra/docker-compose.yml up -d --force-recreate caddy`).
Certificates live in the `caddy-data` volume and survive it.

One-line invocation summaries (#216): set `[summary] model = "<cheap-model>"`
in `fqd.toml` and restart (`deploy.sh --force`) and the daemon keeps a one-line,
cheap-model status per invocation on the dashboard's invocation surfaces —
what work was expected, what it is doing now, how it ended. The model must
be priced (the ADR-0004 startup guarantee applies, so deploy config-first);
the summariser's own spend shows in `fq costs` as the reserved `summary`
agent and is never charged to an invocation. Unset = disabled, zero change.

The operator dashboard (read-only web view, #105) rides in the bundle
and `deploy.sh` stops and relaunches it with the daemon and watcher. It
must run the same build as the daemon: the two share the contract types
they exchange, so a field removed or renamed on one side is a decode
failure on the other.

**One-time setup.** The dashboard reads over the daemon's authenticated
edge, which means it needs an identity — it is the first process other
than the operator's own CLI to need one. Its own
[README](../../services/fq-dashboard/README.md) documents the binary,
its flags and every page; what follows is the dogfood-specific form,
three variables in `.secrets/env` (see `env.example`, which carries the
commands):

1. **Move the edge off port 9472.** `[edge] bind` defaults to
   `127.0.0.1:9472`, which is the port the dashboard itself serves on
   and the one Caddy proxies to. Set `bind = "127.0.0.1:9470"` under
   `[edge]` in `fqd.toml`, restart the daemon, and set `FQ_EDGE` to
   match. `dashboard.sh` refuses to launch without `FQ_EDGE` rather
   than defaulting into the collision.
2. **Pin the daemon.** `FQ_EDGE_FINGERPRINT` is the SHA-256 the daemon
   printed when it provisioned its identity (`edge: certificate
   fingerprint`) and keeps in `<state>/edge/fingerprint`.
3. **Mint an attenuated token.** `FQ_EDGE_TOKEN` must be an
   *attenuation* of the admin token, not the admin token itself:

   ```sh
   fq token attenuate --addr "$FQ_EDGE" \
     --grant read:agent --grant read:control --grant read:cost \
     --grant read:event --grant read:invocation --grant read:turn
   ```

   Six grants, one per domain the pages render, all `read`. Attenuation
   only narrows and never widens, so the dashboard can read exactly
   what it shows and command nothing. The binary refuses to start
   without a token and prints this line when it does.

Manual launch, if ever needed:
`setsid ./current/dashboard.sh >> logs/dashboard.log 2>&1 </dev/null &`.
Reach it via SSH tunnel to `127.0.0.1:9472`, or through the public
door: the infra compose runs Caddy serving `https://dev.lambda.works`
(TLS-only — plain HTTP is refused, not redirected; the dashboard
itself stays loopback-bound). Auth is basic-auth plus a persistent
session: the first successful login sets a 90-day `fq_dash` cookie and
later visits skip the password prompt; rotate the session secret (and
so log every browser out) by changing `DASH_COOKIE`. One-time setup:
write `.secrets/caddy.env` (chmod 600) containing `DASH_USER=<user>`,
`DASH_HASH=<bcrypt>` (hash via `docker run --rm caddy:2 caddy
hash-password`) and `DASH_COOKIE=<token>` (`openssl rand -hex 32`;
leaving it unset is safe — cookie login fails closed and every request
falls back to basic-auth), then `docker compose -f
infra/docker-compose.yml up -d`. After editing `caddy.env`, recreate
the container (`docker compose -f infra/docker-compose.yml up -d
--force-recreate caddy`) — a config reload does not repick env vars.
Caddy is the one process deploy.sh does not manage (it is
docker-supervised, `restart: unless-stopped`).

If the dashboard shows a **"⚠ build skew"** banner (#168), it detected —
from the version `control.status` reports — that the daemon comes from
a different build than itself. This is less likely to be breaking than
it once was: the edge is JSON in a stable envelope, so an older
dashboard reads a newer daemon that has merely added a field. Pages
still render whatever decodes; the remedy is always the same: redeploy
so both run one build
(`deploy.sh` does this by construction — the banner in practice means
someone launched a process by hand from the wrong `releases/<sha>/`).
`fq-dashboard --version` prints the dashboard's build SHA.

Not built yet, by design (see #102): health-gate + auto-rollback after
the flip, and any supervisor (systemd was ruled out of scope there; the
launchers are detached with `setsid`, NATS and Caddy restart via
docker's `restart: unless-stopped`). The production-readiness review
reopened the systemd question as Phase 1 work (#553); the answer is
[ADR-0035](../../docs/adrs/accepted/0035-container-image-and-compose-supervision.md)
— container images under docker compose, no systemd units. Nothing of
it is built yet, and this README describes the `setsid` shape until it
is.

## The compose stack (ADR-0035)

[compose.yml](compose.yml) is the stack the instance moves to under
[ADR-0035](../../docs/adrs/accepted/0035-container-image-and-compose-supervision.md):
six services — the broker, the proxy, the daemon, the watcher, the
dashboard and the scheduler — with `restart: unless-stopped`, health
ordering on the broker, a stop grace period longer than the daemon's
drain deadline, resource limits on the daemon and rotated logs, running
the images `main-artifacts.yml` publishes to `ghcr.io/bricef` (the
[runtime README](../../services/fq-runtime/README.md#published-images)
describes them). No systemd units. Tracked as
[#587](https://github.com/bricef/factor-q/issues/587); this section is
slice 3, the stack definition.

**Not yet the live shape.** `deploy.sh` still deploys the tarball and
launches the four `setsid` processes; moving it to a tag bump against
this file is slice 4, and the migration runbook is slice 6. Until then
the two shapes coexist in this directory and **must not run at once on
one host**: both bind the edge on `127.0.0.1:9470` and the dashboard on
`9472`, and `infra/docker-compose.yml` (the broker and proxy of the
launcher shape) and `compose.yml` both want `:443`. Bring one down
before the other comes up.

What differs from the launcher shape, in the order it bites:

- **The daemon runs in the `fq-dogfood` image**, whose one volume
  `fq-data` at `/var/lib/factor-q` is the instance: `fqd.toml`,
  `fq.toml`, `fq-cron.toml`, `agents/`, `state/`, `cache/`,
  `workspace/`, `build/`. The tree under `~/fq-dogfood` becomes the
  content of that volume (minus `releases/`, `current`, `.secrets/`).
- **`fqd.toml` needs three settings for this shape**: `[edge] bind =
  "0.0.0.0:9470"` (compose publishes it on host loopback; the container's
  loopback would be unreachable), `[workspace] path =
  "/var/lib/factor-q/workspace"` (no environment form exists), and
  `[nats] token_env = "FQ_NATS_TOKEN"` with **no** `url` override needed
  — the image's `FQ_NATS_URL=nats://nats:4222` outranks it. Every other
  directory setting is pinned by the image's environment and ignored
  in the file.
- **The broker leaves host loopback.** It lives on the stack's network
  as `nats:4222`, so `GHW_NATS_URL` and `FQCRON_NATS_URL` in
  `.secrets/env` read `nats://<token>@nats:4222`. Host-side tooling
  that spoke to `127.0.0.1:4223` (a `nats` CLI for a stream backup)
  runs as a one-off container on the `fq-dogfood_default` network
  instead.
- **`.secrets/env` is a plain KEY=VALUE file** under compose: no shell
  expansion, so `GH_TOKEN` must be a literal token. The dashboard's
  three edge settings move to `.secrets/dashboard.env`
  ([template](dashboard.env.example)), the compose-shape equivalent of
  `dashboard.sh`'s `env -i` (#545). The launcher tuning
  `FQ_WATCH_REPO` / `FQ_WATCH_AGENT` / `FQ_WATCH_POLL` becomes the
  watcher's own `GHW_REPO` (required — it has no default) / `GHW_AGENT`
  / `GHW_POLL`.
- **`.env` beside `compose.yml`** ([template](.env.example)) holds
  `FQ_TAG`, the twelve-hex commit to run, plus the resource limits and
  the stop grace period. It is compose's substitution file, not the
  processes' environment. `deploy.sh` will own `FQ_TAG` from slice 4;
  never set it to `main-latest`, which moves under a running instance.
- **The dashboard and Caddy stay on the host network**, loopback-bound
  exactly as the processes were: the dashboard refuses any bind but
  loopback (its safety property — Caddy is the only door), and a
  loopback bind inside a container is unreachable from outside it. The
  `Caddyfile` is unchanged.
- **Pairing happens inside the daemon's container.** The daemon's
  `HEALTHCHECK` is `fq status` over the edge, which needs a pairing the
  container's `fq` keeps under `state/client/` in the volume; until it
  exists the container reports unhealthy. Pair once, from the host:

  ```sh
  docker compose exec fqd fq connect 127.0.0.1:9470 \
    --token "$(docker compose exec fqd cat /var/lib/factor-q/state/edge/admin.token)" \
    --fingerprint "$(docker compose exec fqd cat /var/lib/factor-q/state/edge/fingerprint)"
  ```

  The operator's own `fq` on the host pairs to the same published
  address with the same two files, read through `docker compose exec`
  as above (or from a `docker run --rm -v fq-dogfood_fq-data:/v` view of
  the volume).
- **The broker's health check needs `wget`**, so the stack runs
  `nats:2.14.3-alpine` — the same pinned version as `.nats-version`, the
  variant with a shell. Bump the two together.
- **Zombies.** The daemon is PID 1 in its container and spawns agents'
  processes; `init: true` puts a tiny init above it to reap what a
  killed agent leaves behind.

Bring-up on a host that already has the tree, once the four secrets
files and `.env` exist:

```sh
cd ~/fq-dogfood && docker compose up -d      # all six, broker first
docker compose ps                            # every service, its health
docker compose logs -f fqd                   # the daemon's log
docker compose stop fqd                      # a drain (SIGTERM, ADR-0027), within FQ_STOP_GRACE
```

A deploy or rollback in this shape is `FQ_TAG=<sha>` in `.env` and
`docker compose up -d`; slice 4 gives that a script with the drain,
verification and `flock` the launcher `deploy.sh` has.
