# Dogfood deploys — hermetic, versioned, reversible

Deploy tooling for the dogfood instance (issue #102). The contract:

- **CI builds, the host fetches.** Every merge to main,
  [main-artifacts.yml](../../.github/workflows/main-artifacts.yml) builds
  static musl binaries (`fq`, `fq-cas`, `github-watcher`) plus the
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
├── releases/<sha>/              # fq, fq-cas, fq-dashboard, github-watcher + launchers
├── fq.toml                      # instance config — host-side, `fq reload` to apply
├── agents/                      # agent definitions — host-side
├── .secrets/env                 # the single declared environment (chmod 600)
├── infra/                       # NATS compose + config (copied from ./infra)
├── logs/                        # fq-run.log, watcher.log
└── workspace/ cache/ reports/   # runtime state
```

The launchers (`run.sh`, `watcher.sh`) ship *inside* the artifact bundle
so they are versioned with the binaries they launch and roll back with
them. `deploy.sh` itself runs from a repo checkout — it is the
bootstrap, and can't live inside the thing it swaps.

## Bootstrap (one-time per host)

```sh
mkdir -p ~/fq-dogfood/{releases,logs,agents,.secrets} && chmod 700 ~/fq-dogfood/.secrets
cp -r ops/dogfood/infra ~/fq-dogfood/
install -m 600 ops/dogfood/env.example ~/fq-dogfood/.secrets/env  # then edit
# fq.toml: copy an existing instance config, or generate with `fq init`
ops/dogfood/deploy.sh
```

Migrating an existing in-place instance (pre-#102: host-built binary,
untracked `run.sh`/`watcher.sh`/`redeploy.sh`): fold any local secrets
into `.secrets/env`, delete the legacy scripts, `bin/`, and `fq.rollback`,
then run `deploy.sh`. State (`fq.toml`, `agents/`, `cache/`, `workspace/`,
the NATS volume) is untouched by deploys.

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
in `fq.toml` (and `fq reload`-or-restart) and the daemon keeps a one-line,
cheap-model status per invocation on the dashboard's invocation surfaces —
what work was expected, what it is doing now, how it ended. The model must
be priced (the ADR-0004 startup guarantee applies, so deploy config-first);
the summariser's own spend shows in `fq costs` as the reserved `summary`
agent and is never charged to an invocation. Unset = disabled, zero change.

The operator dashboard (read-only web view, #105) rides in the bundle:
enable `[read_service]` in `fq.toml` (one-time), and `deploy.sh` stops
and relaunches it with the daemon and watcher — it must run the same
build as the daemon, because the read-service RPC is a length-framed
binary codec and a cross-build dashboard fails to decode responses,
rendering "runtime unreachable" (the #154-skew incident). Manual
launch, if ever needed:
`setsid ./current/dashboard.sh >> logs/dashboard.log 2>&1 </dev/null &`.
Reach it via SSH tunnel to `127.0.0.1:9472`, or through the public
door: the infra compose runs Caddy serving `https://dev.lambda.works`
(basic-auth; TLS-only — plain HTTP is refused, not redirected; the
dashboard itself stays loopback-bound). One-time setup: write
`.secrets/caddy.env` (chmod 600) containing `DASH_USER=<user>` and
`DASH_HASH=<bcrypt>` (hash via `docker run --rm caddy:2 caddy
hash-password`), then `docker compose -f infra/docker-compose.yml up
-d`. Caddy is the one process deploy.sh does not manage (it is
docker-supervised, `restart: unless-stopped`).

If the dashboard shows a **"⚠ build skew"** banner (#168), it detected —
over the frozen `ReadService::version` probe — that the daemon comes
from a different build than itself. Pages still render whatever
decodes, but data may be partial or failing (the wire is a binary
codec); the remedy is always the same: redeploy so both run one build
(`deploy.sh` does this by construction — the banner in practice means
someone launched a process by hand from the wrong `releases/<sha>/`).
`fq-dashboard --version` prints the dashboard's build SHA.

Not built yet, by design (see #102): health-gate + auto-rollback after
the flip, and any supervisor (systemd is deliberately out of scope; the
launchers are detached with `setsid`, NATS restarts via docker's
`restart: unless-stopped`).
