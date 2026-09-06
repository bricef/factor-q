# Dogfood host migration — execution plan

> **Opened 2026-09-05.** Executes the move
> [ADR-0035](../../adrs/accepted/0035-container-image-and-compose-supervision.md)
> decided and [#587](https://github.com/bricef/factor-q/issues/587) built
> towards: the dogfood instance leaves the maintainer's dev machine, where
> it runs as `setsid` launchers from a release tree, for a dedicated VM
> running the compose stack. Everything the repository can provide is on
> `main` (#588–#591, #596, #599, #601). This plan is the day itself, and
> the days either side of it. Written as a hand-off: a session with no
> prior context should be able to read this file and the
> [ops README](../../../ops/dogfood/README.md), and run the move with the
> maintainer.

## Assumptions

- **A remote, dedicated VM** — Debian or Ubuntu, root at bootstrap,
  reached over SSH, with a public address so `dev.lambda.works` can
  point at it and Caddy can answer the TLS-ALPN challenge on 443. The
  provider firewall admits 22 and 443 inbound and nothing else; the
  stack publishes nothing else. Outbound it needs `ghcr.io`, GitHub, the
  Anthropic API and wherever `FQ_NOTIFY_HOOK` delivers. A VM with no
  public address changes only the DNS and Caddy step: the dashboard is
  reached by SSH tunnel and Caddy stays stopped.
- **Both hosts run at once** for the duration. The old host is untouched
  until retirement, which is the rollback; the new host is rehearsed on
  before the day.
- **The edge identity is copied, not rotated.** ADR-0035 lists rotating
  as the simpler path; copying is chosen here because the operator's
  client reaches the new host at the same `127.0.0.1:9470` through a
  tunnel, so every existing pairing and the dashboard's token stay valid
  and the day has two fewer steps. Rotation is the fallback if
  `state/edge/` cannot be read off the old host — then re-pair and
  re-mint per the README's "First deploy, then pair".
- **The transfer is a restore set.** The old host runs the launcher
  shape, so `backup.sh` cannot run there; the same set is packaged by
  hand (below) and `restore.sh` fills the new host's volumes from it.
  The migration is therefore the restore drill the ADR asks for, run
  twice: once as rehearsal, once for real.

## Where things stand

- **The old host** runs the launcher shape from `~/fq-dogfood`:
  `releases/<sha>/`, `current`, `.secrets/`, `infra/docker-compose.yml`
  for the broker and proxy (compose project `infra`, volumes
  `infra_nats-data` and `infra_caddy-data`), the daemon's `cache/`
  (three stores), `agents/`, `fqd.toml`, `fq-cron.toml`, and the edge
  identity under `~/.local/state/factor-q/edge/` or `[state] directory`
  if `fqd.toml` sets it. It was at `9477254` (2026-08-25) when the
  [phase-0 plan](2026-09-04-production-readiness-phase-0.md) was written;
  `./current/fq --version` says what it is now. Whatever it is, it is
  before #510, so the move also crosses the event `SCHEMA_VERSION` 2 → 3
  bump (see risks).
- **The new host** does not exist yet.
- **The compatibility read** of the live agent definitions against the
  image has not been done (pre-flight 3).

## Pre-flight (days before; no downtime)

1. **Provision and bootstrap.** Size for the daemon's limits plus the
   rest: `FQ_CPUS`/`FQ_MEMORY` in `.env` (defaults 6 / 24g) with headroom
   for the broker, the proxy and the OS; disk for `build/` up to
   `FQ_BUILD_CACHE_MAX_GB` (60), `FQ_BACKUP_KEEP` (7) sets, the images
   and the workspaces — 150 GB is a floor, not a target. Then, as root:

   ```sh
   curl -fsSL https://raw.githubusercontent.com/bricef/factor-q/main/ops/dogfood/bootstrap.sh | sudo bash
   ```

   It prints the steps only a human can do; do them: `ANTHROPIC_API_KEY`
   and `GH_TOKEN` into `.secrets/env` (the same values as the old host's;
   the broker token bootstrap generated stays — nothing in the event log
   depends on it), `DASH_USER`/`DASH_HASH` copied from the old
   `caddy.env` (a new `DASH_COOKIE` logs every browser out once; copy the
   old one to avoid that), `docker login ghcr.io` as `fq` if the packages
   are private, `FQ_NOTIFY_HOOK` in `.env` and `./notify.sh --test`.
   **For the rehearsal, leave `GH_TOKEN` a placeholder** — see 4.
2. **Pick the build.** `deploy.sh` resolves `main-latest` on the day, but
   `restore.sh` needs a literal tag; write it now and rehearse on it:

   ```sh
   docker pull ghcr.io/bricef/fq-dogfood:main-latest
   docker run --rm ghcr.io/bricef/fq-dogfood:main-latest --version   # fqd 0.x.y (<sha> …)
   sed -i 's/^FQ_TAG=.*/FQ_TAG=<sha>/' ~fq/fq-dogfood/.env
   ```

3. **The compatibility read.** Every live definition in the old host's
   `agents/` is read — not edited — against what the `fq-dogfood` image
   offers, and anything it would now need is listed. The tools on the
   exec baseline `PATH`: `cargo`, `rustc`, `cargo fmt`, `cargo clippy`,
   `go`, `node`, `npx`, `just`, `gh`, `git`, `jq`, `nats-server`,
   `sccache` (`just docker-check` proves the list). The environment an
   agent's processes get is **empty** unless the definition allowlists
   variables in `sandbox.env`; the image sets `CARGO_HOME`,
   `CARGO_TARGET_DIR`, `RUSTC_WRAPPER=sccache`, `SCCACHE_DIR`, `GOCACHE`,
   `GOMODCACHE` under `build/` and `HOME` under `home/`, and an agent
   that compiles wants those (the runtime README, "Environment
   variables"). Anything the old host had on its `PATH` that the image
   lacks — a language runtime, a CLI — is either added to the Dockerfile's
   `tools` stage before the day or dropped from the definition. Record
   the findings on #587.
4. **Rehearse: a set from the old host, restored on the new.** Take the
   set (next section) at a quiet moment — a brief stop of the old daemon,
   `fq invocation list` showing nothing in flight first, so nothing is
   suspended in it and nothing resumes on the rehearsal host. Restore it:

   ```sh
   sudo -iu fq
   cd ~/fq-dogfood && ./restore.sh backups/<stamp>
   docker compose stop github-watcher fq-cron caddy
   ```

   Two watchers on one repository both claim its issues and two
   schedulers both fire its jobs, so on the rehearsal host both stop the
   moment the stack is up (and `GH_TOKEN` is a placeholder in case they
   win the race). Caddy stops because DNS still points at the old host:
   its ACME attempts would fail and count against the certificate
   authority's failed-validation limit. Then the checks: pair the
   container's `fq` (the pairing is not in the set), `fq status` with the
   new version and the agents loaded, `fq invocation list` showing the
   history, `docker compose ps` with `fqd`, the watcher-less stack
   healthy, the dashboard through a tunnel to `127.0.0.1:9472` with the
   old `dashboard.env` values (the identity came across, so the old
   token is valid), and a transcript of a recent invocation opening. A
   check that fails here fails on the day; fix it here.
5. **DNS.** Lower the TTL on `dev.lambda.works` to a few minutes now, so
   the switch on the day propagates while the certificate is issued.
6. **Hold the crontab on the new host** until acceptance: `crontab -u fq
   -r`. `deploy.sh --auto` would otherwise move the rehearsed instance to
   a newer build at :17, and `backup.sh --auto` would take sets of a
   rehearsal. `bootstrap.sh` — or `crontab -u fq
   /opt/factor-q/ops/dogfood/crontab` — puts it back at the end.

## The set, from the launcher shape

On the old host, with the daemon stopped (rehearsal: briefly; the day:
for good). The layout inside `fq-data.tgz` is the volume's:

```sh
cd ~/fq-dogfood
S=backups/$(date -u +%Y%m%dT%H%M%SZ); mkdir -p "$S" stage/state
cp fqd.toml fq-cron.toml stage/;  [ -f fq.toml ] && cp fq.toml stage/
cp -a agents cache stage/
cp -a ~/.local/state/factor-q/edge stage/state/     # or [state] directory's edge/
# workspace/: only if `fq invocation list` shows something suspended — then cp -a workspace stage/
tar -czf "$S/fq-data.tgz" -C stage .
docker run --rm -v infra_nats-data:/nats:ro -v "$PWD/$S:/out" alpine tar -czf /out/nats-data.tgz -C /nats .
( cd "$S" && sha256sum fq-data.tgz nats-data.tgz > SHA256SUMS )
printf 'taken=%s\nfq_tag=%s\nproject=fq-dogfood\nwas_up=launcher-shape\n' "$(basename "$S")" "<sha from pre-flight 2>" > "$S/MANIFEST"
rm -rf stage
rsync -a "$S" fq@<new-host>:fq-dogfood/backups/
```

`stage/fqd.toml` is edited before the tar for the three settings the
compose shape needs (README, "Seed the instance volume"): `[edge] bind
= "0.0.0.0:9470"`, `[workspace] path = "/var/lib/factor-q/workspace"`,
`[nats] token_env = "FQ_NATS_TOKEN"`. Directory settings in it are
ignored — the image's environment pins them. `restore.sh` verifies the
checksums, fills both volumes as root and hands them to the runtime
user, so ownership on the old host does not matter.

## The day

Expected downtime: the minutes between stopping the old daemon and
`fq status` answering on the new host — a copy, a transfer and a
restore. Announce it; the fleet's `status:ready` issues wait.

1. **Quiesce the old host, in this order.** `./current/fq invocation
   list` — wait for anything in flight to finish (or drain and accept a
   suspended invocation, README "Before any restart"). Then SIGTERM the
   **watcher and cron first** (no new claims, no new fires), then `fq
   down` (the drain), then `docker compose -f infra/docker-compose.yml
   down` for the broker and proxy. `fq workers list` must show the worker
   ended `shutdown`, not `stale`. The dashboard can stay up; it will
   render "unreachable" until it is stopped in retirement.
2. **Take the set** as above; `rsync` it across.
3. **Restore on the new host**, with `.secrets/env` now carrying the
   real `GH_TOKEN` and `GHW_REPO=bricef/factor-q`:

   ```sh
   sudo -iu fq
   cd ~/fq-dogfood && ./restore.sh backups/<stamp> --yes    # --yes: over the rehearsal's volumes
   ```

   It brings the whole stack up on `FQ_TAG` — the watcher and the
   scheduler included, which is right now that the old ones are down.
4. **Pair, and the dashboard.** Pair the container's `fq` (README, "First
   deploy, then pair"). `.secrets/dashboard.env` holds the old token and
   fingerprint from the rehearsal; `docker compose ps` should show
   `fq-dashboard` healthy already.
5. **DNS and the certificate.** Point `dev.lambda.works` at the new
   host; `docker compose up -d caddy`; `docker compose logs -f caddy`
   until the certificate is obtained. `https://dev.lambda.works` answers
   with the dashboard behind basic-auth. (`infra_caddy-data` could be
   copied across instead to keep the old certificate; re-issuing is one
   fewer volume to move and Let's Encrypt's normal limits allow it.)
6. **Accept** (below). Then `crontab -u fq /opt/factor-q/ops/dogfood/crontab`
   and `./notify.sh --test`. The next `:17` is the first unattended
   deploy; the first nightly `backup.sh --auto` is the first real set.
7. **Tell #587** what happened, with the times.

## Acceptance

All of these on the new host, with the old daemon down:

- `docker compose ps`: every service running, `fqd`, `github-watcher`,
  `fq-cron` and `fq-dashboard` healthy on their own probes.
- `docker compose exec fqd fq status`: the new version, the agents loaded
  (the same count as the old host), the projector consumer caught up.
- `fq invocation list` shows the old host's history; `fq costs` shows the
  old spend.
- One trigger end to end: label a scratch issue `status:ready`, the
  watcher claims it within a poll, the agent runs on the new host, the
  outcome lands back on the issue.
- The dashboard at `https://dev.lambda.works` with no build-skew banner,
  and a recent transcript rendering.
- `hygiene.sh --report` clean; `logs/notify.log` has the test message
  and the hook delivered it.

## Rollback

Until retirement the old host is intact; rolling back is the day in
reverse and needs one thing done first: **stop the new host's watcher
and scheduler** (`docker compose stop github-watcher fq-cron`) before
the old ones start, for the same double-trigger reason. Then on the old
host: `docker compose -f infra/docker-compose.yml up -d`, and the four
launchers from `current/` exactly as the old `deploy.sh` started them;
point DNS back. Events recorded on the new host in the meantime stay
there — an invocation that ran on the new host is not in the old
projection — so a rollback after real traffic should take a set off the
new host first (`backup.sh` works there) and bring it back the same
way, or accept the gap.

## Retirement

After a week green — the unattended deploy has moved the instance at
least once, a nightly set exists and `hygiene.sh` has not warned —
retire the old host: `docker compose -f infra/docker-compose.yml down
-v` (the copied volumes), remove `releases/`, `current` and `logs/`, and
the `cache/` and identity directories the set carried. `GH_TOKEN` and
the provider key are the same on both hosts, so nothing is revoked by
the move; rotate them on their own schedule.

Then the chores the move unblocks:

- STATUS.md's dogfood paragraph (it says the instance has not moved).
- The phase-0 plan's "The dogfood instance" bullet.
- ADR-0035's `Implementation:` line — the last "not built" is this move.
- The ops README's "Migrating an instance onto the stack": after the
  move it is a runbook for the next host, not the launcher shape; fold
  this plan's set-packaging into it or point at `backup.sh`.
- Close #587. Move this plan to `closed/` with what actually happened.

## Risks

| Risk | Where it bites | What this plan does |
|---|---|---|
| Two watchers or two schedulers on one repository | rehearsal; rollback | Stop the pair on whichever host is not serving, before the other's starts; placeholder `GH_TOKEN` on the rehearsal host |
| A suspended invocation in the set resumes on the wrong host | rehearsal | Take the rehearsal set idle; on the day the old daemon is down for good |
| The event `SCHEMA_VERSION` 2 → 3 bump (#510) | first start on the new host | The projector continues from its durable position; transcripts recorded under v2 may not render until #409 — do **not** delete `cache/projection.db` to force a rebuild, it silently drops what it cannot parse |
| An agent definition assumes a tool or a variable the image does not give it | first trigger | Pre-flight 3 reads every definition; `sandbox.env` allowlists; Dockerfile `tools` stage for a missing tool |
| The identity cannot be read off the old host | pre-flight 4 | Rotate: fresh identity on first start, re-pair, re-mint the dashboard token |
| ACME failed-validation limit | rehearsal; the day | Caddy stopped until DNS points at the host; TTL lowered ahead |
| `deploy.sh --auto` fires at :17 mid-move | rehearsal; the day | Crontab held until acceptance; the scripts also share a lock |
| A full disk on the new host | first weeks | `hygiene.sh` every 30 minutes with `FQ_NOTIFY_HOOK`; `FQ_BUILD_CACHE_MAX_GB` |
| A wrong `fqd.toml` for the shape | restore | The three settings are edited into the staged copy. `restore.sh` brings the stack up without a readiness wait, so after it: `docker compose logs fqd` must show `Runtime ready`, not a refusal; a refusal is fixed in the volume (`docker compose cp`) and `deploy.sh --force` |
