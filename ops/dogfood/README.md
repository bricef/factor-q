# Dogfood deploys — hermetic, versioned, reversible

Deploy tooling for the dogfood instance (issue #102). The contract:

- **CI builds, the host fetches.** Every merge to main,
  [main-artifacts.yml](../../.github/workflows/main-artifacts.yml) builds
  static musl binaries (`fq`, `fq-cas`, `github-watcher`, `fq-cron`) plus the
  launchers in this directory, packages them with a sha256
  ([package.sh](../../scripts/package.sh)), and publishes the bundle to
  the rolling `main-latest` pre-release. The dogfood host never compiles.
- **Every deployed build is kept; `current` picks the active one.**
  [deploy.sh](deploy.sh) verifies the checksum and the embedded commit
  SHA, installs into `releases/<sha>/`, drains the daemon (ADR-0027 —
  escalating past the drain deadline to a confirmed `fq down --now`,
  and only then SIGINT, #63), atomically flips the `current` symlink,
  relaunches, and confirms both processes run from the new release dir
  (`/proc/<pid>/exe`, not log grepping). Exit 0 means you are on the
  target SHA.
- **Rollback is local and instant**: `deploy.sh <previous-sha>` — no
  network, no rebuild, just a symlink flip through the same
  drain/verify path. `deploy.sh` keeps the newest `KEEP_RELEASES`
  (default 5) dirs and prunes the rest.
- **The environment is declared, not ambient.** Both launchers source
  exactly one file, `.secrets/env` ([template](env.example)) — nothing
  else reaches the processes' environment.

## Host layout (`~/fq-dogfood`, override with `FQ_DOGFOOD`)

```text
fq-dogfood/
├── current -> releases/<sha>/   # the active build (symlink)
├── releases/<sha>/              # fq, fqd, fq-cas, fq-dashboard, github-watcher, fq-cron + launchers
├── fqd.toml                     # daemon config — host-side, `fq reload` to apply
├── fq-cron.toml                 # scheduled jobs — host-side; editing deploys/reloads them
├── agents/                      # agent definitions — host-side; canonical copies of
│                                #   repo-tracked ones live in ops/dogfood/agents/ —
│                                #   install with scp, e.g. backlog-groomer.md (weekly
│                                #   groom: cron `fq trigger backlog-groomer` until #257)
├── .secrets/env                 # the single declared environment (chmod 600)
├── infra/                       # NATS compose + config (copied from ./infra)
├── logs/                        # fq-run.log, watcher.log, cron.log
└── workspace/ cache/ reports/   # runtime state
```

The launchers (`run.sh`, `watcher.sh`, `cron.sh`) ship *inside* the artifact bundle
so they are versioned with the binaries they launch and roll back with
them. `deploy.sh` itself runs from a repo checkout — it is the
bootstrap, and can't live inside the thing it swaps.

## Bootstrap (one-time per host)

```sh
mkdir -p ~/fq-dogfood/{releases,logs,agents,.secrets} && chmod 700 ~/fq-dogfood/.secrets
cp -r ops/dogfood/infra ~/fq-dogfood/
install -m 600 ops/dogfood/env.example ~/fq-dogfood/.secrets/env  # then edit
# fqd.toml: copy an existing instance config (`fq init` writes a
#           client-side fq.toml, which is a different file)
# fq-cron.toml: install the host job schedule before starting cron.sh
ops/dogfood/deploy.sh
setsid ~/fq-dogfood/current/cron.sh </dev/null &  # cron.sh appends to logs/cron.log
```

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

## Routine operations

```sh
ops/dogfood/deploy.sh              # upgrade to the newest main build
ops/dogfood/deploy.sh --force      # redeploy/restart the same build (e.g. env change)
ops/dogfood/deploy.sh 1a2b3c4      # roll back / pin (sha prefix ok)
ls ~/fq-dogfood/releases           # deploy history on this host
```

Config and agent-definition changes don't need a deploy at all:
`fq reload` hot-swaps the registry (Design Principle 8). A new provider
key is the exception — add it to `.secrets/env` and `deploy.sh --force`,
since only launch reads the env file.

One-line invocation summaries (#216): set `[summary] model = "<cheap-model>"`
in `fqd.toml` (and `fq reload`-or-restart) and the daemon keeps a one-line,
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

**One-time setup, and it is new.** The dashboard reads over the
daemon's authenticated edge, which means it needs an identity — it is
the first process other than the operator's own CLI to need one. Three
variables in `.secrets/env` (see `env.example`, which carries the
commands):

1. **Move the edge off port 9472.** `[edge] bind` defaults to
   `127.0.0.1:9472`, which is the port the dashboard itself serves on
   and the one Caddy proxies to. Set `bind = "127.0.0.1:9470"` under
   `[edge]` in `fqd.toml`, restart the daemon, and set `FQ_EDGE` to
   match. `dashboard.sh` refuses to launch without `FQ_EDGE` rather
   than defaulting into the collision.
2. **Pin the daemon.** `FQ_EDGE_FINGERPRINT` is the SHA-256 the daemon
   printed when it provisioned its identity (`edge: certificate
   fingerprint`).
3. **Mint an attenuated token.** `FQ_EDGE_TOKEN` must be an
   *attenuation* of the admin token, not the admin token itself:

   ```
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
the flip, and any supervisor (systemd is deliberately out of scope; the
launchers are detached with `setsid`, NATS restarts via docker's
`restart: unless-stopped`).
