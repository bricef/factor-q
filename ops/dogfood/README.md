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
└── .deploy.lock             # deploy.sh's flock

docker volume fq-dogfood_fq-data → /var/lib/factor-q in the daemon's container:
    fqd.toml, fq.toml, fq-cron.toml, agents/, state/ (edge identity + the
    container's pairing), cache/ (the three stores), workspace/, build/, home/
docker volume fq-dogfood_nats-data   → the event log
docker volume fq-dogfood_caddy-data, fq-dogfood_caddy-config → certificates; regenerable
```

`deploy.sh` runs from a repo checkout (it is the bootstrap and cannot
live inside the thing it swaps); everything else on the host is copied
from this directory once and edited in place. Secrets are `chmod 600`
and never committed (`ops/dogfood/.secrets/` is git-ignored so a local
`docker compose config` can create them).

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

## Bootstrap (one-time per host)

Docker Engine with the compose plugin (`docker compose version` ≥ 2).
If the packages under `ghcr.io/bricef` are private, `docker login
ghcr.io` with a token that has `read:packages`. Then:

```sh
mkdir -p ~/fq-dogfood/.secrets && chmod 700 ~/fq-dogfood/.secrets
cp ops/dogfood/compose.yml ~/fq-dogfood/ && cp -r ops/dogfood/infra ~/fq-dogfood/
install -m 644 ops/dogfood/.env.example        ~/fq-dogfood/.env                      # then set FQ_TAG (or let deploy.sh)
install -m 600 ops/dogfood/env.example         ~/fq-dogfood/.secrets/env              # then edit: keys, GH_TOKEN, the broker token
install -m 600 ops/dogfood/dashboard.env.example ~/fq-dogfood/.secrets/dashboard.env  # fingerprint + token come after first start
printf 'authorization { token: "%s" }\n' "$(openssl rand -hex 32)" > ~/fq-dogfood/.secrets/nats-auth.conf && chmod 600 ~/fq-dogfood/.secrets/nats-auth.conf
# .secrets/caddy.env: DASH_USER, DASH_HASH (docker run --rm caddy:2 caddy hash-password), DASH_COOKIE (openssl rand -hex 32)
```

Put the same broker token in the three `.secrets/env` variables
(`FQ_NATS_TOKEN`, and as userinfo in `GHW_NATS_URL` and
`FQCRON_NATS_URL`).

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

**First deploy, then pair.** `ops/dogfood/deploy.sh` pulls, proves and
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
ops/dogfood/deploy.sh              # upgrade to the newest main build (the images' main-latest)
ops/dogfood/deploy.sh --force      # redeploy/restart the same build (a .env, fqd.toml or secrets change)
ops/dogfood/deploy.sh 1a2b3c4d5e6f # roll back / pin (a unique prefix is fine for images already on the host)
cd ~/fq-dogfood && docker compose ps            # every service, its state and health
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
([#587](https://github.com/bricef/factor-q/issues/587)), the post-deploy
health gate with automatic rollback
([#339](https://github.com/bricef/factor-q/issues/339)), and host hygiene
— a disk alert and a prune job for `build/` and terminal workspaces
(#587 slice 5).
