# ADR-0036: The stack schedules its own operations — an ops image and a scheduler service replace the host crontab

## Status

Draft — proposed 2026-09-12. Refines
[ADR-0035](../accepted/0035-container-image-and-compose-supervision.md):
the image-per-binary, compose-as-supervisor, tag-bump-deploy shape stands
unchanged, and this ADR moves the last host-side pieces of it — the
crontab and the five operations scripts — into the stack. It narrows
ADR-0035's clause 7 (the runtime socket is never mounted into any of our
containers) to *never into a container that runs the daemon or an agent*,
with that clause's reason intact. At acceptance ADR-0035's Status line
gains a `Refined by ADR-0036` pointer; until then it is untouched.

Implementation: pending — nothing is built. The worked example that
prompted it is
[#707](https://github.com/bricef/factor-q/pull/707) (merged 2026-09-12):
a change to what the hourly deploy's notification says reached `main`
with CI green and changed nothing on the dogfood host, because the host
runs a *copy* of `deploy.sh` that only a re-run of `bootstrap.sh`
refreshes.

## Context

ADR-0035 made the container image the deployable unit and compose the
supervisor, and the dogfood instance has run that way since 2026-09-12.
What is left on the host outside compose is small and load-bearing:

- **Three crontab lines** (`ops/dogfood/crontab`, installed for the
  deploy user by `bootstrap.sh`): `deploy.sh --auto` hourly at :17 — the
  continuous delivery of ADR-0035 clause 5, with its idle check and
  automatic rollback; `hygiene.sh` every thirty minutes — disk, the
  bounded build cache, dangling images, the age of the newest backup;
  `backup.sh --auto` nightly at 03:30 — a consistent set of the instance
  volume and the broker store, writers stopped.
- **Five scripts** — `deploy.sh`, `hygiene.sh`, `backup.sh`,
  `restore.sh`, `notify.sh` — copied by `bootstrap.sh` from a checkout
  into the instance directory `~fq/fq-dogfood/`, beside the files the
  host authors and bootstrap never touches (`.env`, `.secrets/`,
  `compose.override.yml`) and the files it produces (`logs/`,
  `backups/`, the deploy lock, the deferral marker).

The scripts are in the repository, reviewed and — since #707 — tested by
`just ops-ci`, but they reach the host by exactly one route: someone runs
`bootstrap.sh` again from a refreshed checkout. The README says so
("the one to re-run after a merge"). Until #707 nothing in CI ran under
`ops/dogfood` at all: CI's path filters name the runtime, the store, the
adapters, the dashboard and the images, and a formatting slip in the
hourly deploy's own script was the kind of change that broke silently.

So the system has two layers with two delivery mechanisms. The
application layer — six images, the stack definition, the schedule of
agent triggers that the `fq-cron` service publishes — is versioned,
built, published and rolled back per commit. The operations layer — the
schedule of the stack's *own* maintenance and the scripts that do it — is
a host artefact with a manual rollout, on a host that is otherwise meant
to be nothing but a container runtime and a directory of secrets.

The question this ADR answers: **where does the stack's recurring
maintenance live, and how does a change to it reach a host?** Two shapes
are on the table.

- **Keep the host crontab; ship the scripts in the daemon's image** and
  have the deploy install them from the image it has just verified. The
  scripts become versioned with the build; the schedule and the crontab
  install stay on the host; a second rollout mechanism exists beside the
  deploy.
- **An ops image and a scheduler service in the stack.** The scripts and
  the schedule ship in one image, tagged with the commit like every
  other; a compose service runs the schedule; each job runs as a
  one-shot container from the same image. The host keeps docker,
  compose, the stack definition and the host-authored files, and
  nothing else of ours.

One fact bounds the design. All three jobs *drive the stack*: the deploy
stops and starts services and rewrites `FQ_TAG`; the backup stops the
writers and reads their volumes; hygiene prunes images and empties a
subtree of the instance volume. A container that does those things needs
the container runtime's socket, which ADR-0035 clause 7 forbids for a
stated reason — agents run inside the daemon's container, and a socket
there is a root shell on the host. The reason is about *which* container.

## Decision

1. **An ops image, `fq-ops`, built from the same Dockerfile and published
   like the others.** A target `ops` in `services/fq-runtime/Dockerfile`
   holds the five scripts, `bash`, the docker CLI and compose plugin,
   `curl`, `jq`, `tar`, and a container-native cron runner (supercronic:
   foreground, logs to stdout, runs unprivileged, a single static
   binary), every one of them pinned and checksummed the way the `tools`
   stage pins `just`, `gh` and `sccache`, and gated by `just check-pins`
   where a pin has a twin. It is not distroless and never will be: it is
   the one image whose purpose is to run shell against the host's
   runtime. `just docker-build` builds it, `docker-check` runs its
   `--version` (which prints `fq-ops <twelve-hex sha>`, stamped at
   build), `docker-publish` pushes it to `ghcr.io/bricef/fq-ops` at the
   commit tag and `main-latest`, exactly as for the five images of
   ADR-0035.

2. **A scheduler service, `ops`, in the compose file.** It runs the image
   at `${FQ_TAG}` like every other service, `restart: unless-stopped`,
   with a `HEALTHCHECK` that asks whether the cron runner is alive
   (clause 8 of ADR-0035 applies). Its schedule is a crontab **inside the
   image** — the same `ops/dogfood/crontab`, now the image's, so the
   times are versioned with the scripts they run and reviewed in the same
   diff. Two mounts: the runtime's socket, and the instance directory at
   the same absolute path it has on the host (`FQ_DOGFOOD`, already in
   `.env`), so that compose inside the container reads the same
   `compose.yml`, `.env`, override and secrets the host does, and every
   host path a script hands to `docker run -v` resolves. It runs as the
   deploy user's uid with the docker group's gid — the posture the host
   crontab has today, no more. Schedule times are UTC by declaration.

3. **Every job is a one-shot sibling; nothing runs in the scheduler's
   own process.** Each crontab line is
   `docker compose run --rm --no-deps ops <verb> …`: the scheduler only
   fires, and the job runs in a fresh container from the same image with
   the same two mounts. This is the invariant that makes the deploy
   possible from inside the stack: `deploy.sh` ends with
   `docker compose up -d`, which recreates the `ops` service container
   on a new tag, and a job in a one-off container survives that — compose
   does not manage `run` containers under `up`. The deploy that replaces
   the scheduler is not killed by replacing the scheduler. By hand, the
   same verbs run the same way: `docker compose run --rm ops backup`,
   `… ops restore <set> --yes`, `… ops hygiene --report`,
   `… ops deploy <sha>`. The image's entrypoint is a small `fq-ops`
   dispatcher over the verbs; the default command is the scheduler.

4. **The ops image is in the deploy's verified set, and in the
   rollback.** `deploy.sh` pulls `fq-ops:<sha>` with the other images,
   proves its `--version` reports the tag, brings the stack up with it,
   waits on its probe like the adapters' and the dashboard's, and a
   rollback puts the previous `fq-ops` back with the previous scripts
   and the previous schedule. A script change therefore reaches a host
   the way a daemon change does — through the next hourly deploy — and
   is undone the same way. The service is not in the deploy's
   bring-down list (it must keep running to be replaced), and it is
   recreated last, by the `up`.

5. **ADR-0035 clause 7 narrows to its reason.** The runtime's socket is
   never mounted into a container that runs the daemon or an agent —
   `fq-runtime`, `fq-dogfood` — nor into the adapters or the dashboard,
   which have no use for it. It is mounted into exactly one image,
   `fq-ops`, which runs only the repository's operations scripts on the
   repository's schedule, executes nothing an agent wrote, and is the
   only image with a shell. That container is root-equivalent on the
   host, as the deploy user in the docker group already is. A socket
   proxy that admits only the API the scripts use (containers, images,
   exec, volumes) is a hardening step this ADR leaves open, not a
   precondition.

6. **The host layer, after this.** Docker Engine and the compose plugin;
   the instance directory with the tracked stack definition
   (`compose.yml`, `infra/`) and the host-authored files (`.env`,
   `.secrets/`, `compose.override.yml`); `logs/` for `notify.log` and
   `backups/` for the sets. No crontab. No scripts. `bootstrap.sh` stops
   installing a crontab and stops copying scripts; what it still lays out
   is what compose needs before the first `up`. A second host is docker,
   the directory, and `docker compose up -d`. Job output goes where every
   other service's does — the container log, rotated by the driver, read
   with `docker compose logs ops` — and `notify.log` stays the durable
   record of every message sent, because it is written on the bind mount.

## Rationale

**Why not the first shape — scripts in the daemon's image, crontab on
the host.** It halves the problem. The scripts become versioned, but the
schedule stays a host artefact, the crontab install stays in bootstrap,
and the deploy gains a second thing to install from an image. Two
delivery mechanisms for one layer is the condition to end, not to
refine.

**Why not host cron as it is.** The status quo works, and #707 shows its
cost: a reviewed, tested, merged change that does nothing until a human
remembers a command. The three lines are also the only reason a host
needs anything of ours installed outside the container runtime, which is
the difference between "a host with docker" and "a host we configured".

**Why not Watchtower or an updater container.** The README's objection
stands: the images are not published atomically, and an updater with no
readiness check and no rollback is roulette. The scheduler service is not
an updater; it fires the same `deploy.sh` with the same drain, checks and
rollback, from inside the stack instead of beside it.

**Why one-shot siblings and not jobs in the scheduler's process.** A
deploy that runs in the scheduler's process is killed by its own
`compose up` the moment the scheduler's image changes — before
verification, before rollback. Running each job as a sibling makes the
scheduler replaceable at any moment, which is what lets it be deployed
like everything else. It also keeps the scheduler tiny and the jobs'
lifetimes bounded: a job that hangs is a container to kill, not a
scheduler to restart.

**Why supercronic.** Container-native: runs in the foreground as PID 1,
logs each job's output to stdout with the job line, passes the
environment through, needs no root and no `/var/spool`. Busybox `crond`
wants root and drops the environment; a `while true; sleep` loop is a
scheduler nobody would review. It is one more pinned binary in an image
that already pins six.

**Why the socket, given clause 7.** Because the jobs' work *is* stopping,
starting, pruning and reading containers and volumes; there is no way to
do that work without the runtime's API, and the host user who does it
today holds the same power. Clause 7 exists so that an agent — code the
fleet wrote, running in the daemon's container — can never reach the
runtime. That stays exactly true. The narrowing names the one image that
may hold the socket and says what it may run: the repository's scripts,
and nothing from a workspace.

**Why the ops image rides `FQ_TAG` rather than its own pin.** A pin of
its own would reintroduce the manual rollout for the one image the ADR
exists to fold in, and would let the scripts and the stack they operate
drift apart. One tag means the deploy, the rollback and the coherence
check cover the operations layer for free.

## Consequences

- The whole system is `docker compose up -d`: images, schedule, scripts.
  A host that has docker and the instance directory has the instance.
- A change to a script or to the schedule lands with the build it was
  merged with, is checked by `just ops-ci` and `docker-check` in CI, and
  is rolled back with the build. `bootstrap.sh` re-runs stop being a step
  anyone has to remember.
- Every operations verb has one spelling on every host —
  `docker compose run --rm ops <verb>` — so the README's runbook stops
  depending on what the host has installed.
- **A supervisor inside the supervised.** If the `ops` service is down,
  nothing deploys, prunes or backs up until something brings it back.
  `restart: unless-stopped` covers a crash; the probe plus the deploy's
  health wait and rollback cover an image whose scheduler does not come
  up; a host whose container runtime is down stops everything today
  too. What remains is a scheduler that is up and not firing — the same
  failure a host cron has, and `hygiene.sh`'s backup-age warning is the
  existing detector for its most expensive form.
- **A root-equivalent container with a shell**, narrowing clause 7. The
  socket-proxy hardening is recorded as open. The bind mount also lets
  the ops container read `.secrets/`, as the deploy user can today; it
  holds no provider key in its own environment.
- A new image to build, check, publish and pin: `fq-ops` joins the five
  of ADR-0035 in `docker-build`, `docker-check`, `docker-publish`,
  `main-artifacts.yml` and the pin gate.
- Readings that were of the host become questions to the runtime:
  hygiene's "how full is docker's data root" is `docker system df`
  rather than `df` on a host path, and the `df` fallback goes.
- `notify.sh`'s signature names the host, not the container: the host
  name comes from `.env` rather than `hostname`.
- The deploy's bring-down list does not gain the `ops` service, and its
  `up` recreates the scheduler last; a rollback replays that sequence.
  Both are consequences of clause 3 and are where the implementation is
  most likely to be wrong, so the acceptance drill below exercises both.

## Implementation

One PR per slice, each behind the gates, in this order.

1. **The image.** The `ops` Dockerfile target, the `fq-ops` dispatcher
   with `--version`, the pins, `docker-build` / `docker-check` /
   `docker-publish` and `main-artifacts.yml`. Nothing on the host changes.
2. **The scheduler, running hygiene and backup.** The `ops` compose
   service with its two mounts, probe and the in-image crontab carrying
   the hygiene and backup lines as one-shot siblings; the host crontab
   shrinks to the deploy line. Job output moves to the container log;
   `notify.sh` takes the host name from `.env`; hygiene asks the runtime
   for its readings.
3. **The deploy as a sibling.** `deploy.sh --auto` moves into the
   crontab; `deploy.sh` pulls, verifies, ups and rolls back `fq-ops`
   with the rest; the host crontab goes; `bootstrap.sh` stops copying
   scripts and installing cron; the README's runbook is rewritten in
   `docker compose run --rm ops` verbs; ADR-0035 gains its pointer and
   this ADR moves to `accepted/`.
4. **Hardening, as its own decision:** the socket proxy; off-host copies
   of the backup sets (`FQ_BACKUP_HOOK`, unchanged by this ADR).

**Acceptance**, on the dogfood guest: one hourly deploy observed end to
end from the scheduler, with the notification naming the build and what
landed; one rollback drill — a tag whose `fq-ops` probe cannot pass is
deployed and the instance is back on the previous tag with the previous
schedule, unattended; a nightly set produced by the scheduler and
restored by `docker compose run --rm ops restore`; `hygiene --report`
from the scheduler's log; `crontab -l` for the deploy user empty;
`bootstrap.sh` re-run changing nothing under the instance directory.

## Open questions

- Whether `compose.yml` and `infra/` should also ship in the image and be
  emitted on first run (`docker run --rm fq-ops init > compose.yml`),
  leaving bootstrap with nothing tracked to copy. Left on the host for
  now: compose reads the file before any image is pulled, and a stack
  definition that lives outside the images is the ordinary shape.
- Whether the socket proxy belongs in slice 2 rather than slice 4. It
  does not change the design; it changes how much of the runtime's API a
  compromised ops container reaches.
- Whether `restore.sh`, which brings the whole stack down and up, wants
  to be a sibling or stays a by-hand verb run from the host with the
  image's copy of the script. A sibling works — it is the same invariant
  as the deploy — and keeps one spelling.
