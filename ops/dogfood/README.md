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
├── .env                     # FQ_TAG (deploy.sh owns it), image repo, limits — from .env.example
├── infra/nats.conf          # broker config; infra/Caddyfile the proxy's — copied from ops/dogfood/infra/
├── .secrets/env             # provider keys, GH_TOKEN, the broker token, the adapters' URLs (env.example)
├── .secrets/dashboard.env   # the dashboard's three edge settings, nothing else (dashboard.env.example)
├── .secrets/nats-auth.conf  # authorization { token: "…" }
├── .secrets/caddy.env       # DASH_USER / DASH_HASH / DASH_COOKIE
├── deploy.sh, hygiene.sh, backup.sh, restore.sh, notify.sh   # copied by bootstrap.sh, run by the crontab
├── logs/                    # deploy.log, hygiene.log, backup.log — the cron jobs' output; notify.log, every message sent
├── backups/                 # backup.sh's sets, FQ_BACKUP_KEEP of them
├── .deploy.lock             # the flock deploy.sh, backup.sh and restore.sh share
└── .deploy.deferred         # since when deploy.sh --auto has been deferring the same build

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

`ops/dogfood/agents/` holds `backlog-groomer.md` (the weekly groom,
until #257 lands), which declares `model: claude-fable-5` — check it is
in the live registry before installing it.

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

It installs Docker Engine and the compose plugin from Docker's
repository (plus `git` and `cron`), asks the distribution's init to run
the container runtime — the only thing we ask of it — creates the
deploy user `fq` in the `docker` group, lays out `~fq/fq-dogfood` with
the tracked files and the four secrets files from their templates (one
broker token generated and written to all four places, a dashboard
session secret generated), and installs the [crontab](crontab). It ends
by printing what only a human can do:

1. `.secrets/env`: `ANTHROPIC_API_KEY`, `GH_TOKEN` (literal — nothing
   runs `gh auth token` for you now; #402 wants a per-role PAT).
   `.secrets/caddy.env`: `DASH_USER`, `DASH_HASH` (`docker run --rm
   caddy:2 caddy hash-password`). `docker login ghcr.io` as `fq` if the
   packages are private.
2. Seed the instance volume (below), or `restore.sh <set>` to bring an
   existing instance across.
3. The first `deploy.sh`, then pair and mint the dashboard token (below).

Knobs: `FQ_USER`, `FQ_REPO_URL`, `FQ_REF`, `FQ_REPO_DIR`. Inbound 443 and
22 are the provider firewall's business; the stack publishes nothing
else. The crontab is active from the moment it is installed, but
`deploy.sh --auto` deploys nothing until the daemon can be asked whether
it is idle — i.e. until the pairing in step 3 exists.

**Seed the instance volume.** The daemon needs `fqd.toml`, `agents/`
and, if the scheduler runs, `fq-cron.toml` inside the volume before its
first start. Stage them in a directory and copy them in through the
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

**First deploy, then pair.** `~/fq-dogfood/deploy.sh` pulls, proves and
starts everything. The daemon mints its edge identity on first start
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
Caddy at `https://dev.lambda.works` (TLS-only, basic-auth plus a 90-day
`fq_dash` session cookie; rotate `DASH_COOKIE` to log every browser
out).

## Routine operations

```sh
~/fq-dogfood/deploy.sh              # upgrade to the newest main build (the images' main-latest)
~/fq-dogfood/deploy.sh --force      # redeploy/restart the same build (a .env, fqd.toml or secrets change)
~/fq-dogfood/deploy.sh 1a2b3c4d5e6f # roll back / pin (a unique prefix is fine for images already on the host)
tail -f ~/fq-dogfood/logs/deploy.log # what the hourly deploy.sh --auto did
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
`deploy.sh --force`. A new value in `.secrets/env` or `.env` is the
same story: an `env_file` is read when a container is *created*, which
`deploy.sh --force` does and a restart does not. `fq-cron.toml` is the
exception: the scheduler watches it and reloads on edit.

**Editing files in the volume.** The config and the agents live inside
`fq-dogfood_fq-data`, not on the host's filesystem. Edit them through
the daemon's container (`docker compose exec fqd sh`, then `vi` under
`/var/lib/factor-q`), or copy in and out:

```sh
docker compose cp ~/new-agent.md fqd:/var/lib/factor-q/agents/   # then fq reload
docker compose cp fqd:/var/lib/factor-q/fqd.toml ./fqd.toml       # out, to edit; cp back, then deploy.sh --force
```

One-line invocation summaries (#216): set `[summary] model = "<cheap-model>"`
in `fqd.toml` and `deploy.sh --force`; the daemon keeps a one-line,
cheap-model status per invocation on the dashboard. The model must be
priced (the ADR-0004 startup guarantee applies, so deploy config-first);
the summariser's own spend shows in `fq costs` as the reserved `summary`
agent. Unset = disabled.

If the dashboard shows a **"⚠ build skew"** banner (#168), it and the
daemon come from different builds. `deploy.sh` moves all four together
by construction, so in practice it means one container was recreated
by hand from a different tag; a `deploy.sh --force` cures it.

### Before any restart

`deploy.sh`, `deploy.sh --force` and the two procedures below all stop
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
worker's terminal state is `shutdown`, not `stale`. The `fq` client
prints tarpc INFO spans to stderr on every call (#535); `2>/dev/null` is
safe when reading its output.

A deploy that crosses an event `SCHEMA_VERSION` bump (2 → 3 with #510)
does not rebuild the projection — the projector continues from its
durable position and only new events are projected — but transcripts of
invocations recorded under the old version may not render until #409
is done. Do **not** delete `cache/projection.db` across such a bump: a
rebuild replays every event and silently drops the ones it cannot parse
(#409).

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
4. `deploy.sh --force`: it recreates the three consumers with the new
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

## Continuous delivery: `deploy.sh --auto`

The [crontab](crontab) runs `deploy.sh --auto` hourly. It is the same
deploy as by hand — pull `main-latest`, resolve it to a commit, prove
every image reports it, drain, up, verify — with three differences for
running unattended:

- **Quiet when there is nothing to do.** One timestamped line in
  `logs/deploy.log` per run; the narration starts only when a deploy is
  actually going to happen.
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
- **It tells you what it did.** A deploy, a rollback and a failure each
  go through [`notify.sh`](notify.sh) (below). So does a deferral that
  has gone on for `FQ_DEFER_WARN_HOURS` (6): an invocation stuck in
  flight, or a container nobody paired, would otherwise keep every merge
  off the host with nothing but a quiet line an hour in the log. Reported
  once per target build, in `.deploy.deferred`.

Not Watchtower, deliberately: the five images are not published
atomically (a poll mid-publish would recreate the daemon on one build
and the dashboard on another), and an updater without a readiness check
and a rollback is not delivery, it is roulette. The cadence is hourly
rather than per-merge because the fleet merges its own PRs. `deploy.sh`
by hand still works at any time; the two share a lock.

## Notifications: `notify.sh`

The crontab sends every script's output to a file under `logs/`, so
cron mail never fires; without a channel of its own, a rollback or a
full disk would sit in a log until someone looked. `notify.sh <subject>`
(body on stdin) is that channel: it runs `FQ_NOTIFY_HOOK` from `.env` —
a shell command given the subject as `$1` and the body on stdin — and
appends every message to `logs/notify.log` whether or not a hook is set.
`.env.example` has one-line hooks for ntfy, Slack and mail;
`./notify.sh --test` proves the one you chose. A missing hook is one
line on stderr in the calling script's log, next to the thing it could
not deliver; a failing hook is reported the same way and never fails
its caller.

What goes through it, all unattended: `deploy.sh --auto`'s deploys,
rollbacks, failures and long deferrals; `hygiene.sh`'s warnings, one
message per run; a failed `backup.sh --auto`. A deploy by hand tells
its terminal and nothing else.

Machine-scrapeable metrics from the daemon itself are
[#342](https://github.com/bricef/factor-q/issues/342), which this does
not touch: it is the host's scripts speaking, not the runtime.

## Hygiene: `hygiene.sh`

Every 30 minutes from the crontab, into `logs/hygiene.log`: the age of
the newest backup set (warns past `FQ_BACKUP_STALE_HOURS`, 36 — a
nightly that has quietly stopped), the disk docker lives on (warns above
`FQ_DISK_WARN_PCT`, 80% — a full disk has killed the daemon and the
broker before), `docker system df`, the instance volume by subtree, the
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

`backup.sh` (nightly at 03:30 from the crontab, `--auto` so it defers
while an invocation is in flight) takes a **consistent** copy: it stops
the scheduler, the daemon (a drain), the watcher and the broker, copies
the instance volume minus `build/` and `workspace/` and the broker's
JetStream store into `backups/<utc-stamp>/` as two tarballs with
`SHA256SUMS` and a `MANIFEST` (the tag it was taken at), and starts the
stack again — a minute or two with the dashboard showing "runtime
unreachable". Copying SQLite and JetStream files under a live writer
would not be guaranteed to restore, which is the only property a backup
has. `FQ_BACKUP_KEEP` (7) sets are kept on the host; `FQ_BACKUP_HOOK` is
a command run with the finished set's directory, for the off-host copy
— the on-host copy alone does not survive the host. Unattended, a
failed backup goes through `notify.sh`; a backup that stops happening
is `hygiene.sh`'s stale-set warning.

`restore.sh <set> [--yes]` is the other half: it verifies the checksums,
takes the stack down (volumes kept), refuses to overwrite a volume that
already has content unless `--yes`, fills both volumes from the tarballs
as root and hands them to the runtime user, and brings the stack up on
the set's tag (or `.env`'s, if set). The pairing comes back with
`state/client/`, so `fq status` answers immediately.

**The drill**, once, and again after anything touches the layout: on a
scratch VM, `bootstrap.sh`, copy a backup set over, `restore.sh <set>`,
then `docker compose exec fqd fq status` and `fq invocation list` show
the instance as it was. Clone-and-restore is cheap on a dedicated VM;
the ADR's acceptance asks for it and so does the production-readiness
review's Phase 3.

## Migrating an instance onto the stack

From a host running the launcher shape (or from one host to another):
the state to carry is the `~/fq-dogfood` tree minus `releases/`,
`current`, `logs/` and `.secrets/`, plus the old broker's JetStream
volume. Rotating the edge identity is simpler than moving it and costs
one re-pair and one dashboard token; the steps below move it.

1. **Stop the old shape.** `./current/fq down` (or `deploy.sh` of the
   launcher era with nothing to deploy), then SIGTERM the watcher, cron
   and dashboard, then `docker compose -f infra/docker-compose.yml
   down` for the old broker and proxy. Confirm with `fq workers list`
   beforehand that the worker ends `shutdown`, not `stale`.
2. **Bootstrap the new shape** (above) on the target host, with the same
   secrets and the same broker token. Do not run `deploy.sh` yet.
3. **Seed the volume from the tree**: `fqd.toml` (with the three
   settings for this shape — the edge bind changes from `127.0.0.1:9470`
   to `0.0.0.0:9470`), `fq-cron.toml`, `agents/`, `cache/` (the three
   stores), `workspace/` if anything is suspended, and the edge identity
   — `~/.local/state/factor-q/edge/` by default, or `[state] directory`
   if set — as `state/edge/`. Copy in through the image as in Bootstrap;
   the copy runs as the runtime user, so ownership comes out right.
4. **Move the event log.** The old compose project's volume is
   `infra_nats-data`; the new one is `fq-dogfood_nats-data`:

   ```sh
   docker volume create fq-dogfood_nats-data
   docker run --rm -v infra_nats-data:/from:ro -v fq-dogfood_nats-data:/to alpine sh -c 'cp -a /from/. /to/'
   ```

   Likewise `infra_caddy-data` → `fq-dogfood_caddy-data` to keep the
   certificates (or let Caddy re-issue them).
5. **`deploy.sh`**, then pair as in Bootstrap. With the identity copied,
   the operator's existing pairing and the dashboard's token stay valid
   — `FQ_EDGE` in `dashboard.env` is the same `127.0.0.1:9470`. Re-run
   the "after a deploy" checks.
6. **Retire** the tree's `releases/`, `current` and `logs/`, and the
   old `infra/docker-compose.yml`; the host's `gh` login is no longer
   read by anything (GH_TOKEN is literal in `.secrets/env`).

Not built yet: probes on the adapter and dashboard images
([#587](https://github.com/bricef/factor-q/issues/587)), and a
notification channel for the warnings `hygiene.sh` and `deploy.sh
--auto` write to their logs and cron mail — the metrics and alerting of
[#342](https://github.com/bricef/factor-q/issues/342).
