# Dogfood deploys — images, one volume, a tag bump

Deploy tooling for the dogfood instance under
[ADR-0035](../../docs/adrs/accepted/0035-container-image-and-compose-supervision.md)
(built out under [#587](https://github.com/bricef/factor-q/issues/587);
the previous, launcher-based shape was
[#102](https://github.com/bricef/factor-q/issues/102)). The contract:

- **CI builds and publishes, the host pulls.** Every merge to main,
  [main-artifacts.yml](../../.github/workflows/main-artifacts.yml) builds
  static musl binaries and publishes one image per binary to
  `ghcr.io/bricef` — `fq-dogfood` (the daemon plus the fleet's
  toolchain), `github-watcher`, `fq-cron`, `fq-dashboard` — tagged with
  the twelve-hex commit the binary inside reports, plus a moving
  `main-latest` ([runtime README](../../services/fq-runtime/README.md#published-images)).
  The dogfood host never compiles.
- **The registry is the deploy history; a deploy is a tag bump.**
  [deploy.sh](deploy.sh) pulls the target tag, proves every image's
  binary reports it (`--version`, no `-dirty`), stops the scheduler then
  the daemon (SIGTERM is the drain, ADR-0027; compose's grace period is
  the deadline), writes `FQ_TAG` in `.env`, brings the stack up with
  `docker compose up -d`, waits for the daemon's `Runtime ready`, and
  confirms on the *running containers* that all four run the target
  image. Exit 0 means you are on the target commit.
- **Rollback is the same command with an older sha** — local if the
  images are still on the host, a pull otherwise. The registry keeps
  every commit tag.
- **The instance is one volume.** Everything the daemon persists —
  config, agents, the edge identity, the stores, workspaces, build caches
  — lives in the named volume `fq-dogfood_fq-data` at
  `/var/lib/factor-q`. Backing up or moving the instance is that volume
  plus the broker's `fq-dogfood_nats-data`.
- **Compose is the supervisor.** Six services with `restart:
  unless-stopped`, health ordering on the broker, rotated logs, resource
  limits on the daemon ([compose.yml](compose.yml)). No systemd units;
  the host's init starts the container runtime and nothing else of ours.

## Host layout (`~/fq-dogfood`, override with `FQ_DOGFOOD`)

```text
fq-dogfood/
├── compose.yml              # the stack — copied from ops/dogfood/
├── compose.override.yml     # host-authored, bootstrap never touches it — the internal Caddyfile, a rehearsal's profiles
├── .env                     # FQ_TAG (the deploy owns it), image repo, limits, the four host facts the ops service needs — from .env.example
├── infra/nats.conf          # broker config; infra/Caddyfile and infra/Caddyfile.internal the proxy's — copied from ops/dogfood/infra/
├── .secrets/env             # provider keys, GH_TOKEN, the broker token, the adapters' URLs (env.example)
├── .secrets/dashboard.env   # the dashboard's three edge settings, nothing else (dashboard.env.example)
├── .secrets/nats-auth.conf  # authorization { token: "…" }
├── .secrets/caddy.env       # DASH_USER / DASH_HASH / DASH_COOKIE / DASH_INTERNAL_ADDR on an internal host
├── logs/                    # notify.log — every message sent; the scheduled jobs (deploy, hygiene, backup) log to the ops service (`docker compose logs ops`)
├── backups/                 # backup's sets, FQ_BACKUP_KEEP of them
├── .deploy.lock             # the flock deploy, backup and restore share
└── .deploy.deferred         # since when deploy --auto has been deferring the same build

docker volume fq-dogfood_fq-data → /var/lib/factor-q in the daemon's container:
    fqd.toml, fq.toml, fq-cron.toml, agents/, state/ (edge identity + the
    container's pairing), cache/ (the three stores), workspace/, build/, home/
docker volume fq-dogfood_nats-data   → the event log
docker volume fq-dogfood_caddy-data, fq-dogfood_caddy-config → certificates; regenerable
```

Everything tracked is copied in by `bootstrap.sh` and refreshed by
running it again; the secrets and `.env` are written once by hand and
never overwritten. Secrets are `chmod 600` and never committed
(`ops/dogfood/.secrets/` is git-ignored so a local `docker compose
config` can create them).

**Installing a repo-tracked agent is two steps, and the order matters.**
An agent definition names a model, and the daemon refuses to start when
any loaded agent names a model no `[providers.<name>] models = [...]`
entry in `fqd.toml` declares (the ADR-0004 pricing guarantee —
`deploy.sh` aborts on `registry validation failed`). The registry is
read at **startup only**: `fq reload` re-reads the agents directory, not
the config. So:

1. add the model to the right provider's `models` list in `fqd.toml`
   (inside the volume — see "Editing files in the volume") and restart
   the daemon (`deploy.sh --force`) so the new registry takes;
2. then copy the definition into `agents/` and `fq reload`.

The live instance's own definitions are not here: they live in the
`bricef/fq-dogfood` ops repo and deploy with `migrate/sync-config.sh`
(see "Declared state is version-controlled, not hand-edited").
`ops/dogfood/agents/` holds only `backlog-groomer.md` — the weekly groom,
until #257 lands — which declares `model: claude-fable-5`; add that to
the registry before installing it.

## Bootstrap (one-time per host, and again when the tracked files change)

A dedicated Debian or Ubuntu host, and root. [bootstrap.sh](bootstrap.sh)
does the host-side work and is idempotent — run it again after any change
to `compose.yml`, `infra/` or the scripts, and it refreshes those while
never touching a secret, `.env`, or a volume:

```sh
# from a checkout on the host
sudo ops/dogfood/bootstrap.sh
# or from nothing — clones the repository to /opt/factor-q first
curl -fsSL https://raw.githubusercontent.com/bricef/factor-q/main/ops/dogfood/bootstrap.sh | sudo bash
```

Run from a checkout, it lays out that checkout as it is — pull first, or
the "refresh" is whatever the checkout last saw. The `curl` form fetches
`main` into `/opt/factor-q` before laying anything out, so it is the one
to re-run after a merge.

It installs Docker Engine and the compose plugin from Docker's
repository (plus `git`), asks the distribution's init to run the
container runtime — the only thing we ask of it — creates the deploy
user `fq` in the `docker` group, lays out `~fq/fq-dogfood` with the
tracked files and the four secrets files from their templates (one
broker token generated and written to all four places, a dashboard
session secret generated), writes the four host facts the ops service
needs into `.env`, and removes a host crontab and script copies from
before ADR-0036. It ends by printing what only a human can do:

1. `.secrets/env`: `ANTHROPIC_API_KEY` and `OPENROUTER_API_KEY` — one
   key per provider `fqd.toml` declares, because a missing one fails the
   daemon at startup and not at invocation time — plus `GH_TOKEN`
   (literal — nothing runs `gh auth token` for you now; #402 wants a
   per-role PAT). `.env`: `FQ_TAG` naming a build (`docker run --rm
   ghcr.io/bricef/fq-dogfood:main-latest --version`), so the ops image
   itself can be pulled, and `FQ_NOTIFY_HOOK`, then
   `docker compose run --rm ops notify --test`.
   `.secrets/caddy.env`: `DASH_USER`, `DASH_HASH` (`docker run --rm
   caddy:2 caddy hash-password`), and on a host with no public address
   `DASH_INTERNAL_ADDR` and the override from "An internal host".
   `docker login ghcr.io` as `fq` if the packages are private.
2. Seed the instance volume (below), or
   `docker compose run --rm ops restore <set> --yes` to bring an
   existing instance across (a freshly bootstrapped host reads as
   occupied — see "Backups and the restore drill").
3. The first deploy — `docker compose run --rm ops deploy` — then pair
   and mint the dashboard token (below).

Knobs: `FQ_USER`, `FQ_REPO_URL`, `FQ_REF`, `FQ_REPO_DIR`. Inbound 443 and
22 are the host's own business — a provider firewall on a public VM, or,
on an internal guest, the fact that nothing outside the tunnel can route
to the address at all. The stack publishes nothing else. The ops
service's schedule is live from the first `docker compose up`, but
`deploy --auto` deploys nothing until the daemon can be asked whether it
is idle — i.e. until the pairing in step 3 exists.

**Seed the instance volume.** The daemon needs `fqd.toml`, `agents/`
and `fq-cron.toml` inside the volume before its first start — compose
starts `fq-cron` unconditionally, and it exits 1 on a file that is
missing. `fq-cron.toml` must hold at least one `[[job]]` block, or the
line `job = []` for an instance that starts with nothing scheduled: an
empty file is refused at startup and by `--check`, because zero bytes
are what a reader sees of a half-written save and are not worth guessing
about (#664). Stage them in a directory and copy them in through the
image (the volume is created on first use and owned by the runtime
user; the copy runs as that user):

```sh
cd ~/fq-dogfood
mkdir -p seed/agents   # fqd.toml, fq-cron.toml, agents/*.md go here
docker compose run --rm --no-deps -v "$PWD/seed:/seed:ro" --entrypoint sh fqd \
  -c 'cp -r /seed/. /var/lib/factor-q/ && ls -la /var/lib/factor-q'
```

`fqd.toml` must say, for this shape (everything else about directories
is pinned by the image's environment and ignored in the file):

```toml
[edge]
bind = "0.0.0.0:9470"                   # compose publishes it on 127.0.0.1:9470; a loopback bind inside the container is unreachable
[workspace]
path = "/var/lib/factor-q/workspace"    # no environment form exists
[nats]
token_env = "FQ_NATS_TOKEN"             # the URL is the image's; the token is in .secrets/env
```

`fq init` writes a fresh-project starter, not this instance's config;
start from the instance's existing `fqd.toml`.

Nothing needs seeding for git auth: the image ships `gh`'s credential
helper in `/etc/gitconfig`, so a definition that pushes with plain `git`
over HTTPS works on `GH_TOKEN` alone — but a commit identity is the
definition's job, because who a commit is by is per-agent, not per-image.

**First deploy, then pair.** `docker compose run --rm ops deploy` pulls,
proves and starts everything (compose pulls the ops image itself at
`.env`'s `FQ_TAG` first, which is why bootstrap asks for one). The daemon mints its edge identity on first start
into `state/edge/` and logs the fingerprint. Its container reports
unhealthy until its `fq` is paired (the health check is `fq status`);
pair once, from the host — the pairing is kept in the volume:

```sh
cd ~/fq-dogfood
docker compose exec fqd fq connect 127.0.0.1:9470 \
  --token "$(docker compose exec fqd cat /var/lib/factor-q/state/edge/admin.token)" \
  --fingerprint "$(docker compose exec fqd cat /var/lib/factor-q/state/edge/fingerprint)"
docker compose exec fqd fq status      # answers; `docker compose ps` shows fqd healthy within a minute
```

The operator's own `fq` on the host pairs to the same published address
with the same two files (read them through `docker compose exec fqd cat
…` as above); without a terminal `fq connect` requires `--fingerprint`
(#544).

**The dashboard's identity.** Mint an attenuated token and write the
three values into `.secrets/dashboard.env`, then recreate the dashboard
(an `env_file` is read only on create):

```sh
docker compose exec fqd fq token attenuate --addr 127.0.0.1:9470 \
  --grant read:agent --grant read:control --grant read:cost \
  --grant read:event --grant read:invocation --grant read:turn
# FQ_EDGE=127.0.0.1:9470, FQ_EDGE_FINGERPRINT=<state/edge/fingerprint>, FQ_EDGE_TOKEN=<the output>
docker compose up -d fq-dashboard
```

Six grants, one per domain the pages render, all `read`; attenuation
only narrows, so the dashboard can read exactly what it shows and
command nothing. Reach it via SSH tunnel to `127.0.0.1:9472`, or through
Caddy — on a public host at that host's own name, on an internal one at
`https://{$DASH_INTERNAL_ADDR}` (the live instance: `https://10.20.0.10/`
over the tunnel, see "An internal host"). TLS-only, basic-auth plus a
90-day `fq_dash` session cookie; rotate `DASH_COOKIE` to log every
browser out.

## Routine operations

The live instance runs on an internal guest: its dashboard is
`https://10.20.0.10/` over the WireGuard tunnel, its declared state is
version-controlled in the `bricef/fq-dogfood` ops repo rather than edited
in the volume, and its backup sets stay on the guest until
`FQ_BACKUP_HOOK` names somewhere else. Everything else below is generic
to the stack.

```sh
cd ~/fq-dogfood
docker compose run --rm ops deploy               # upgrade to the newest main build (the images' main-latest)
docker compose run --rm ops deploy --force       # redeploy/restart the same build (a .env, fqd.toml or secrets change)
docker compose run --rm ops deploy 1a2b3c4d5e6f  # roll back / pin (a unique prefix is fine for images already on the host)
docker compose logs -f ops                       # what the hourly deploy --auto, hygiene and the nightly backup did
cd ~/fq-dogfood && docker compose ps            # every service, its state and health (each image probes itself)
docker compose logs -f fqd                      # the daemon's log (rotated by the driver: 5 × 50 MB)
docker compose exec fqd fq status               # ask the daemon; fq doctor, fq workers list likewise
docker compose stop fqd                         # a drain (SIGTERM), within FQ_STOP_GRACE
docker images ghcr.io/bricef/fq-dogfood         # local deploy history
```

**Agent-definition** changes don't need a deploy: `docker compose exec
fqd fq reload` re-reads the agents directory and hot-swaps the registry
(Design Principle 8), affecting the next trigger. **Config** changes do
need one — `fqd.toml` is read once, at startup — so a `[providers]`,
`[edge]`, `[summary]`, `[worker]` or retention edit takes effect on
`docker compose run --rm ops deploy --force`. A new value in
`.secrets/env` or `.env` is the same story: an `env_file` is read when a
container is *created*, which a `deploy --force` does and a restart does
not. `fq-cron.toml` is the
exception: the scheduler watches it and reloads on edit.

**Declared state is version-controlled, not hand-edited.** `agents/`,
`fqd.toml` and `fq-cron.toml` live in the `bricef/fq-dogfood` ops repo;
deploying a change is an edit and a commit there, then
`migrate/sync-config.sh fq@<host>`, which copies as the runtime user,
removes retired definitions and runs `fq reload` — `--restart` for
`fqd.toml`, whose model registry is read only at startup. `fq-cron.toml`
hot-reloads by itself. The commands below are for reading the volume and
for one-off repair: anything left in it by hand is gone at the next sync.

**Editing files in the volume.** The config and the agents live inside
`fq-dogfood_fq-data`, not on the host's filesystem. Edit them through
the daemon's container (`docker compose exec fqd sh`, then `vi` under
`/var/lib/factor-q`), or copy in and out:

```sh
docker compose cp ~/new-agent.md fqd:/var/lib/factor-q/agents/   # then fq reload
docker compose cp fqd:/var/lib/factor-q/fqd.toml ./fqd.toml       # out, to edit; cp back, then deploy --force
```

One-line invocation summaries (#216): set `[summary] model = "<cheap-model>"`
in `fqd.toml` and `deploy --force`; the daemon keeps a one-line,
cheap-model status per invocation on the dashboard. The model must be
priced (the ADR-0004 startup guarantee applies, so deploy config-first);
the summariser's own spend shows in `fq costs` as the reserved `summary`
agent. Unset = disabled.

If the dashboard shows a **"⚠ build skew"** banner (#168), it and the
daemon come from different builds. The deploy moves all five together
by construction, so in practice it means one container was recreated
by hand from a different tag; a `deploy --force` cures it.

### Before any restart

The deploy, `deploy --force` and the two procedures below all stop
the daemon. Two checks first, every time:

1. **In-flight work.** `docker compose exec fqd fq invocation list` and
   look for anything not in a terminal state. `fq status` showing
   dispatcher lag 0 only means no *pending* triggers; an
   already-dispatched invocation can be executing. The drain suspends
   in-flight invocations at a step boundary and the next start resumes
   them, but a run that is mid tool-call when the deadline expires
   becomes ambiguous and can never be resumed.
2. **Disk.** `df -h /` and `docker system df`. Per-invocation
   workspaces and the build caches fill the volume; a full disk killed
   the daemon on 2026-07-20. `build/` and terminal workspaces are
   prunable (host hygiene is #587 slice 5).

After a deploy: `fq status` answers with the new version and the agents
loaded, the projector consumer is caught up, `docker compose ps` shows
every service running (fqd `healthy` once paired), and the previous
worker's terminal state is `shutdown`, not `stale`. `fq status` also
reports `fq-summary ✗ lagging` on a filtered consumer even when
JetStream has nothing pending
([#672](https://github.com/bricef/factor-q/issues/672)) — read the
projector and coordination consumers as the health signal and ignore
that line. The `fq` client prints tarpc INFO spans to stderr on every
call (#535); `2>/dev/null` is safe when reading its output.

A deploy that crosses an event `SCHEMA_VERSION` bump (2 → 3 with #510)
does not rebuild the projection — the projector continues from its
durable position and only new events are projected — but transcripts of
invocations recorded under the old version may not render until #409
is done. Do **not** delete `cache/projection.db` across such a bump: a
rebuild replays every event and silently drops the ones it cannot parse
(#409).

### Broker restarts and the Go adapters (#551)

`github-watcher` and `fq-cron` survive a broker restart on their own:
both reconnect without an attempt limit and both may be started before
the broker is up. Before #551 nats.go's default gave up after sixty
attempts two seconds apart, so a broker down for more than two minutes
made `fq-cron` exit — and `restart: unless-stopped` brought it straight
back into the same outage, crash-looping until the broker returned —
while the watcher stayed up and kept polling GitHub with a dead
connection, churning issue labels it could not trigger. A restart policy
cannot rescue a process that does not fall over, which is why the
watcher's half was the worse of the two. Three consequences for an
operator:

- Restarting `nats` alone no longer needs the adapters restarted with
  it, unless what changed is a value they read at startup (the token
  below).
- A restart loop on `fq-cron` during a broker outage is no longer
  expected behaviour; `docker compose ps` showing it recently restarted
  is now worth a look at the logs.
- During an outage both log the disconnect and answer `/healthz` with
  503; the watcher also logs a skipped poll cycle each interval, and
  `fq-cron` logs its state-store retries. Neither publishes anything
  while disconnected — a publish fails immediately rather than being
  buffered and delivered on reconnect.

### Broker token (#542)

The broker requires a token: `infra/nats.conf` includes `auth.conf`,
which compose mounts from `.secrets/nats-auth.conf`. Every client
presents the same value — the daemon through `[nats] token_env`
(`FQ_NATS_TOKEN`), the watcher and cron as URL userinfo in
`GHW_NATS_URL` and `FQCRON_NATS_URL`. The broker is on the stack's
network only (`nats:4222`), not on host loopback. Set or rotate it in
one window, because every consumer restarts:

1. Write the new value into `.secrets/nats-auth.conf` and the three
   variables in `.secrets/env`.
2. `docker compose stop fq-cron fqd github-watcher` — the drain, done by
   hand here because the broker has to restart while nothing is
   connected to it.
3. `docker compose up -d --force-recreate nats`, then wait for
   `docker compose ps nats` to show `healthy`.
4. `docker compose run --rm ops deploy --force`: it recreates the three consumers with the new
   environment and verifies the stack.
5. Verify: `fq status` answers; `docker compose logs github-watcher
   fq-cron` show no authorization errors; and an unauthenticated
   publish is refused — from a one-off container on the stack's network:

   ```sh
   docker compose run --rm --no-deps --entrypoint sh fqd -c \
     'exec 3<>/dev/tcp/nats/4222; printf "PUB x 1\r\na\r\n" >&3; timeout 2 cat <&3'
   ```

   answers `-ERR 'Authorization Violation'` after the broker's `INFO`
   line — the Phase 0 exit criterion for the broker
   ([#554](https://github.com/bricef/factor-q/issues/554)).

### Caddy (#543)

The admin API is off (`admin off` in the Caddyfile's global block), so
no process on the host can read or replace the running config through
`localhost:2019`; `curl localhost:2019/config/` must be refused. The
cost is that `caddy reload` is gone: after any Caddyfile or `caddy.env`
change, recreate the container (`docker compose up -d --force-recreate
caddy`). Certificates live in the `fq-dogfood_caddy-data` volume and
survive it. Caddy and the dashboard run on the host network,
loopback-bound, exactly as the processes did — the dashboard refuses
any other bind, and Caddy is the only door.

### An internal host

A host with no public address — the dogfood guest on nest is one — has
nothing for `dev.lambda.works` or ACME to point at, and the migration
plan's answer was an SSH tunnel to `127.0.0.1:9472`. The door can stay
instead: [infra/Caddyfile.internal](infra/Caddyfile.internal) is the
tracked Caddyfile with the site address and the certificate source
changed — `https://{$DASH_INTERNAL_ADDR}`, `bind {$DASH_INTERNAL_ADDR}`,
`tls internal` — so Caddy answers on the host's tunnel-side address only,
with a certificate from its own CA and the same basic-auth and session
cookie. The dashboard stays loopback-only behind it.

Two host-authored lines wire it in. In `.secrets/caddy.env`:

```sh
DASH_INTERNAL_ADDR=10.20.0.10     # the address the tunnel reaches — never 0.0.0.0
```

and a `compose.override.yml` beside `compose.yml`, which `bootstrap.sh`
never touches (it refreshes both Caddyfiles, and leaves the override and
`caddy.env` alone):

```yaml
services:
  caddy:
    volumes:
      - ./infra/Caddyfile.internal:/etc/caddy/Caddyfile:ro
```

Then `docker compose up -d --force-recreate caddy`. The operator's
browser trusts the CA once — `docker compose exec caddy cat
/data/caddy/pki/authorities/local/root.crt` — or accepts the warning.
Nothing else on the host changes: `:80` stays closed, the admin API stays
off, the edge and the dashboard stay on loopback, and a port scan of the
address from outside the tunnel finds nothing, because nothing outside
the tunnel can reach the address at all.

The same override is where a rehearsal keeps the adapters off (`profiles:
["cutover"]` on `github-watcher` and `fq-cron`) while the old host's pair
is still live — the [migration
plan](../../docs/plans/active/2026-09-05-dogfood-host-migration.md) has
the details.

## Continuous delivery: `deploy --auto`

The `ops` service runs `deploy --auto` hourly from the image's crontab
([`ops.crontab`](ops.crontab)), as a one-shot sibling container that
the deploy's own `compose up` cannot kill. It is the same deploy as by
hand — pull `main-latest`, resolve it to a commit, prove every image
reports it, drain, up, verify — with three differences for running
unattended:

- **Quiet when there is nothing to do.** One timestamped line in
  `docker compose logs ops` per run; the narration starts only when a
  deploy is actually going to happen.
- **It waits its turn.** Before draining it asks the daemon, through
  the container's `fq`, whether any invocation is in flight, and defers
  to the next run if so, or if the daemon cannot be asked (an unpaired
  container is never assumed idle). This automates the first check of
  "Before any restart", and it is why a merge lands on the next quiet
  hour rather than interrupting the fleet's own builds.
- **It rolls back by itself.** If the new build does not log `Runtime
  ready` (or logs a startup refusal), or the watcher, scheduler or
  dashboard does not come up healthy on its own probe, it puts the
  previous tag back, verifies that, and exits non-zero with a `⟲ rolled
  back` line — the log shows it, `notify.sh` tells you, and the instance
  is on the build it was on before. A rollback that also fails says
  "needs a human" and stops.
- **It tells you what it did — and what landed.** A deploy, a rollback
  and a failure each go through [`notify.sh`](notify.sh) (below). The
  deploy message lists the commits between the build that was live and
  the new one: user-facing ones (`feat`, `fix`, `perf`) by subject, up
  to six, the rest counted by type, then the GitHub compare link. The
  list is asked of GitHub through the daemon container's own `gh` (it
  holds `GH_TOKEN`; the host keeps no current checkout), for the
  repository `GHW_REPO` names — best effort, so a failure is one line
  in the message, never a failed deploy. So does a deferral that
  has gone on for `FQ_DEFER_WARN_HOURS` (6): an invocation stuck in
  flight, or a container nobody paired, would otherwise keep every merge
  off the host with nothing but a quiet line an hour in the log. Reported
  once per target build, in `.deploy.deferred`.

Not Watchtower, deliberately: the five images are not published
atomically (a poll mid-publish would recreate the daemon on one build
and the dashboard on another), and an updater without a readiness check
and a rollback is not delivery, it is roulette. The cadence is hourly
rather than per-merge because the fleet merges its own PRs. A deploy by
hand (`docker compose run --rm ops deploy`) still works at any time; the
two share a lock.

## The ops service: `docker compose run --rm ops <verb>`

The stack schedules its own maintenance
([ADR-0036](../../docs/adrs/accepted/0036-ops-image-and-scheduler-service.md)).
The `ops` service runs the commit's `fq-ops` image — the five scripts,
their schedule (`ops.crontab`) under supercronic, and the docker CLI and
compose plugin they drive the stack with — as the deploy user with the
docker group. It mounts two things: the runtime's socket, the one
container that holds it (the daemon's never does; agents run there), and
this directory at the same path it has on the host, so `docker compose`
inside reads the same `compose.yml`, `.env`, override and secrets, and
every host path a script hands to `docker run -v` resolves. Every job on
its crontab is a one-shot sibling container from the same image, never a
process of the scheduler's own, so a deploy's `compose up` can recreate
the service while a job runs. It runs the hourly deploy, hygiene every
30 minutes and the backup nightly; nothing of ours runs from the host.

Any verb runs the same way by hand, from any host with docker and this
directory — `docker compose run --rm ops deploy [sha]`,
`… ops hygiene --report`, `… ops backup`, `… ops restore <set> --yes`,
`… ops notify --test` — and there are no copies of the scripts on the
host. `docker compose logs -f ops` is where the scheduled jobs' output
goes; a deploy, a rollback, `hygiene`'s warnings and a failed `backup`
still reach you through `notify.sh` as before.

Four host facts in `.env` describe the service — `FQ_DOGFOOD` (this
directory's absolute path), `FQ_UID` and `FQ_DOCKER_GID` (the deploy
user and the docker group), `FQ_HOST` (the name the notifications carry)
— and `bootstrap.sh` writes them, appending to an `.env` that predates
them. On a host that predates the service: re-run `bootstrap.sh` (the
`curl` form) — it writes the four values and removes the old crontab and
the script copies — then `docker compose up -d ops`. The oldest build
the stack can run from then on is the first with an `fq-ops` image;
`deploy <older sha>` fails at `up`.

## Notifications: `notify.sh`

The ops service's jobs log to its container (`docker compose logs ops`)
and nothing mails; without a channel of its own, a rollback or a full disk would sit in a log until
someone looked. `notify.sh <subject>`
(body on stdin) is that channel: it runs `FQ_NOTIFY_HOOK` from `.env` —
a shell command given the subject as `$1` and the body on stdin — and
appends every message to `logs/notify.log` whether or not a hook is set.
`.env.example` has one-line hooks for ntfy, Slack and mail;
`docker compose run --rm ops notify --test` proves the one you chose. A missing hook is one
line on stderr in the calling script's log, next to the thing it could
not deliver; a failing hook is reported the same way and never fails
its caller.

What goes through it, all unattended: `deploy --auto`'s deploys
(with the commits that landed — the formatting is the one part of
`deploy.sh` with a test, `ops/dogfood/tests/render-changes.sh`, run by
`just ops-ci`), rollbacks, failures and long deferrals; `hygiene`'s
warnings, one message per run; a failed `backup --auto` — all from the
ops service, signed with `FQ_HOST` rather than the container's name. A
deploy by hand tells its terminal and nothing else.

Machine-scrapeable metrics from the daemon itself are
[#342](https://github.com/bricef/factor-q/issues/342), which this does
not touch: it is the host's scripts speaking, not the runtime.

## Hygiene: `hygiene.sh`

Every 30 minutes from the ops service, into `docker compose logs ops`:
the age of the newest backup set (warns past `FQ_BACKUP_STALE_HOURS`,
36 — a nightly that has quietly stopped), the disk docker lives on
(warns above `FQ_DISK_WARN_PCT`, 80% — a full disk has killed the daemon
and the broker before; read through a one-off container with the data
root mounted, so the reading is the same from the host and from the
service), `docker system df`, the instance volume by subtree, the
workspace count and how many are untouched for a week, and dangling
images pruned. Above
`FQ_BUILD_CACHE_MAX_GB` (60) it empties the daemon's `build/` subtree —
cargo target, sccache and go caches, all regenerable — but only while no
invocation is in flight; the next build is cold. Workspaces are reported
and never deleted: reclaiming a terminal invocation's directory is the
daemon's job (#367), and the script cannot tell a suspended one from a
dead one. A non-zero exit means a threshold was crossed, and the
warnings go through `notify.sh` as one message; `hygiene.sh --report`
never prunes.

## Backups and the restore drill

`backup.sh` (nightly at 03:30 from the ops service, `--auto` so it
defers while an invocation is in flight) takes a **consistent** copy: it stops
the scheduler, the daemon (a drain), the watcher and the broker, copies
the instance volume minus `build/` and `workspace/` and the broker's
JetStream store into `backups/<utc-stamp>/` as two tarballs with
`SHA256SUMS` and a `MANIFEST` (the tag it was taken at), and starts the
stack again — a minute or two with the dashboard showing "runtime
unreachable". Copying SQLite and JetStream files under a live writer
would not be guaranteed to restore, which is the only property a backup
has. `FQ_BACKUP_KEEP` (7) sets are kept on the host; `FQ_BACKUP_HOOK` is
a command run with the finished set's directory, for the off-host copy
— the on-host copy alone does not survive the host. The live instance
has no `FQ_BACKUP_HOOK` yet, so every one of its sets exists only on the
guest; until a hook names somewhere else, pull the newest set by hand
(`rsync -a fq@<host>:fq-dogfood/backups/<set> ./`) after anything
interesting. Unattended, a failed backup goes through `notify.sh`; a
backup that stops happening is `hygiene.sh`'s stale-set warning.

`docker compose run --rm ops restore <set> [--yes]` is the other half: it verifies the checksums,
takes the stack down (volumes kept), refuses to overwrite a volume that
already has content unless `--yes`, fills both volumes from the tarballs
as root and hands them to the runtime user, and brings the stack up on
the set's tag (or `.env`'s, if set). The pairing comes back with
`state/client/`, so `fq status` answers immediately. Restore `nats-data`
**with** its consumer state, never a stream on its own: a daemon
attaching with no existing durable halts `fq-coordination` and
`fq-projector` at sequence 1 and still logs `Runtime ready`
([#684](https://github.com/bricef/factor-q/issues/684)). After any
restore, `fq status` must show coordination caught up, not merely a
daemon that started.

**The drill**, once, and again after anything touches the layout: on a
scratch VM, `bootstrap.sh`, copy a backup set over, `docker compose run
--rm ops restore <set> --yes`, then `docker compose exec fqd fq status` and `fq invocation
list` show the instance as it was. The `--yes` is not impatience: the
image seeds the volume's layout on first mount, so a freshly
bootstrapped host reads as occupied and the refusal is spurious
([#671](https://github.com/bricef/factor-q/issues/671)).
Clone-and-restore is cheap on a dedicated VM; the ADR's acceptance asks
for it and so does the production-readiness review's Phase 3. The drill
has been run for real twice — the dogfood instance's rehearsal and its
cutover.

## Moving an instance to another host

An instance is two volumes and four secrets files, so moving it is a
backup set, a bootstrap and a restore — the same three steps whether the
destination is a bigger VM, another provider or an internal guest. Moving
*off* the old launcher shape is history: it happened once, on 2026-09-12,
and the worked example with the day's real timings is the
[migration plan](../../docs/plans/active/2026-09-05-dogfood-host-migration.md)
([#587](https://github.com/bricef/factor-q/issues/587)); the ops repo's
`migrate/take-set.sh` is what packaged a set out of a launcher host, if
one ever needs packaging again.

1. **Package the set on the source: `backup`.** It is consistent by
   construction — the scheduler, the daemon, the watcher and the broker
   are all stopped for the copy — and it carries what the new host needs:
   `state/edge/` (the edge identity), `state/client/` (the container's
   pairing), `home/` (the fleet's git identity) and `cache/` (the
   stores), skipping `build/` and `workspace/`. It writes `SHA256SUMS`
   and a `MANIFEST` naming the tag the set was taken at.

   ```sh
   cd ~/fq-dogfood && docker compose run --rm ops backup
   rsync -a backups/<utc-stamp> fq@<new-host>:fq-dogfood/backups/
   ```

2. **The source's publishers stop before the destination's start.**
   `docker compose stop github-watcher fq-cron`, then
   `docker compose stop fqd` — two commands, because compose orders
   nothing between services that do not depend on each other: the two
   publishers first, so nothing is claimed or fired mid-drain, then the
   daemon; the dashboard last or never. Confirm with `fq workers list` that the
   worker ended `shutdown`, not `stale`. Never run two watchers or two
   schedulers against one repository, or both claim the same issues: the
   old pair is down before the new pair is up, and the reverse on a
   rollback. A rehearsal that wants the destination running while the
   source still publishes holds its pair back on a `profiles:
   ["cutover"]` entry in `compose.override.yml` ("An internal host").

3. **Bootstrap the destination** (above) with the same secrets and the
   same broker token, and `FQ_TAG` in `.env` naming the source's build.
   Do not run the deploy yet — the restore brings the stack up itself,
   on the set's tag. A host with no public address gets the
   `DASH_INTERNAL_ADDR` and `compose.override.yml` two-liner from "An
   internal host" now, before anything starts.

4. **`docker compose run --rm ops restore <set> --yes`.** The flag is right here, not a
   workaround: the image seeds the volume's layout on first mount, so a
   freshly bootstrapped host reads as occupied and the refusal is
   spurious ([#671](https://github.com/bricef/factor-q/issues/671)). The
   broker volume has to arrive **with** its durable consumer state — a
   daemon attaching to a stream with no existing durable halts
   `fq-coordination` and `fq-projector` at sequence 1 and still logs
   `Runtime ready` ([#684](https://github.com/bricef/factor-q/issues/684)).
   A `backup.sh` set carries the durables; a stream copied by hand may
   not.

5. **The identity is copied, so nothing re-pairs.** The set carries
   `state/edge/`, so the destination's daemon loads the source's identity
   instead of minting one: clients that were paired with the old daemon
   stay paired, and the dashboard's attenuated token and its
   `FQ_EDGE=127.0.0.1:9470` are unchanged. The step people get wrong is
   looking for the admin token afterwards. A loaded identity mints no new
   token and writes no `state/edge/admin.token` — the daemon says so at
   startup, "no admin.token beside the loaded identity; the pairing
   already stored client-side (connections.toml) is the only copy of the
   admin token" — so carry `state/client/` in the set and the container's
   own `fq` is already paired: `docker compose exec fqd fq status`
   answers and the container goes healthy with no `fq connect` at all. If
   the pairing was not carried, mint a fresh token in the container
   (`docker compose exec fqd fq token …`) rather than hunting for a file
   that is not there. Rotating the identity instead is the fallback, and
   costs one re-pair per client and one new dashboard token.

6. **Set the declared state from the ops repo.** `agents/`, `fqd.toml`
   and `fq-cron.toml` arrive inside the set, but the source of truth is
   the `bricef/fq-dogfood` ops repo: after the restore,
   `migrate/sync-config.sh fq@<host> --restart` is what makes the two
   agree, and `migrate/compose-shape.sh` holds the `fqd.toml` edits this
   shape needs — edge bind, workspace path, broker URL and token
   variable, the agents and cache directories — in one place, so a
   packaged set and a live sync cannot disagree.

7. **Accept the move before trusting it.** Six services running and
   healthy in `docker compose ps`; `fq status` reporting the tag, the
   expected agent count and the projector and coordination consumers
   caught up (ignoring the `fq-summary ✗ lagging` line, #672);
   `fq invocation list` and `fq costs` showing the history that came
   across; one trigger end to end; the dashboard rendering a recent
   transcript with no build-skew banner;
   `docker compose run --rm ops hygiene --report` clean; and
   `docker compose run --rm ops notify --test` delivered.

8. **Then the schedule.** It lives in the `ops` service and is live
   whenever the service is up — and the restore brings the whole stack
   up. So right after the restore, `docker compose stop ops` until the
   acceptance above passes, then `docker compose up -d ops`; otherwise
   the hourly `deploy --auto` can move the tag, or the nightly backup
   stop the stack, in the middle of the move. There is no host crontab
   to take out or put back.

The one move this has actually had — pre-flight, the rehearsal, the
day's sequence, acceptance, rollback and retirement — is the
[migration plan](../../docs/plans/active/2026-09-05-dogfood-host-migration.md),
and it is the worked example to read beside these eight steps.
