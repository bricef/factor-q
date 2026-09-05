# ADR-0035: The deployable unit is a container image, and docker compose is the supervisor

## Status

Accepted (2026-09-04). Refines
[ADR-0027](0027-graceful-drain-deploys.md): the drain model is unchanged,
and this ADR fixes the packaging and supervision that ADR-0027 left to "a
companion decision". It answers the decision
[#553](https://github.com/bricef/factor-q/issues/553) asked for — no
systemd units — and retires the "no supervisor" carve-out of
[#102](https://github.com/bricef/factor-q/issues/102). The tarball channel
of [ADR-0022](0022-binary-distribution-and-licensing.md) stays as it is;
images are a second artifact, not a replacement.

Implementation: partial — the images (clauses 1, 3, 4 and the daemon's
half of 6 and 8) are built and published: `services/fq-runtime/Dockerfile`
holds the `minimal`, `dogfood`, `watcher`, `cron` and `dashboard` targets,
assembled from the release binaries rather than compiled in-image, with
one volume at `/var/lib/factor-q` and a `HEALTHCHECK` on the daemon
([#587](https://github.com/bricef/factor-q/issues/587), slice 1); every
merge to `main` pushes them to `ghcr.io/bricef/<name>` tagged with the
twelve-hex commit the binaries report and with a moving `main-latest`
(slice 2); `ops/dogfood/compose.yml` is the stack definition and
supervisor of clause 2 with clause 7's network shape and the one volume
of clause 6 (slice 3); `ops/dogfood/deploy.sh` is clause 5's tag bump —
pull, prove every image's binary reports the tag, `docker compose stop`
as the drain, `up -d`, verify on the running containers — and the
`setsid` launchers and the release tree are gone (slice 4); a
`bootstrap.sh` provisions a dedicated host, `deploy.sh --auto` is hourly
continuous delivery with an idle check and automatic rollback, and
`hygiene.sh`, `backup.sh` and `restore.sh` bound the volume and make the
instance restorable (slice 5). Not built: probes on the adapter and
dashboard images (8); the live instance has not yet been moved onto the
stack (the runbook is in the ops README). The tracking issue carries
what remains.

## Context

The dogfood stack is four processes — `fqd`, `github-watcher`,
`fq-dashboard`, `fq-cron` — launched as `setsid … &` from a
`releases/<sha>/` tree that `ops/dogfood/deploy.sh` maintains, next to a
NATS broker and a Caddy proxy that already run under docker compose.
Nothing restarts a crashed process and nothing but the containers survives
a reboot. Every input to the instance is host-side and untracked: the
daemon config, the cron schedule, the agent definitions, the secrets file,
the edge identity under `~/.local/state/factor-q`, and the three SQLite
stores, which resolve under `[cache] directory`. The host is the
maintainer's development machine, shared with the repo's own worktrees and
their build trees.

The instance is moving to a dedicated host. That move forces the question
this ADR answers: **what supervises the four processes, and what is the
unit that gets deployed?** Two shapes were on the table.

- **systemd units** ([#553](https://github.com/bricef/factor-q/issues/553),
  the production-readiness review's Phase 1 item 9): four unit files with
  `Restart=always` and a `TimeoutStopSec` at the drain deadline, with
  `deploy.sh` calling `systemctl restart` after the symlink flip.
- **Container images under docker compose**: one image per binary, built
  by CI and tagged with the commit it was built from; compose as the
  stack definition and the supervisor; the same `restart:` policy the
  broker and proxy already use.

Two facts about the workload shape the choice. First, the daemon is
already built to live in a bare envelope: a static musl binary that takes
its configuration from one file and the environment, drains on SIGTERM,
and recovers in-flight work from its own WAL on the next start
(ADR-0027). It needs no init system, no shell and no service manager, and
the distroless image in the tree is the standing proof. Second, the
dogfood *workload* is not bare at all: fleet agents clone the repository
and run `just ci` inside their invocation workspace, so the process that
hosts them needs `cargo`, `git`, `gh`, `just`, Go, Node and the pinned
`nats-server` on the `exec` tool's fixed `PATH`. Today that toolchain is
whatever the host happens to have. Nothing in the repository declares it.

A third fact bounds what this ADR is *not*. Where agent subprocesses run,
and with what isolation, is [ADR-0010](0010-agent-execution-isolation.md)
and [ADR-0028](0028-tool-scoped-isolation-and-workspace.md), tracked by
[#209](https://github.com/bricef/factor-q/issues/209), and the review says
not to start it before its Phase 2. This ADR is about packaging and
supervision of the daemon. It leaves agents running inside the daemon's
process boundary, as the daemon's user, exactly as they do now.

## Decision

1. **The deployable unit is an OCI image, tagged with the commit SHA it
   was built from.** CI builds and publishes one image per shipped binary
   on every merge to `main`, beside the tarball it publishes today. The
   image tag is the same twelve-hex commit the binaries already stamp into
   `--version`, so the coherence check `deploy.sh` performs across the
   bundle becomes a property of the tag.

2. **Docker compose is the stack definition and the supervisor.** One
   compose file in `ops/dogfood/` names every service — the broker, the
   proxy, the daemon, the watcher, the dashboard and the scheduler — with
   its `restart:` policy, its `healthcheck`, its `depends_on` ordering
   (`condition: service_healthy` on the broker), its `stop_grace_period`,
   its resource limits and its log rotation. **No systemd units are
   authored.** The host's init starts the container runtime and nothing
   else of ours; that is the distribution's systemd, not a dependency of
   this project.

3. **The daemon assumes a barebones envelope, and the minimal image is
   the gate that proves it.** `fqd` runs from a distroless image holding
   `fqd` and `fq` and nothing else, with configuration from one file and
   the environment, and every piece of state under one mounted volume at
   a declared path. That image stays in CI as it is today, and anything the
   daemon turns out to need from a host beyond that is a defect in the
   daemon, not a reason to fatten the image.

4. **A second, fat image layers the agent toolchain on the minimal one.**
   `fq-dogfood` is `FROM` the minimal image and adds the toolchain the
   fleet's agents need, installed where the `exec` tool's baseline `PATH`
   finds it. This is the transitional shape: the worker and the control
   plane share one process today, so the process that must carry the
   toolchain is the process that also runs the control plane. When they
   split, the control plane runs the minimal image and only the worker
   image carries the toolchain. That split is future work and not a
   blocker for the move.

5. **Deploy is a tag bump; rollback is the previous tag.** `deploy.sh`
   drains through `fq down` as ADR-0027 requires, then points compose at
   the new tag and brings the stack up. The `releases/<sha>/` tree, the
   `current` symlink, the `/proc/<pid>/exe` verification and the four
   `setsid` launchers retire; image identity replaces all of them. A
   `docker stop` is also a drain — SIGTERM is the graceful path — so every
   service's `stop_grace_period` must exceed the daemon's
   `drain_deadline_ms` with headroom, or a deploy is a kill.

6. **One volume per image, and the daemon's volume is the instance.**
   Everything the daemon reads or writes that outlives the container
   lives under a single mount, in a fixed layout that mirrors today's
   `~/fq-dogfood` tree without `releases/` and `current`: the config
   (`fqd.toml`, `fq-cron.toml`, `agents/`), the edge identity
   (`[state] directory`), the three stores (given an explicit
   `[cache] directory` under the mount rather than the XDG cache default
   the config template says a cleaner may empty), the invocation
   workspace, and the agents' build state — `target/` and the sccache
   directory — so an image update does not cold-build every invocation
   that follows it. The container has no other writable path. Backing up,
   migrating or inspecting the instance is one volume, and the layout
   inside it is the contract: a backup may exclude the workspace and
   build subtrees by path, and a restore is the tree, nothing else. The
   secrets file stays on the host as compose's `env_file`, because
   compose reads it before any volume is mounted, and it is the one
   thing a volume copy must not carry. The broker and the proxy keep the
   volumes their own images define; the complete state of the instance
   is therefore the daemon's volume plus the JetStream store.

7. **The container runtime's socket is never mounted into any of our
   containers.** Agents run inside the daemon's container; a socket there
   is a root shell on the host. The broker joins the compose network,
   token-authenticated
   ([#542](https://github.com/bricef/factor-q/issues/542)); agents share
   the daemon's network namespace, so the token is still what stands
   between an agent and the trigger subject.

8. **Every image carries a `HEALTHCHECK`.** The daemon's probe is a
   loopback liveness signal that needs no pairing and no token, so a
   supervisor can ask "is it alive" without holding a credential. The
   post-deploy health gate and rollback of
   [#339](https://github.com/bricef/factor-q/issues/339) consume the same
   probe.

## Rationale

**Why not systemd.** Four unit files touch no daemon code, so the cost is
not bloat in the daemon. The cost is that they are throwaway operational
surface for a shape the project does not intend to keep: the units, the
`systemctl` branch in `deploy.sh`, the README's install step and the
provisioning script that puts the toolchain on the host would all be
written, tested by hand on one machine, and discarded when the daemon is
containerised anyway. They also entrench the host as a hand-configured
machine whose environment nothing in the repository describes, which is
the condition the move is meant to end.

**Why the cost comparison favours containers.** The toolchain provisioning
work exists on both paths. On the systemd path it is a script that runs
once on one host; on the container path it is a `Dockerfile` that CI
builds on every merge. The container path adds a `deploy.sh` rewrite, the
volume layout and the grace-period setting, and it removes the release
tree, the symlink flip and the process-identity verification. Comparable
effort; a reproducible artifact on one side and a pet on the other.

**Why the fat image now rather than a thin daemon with a runner.** A
scratch daemon that dispatches subprocesses into a separate container is
ADR-0010: changes to the `exec` tool, workspace mounts across a container
boundary, kill semantics across it, and a sandbox mapping. Doing that as
part of a host move turns an operations project into the isolation
project the review says to hold. The fat image is the envelope the daemon
already lives in — a host with a toolchain — made reproducible. The
minimal image kept in CI is what stops the fat one from becoming the
definition of the daemon's needs.

**Why compose and not an orchestrator.** One host, one instance, six
services, a single operator. Compose already runs two of the six. Anything
more is machinery for problems this deployment does not have; the ADR's
clauses carry over to an orchestrator unchanged if a second host ever
appears.

**What this does not decide.** Packaging is not isolation. Agents still
run as the daemon's user inside the daemon's container, with the daemon's
network and the daemon's view of the workspace. The sandbox is exactly as
strong as it was on the host. The review's Phase 1 timeouts and Phase 2
durability work are untouched by, and independent of, this decision.

## Consequences

**Positive.**

- Restart on crash and start on reboot come from `restart:` policy, with
  no code and no units.
- The stack has one tracked definition, and the host holds one volume
  per image
  and the secrets file.
- Image tag equals commit, so deploy and rollback are the same one-line
  operation, and the bundle coherence check becomes structural.
- Per-service CPU and memory limits answer the job-cap half of
  [#292](https://github.com/bricef/factor-q/issues/292) with two lines
  of compose.
- Log rotation and capture come from the log driver.
- `depends_on` with a health condition removes the race the adapters lose
  today when they start before the broker
  ([#551](https://github.com/bricef/factor-q/issues/551)).

**Costs and risks.**

- The fat image is several gigabytes and must be rebuilt when a toolchain
  pin moves. The minimal image does not share this cost.
- The build subtree of the daemon's volume is load-bearing: without it
  every invocation after an image update cold-builds for the better
  part of half an hour and writes twenty gigabytes. It is also the
  subtree that fills the volume: build churn shares a filesystem with
  the edge identity and the stores, so the disk-usage alert
  [#367](https://github.com/bricef/factor-q/issues/367) asks for and a
  pruning job in `fq-cron` are part of this shape, not extras. Every
  directory setting the daemon has (`[state]`, `[cache]`, `[workspace]`,
  `[agents]`) and the build-cache variables the image exports for
  agents to allowlist (`CARGO_TARGET_DIR`, `SCCACHE_DIR`) must point
  under the mount; one that points elsewhere is state the volume does
  not capture.
- The E3 defects in the existing `Dockerfile` — no `FQ_STATE_DIR` under a
  volume, no `HEALTHCHECK`, a broker URL with no token — go from review
  findings to blockers.
- A `stop_grace_period` shorter than the drain deadline turns every deploy
  into a hard kill, silently. The compose file has to carry the daemon's
  deadline, and a change to one without the other is a defect.
- The distroless image runs as a fixed non-root UID; the volume's
  ownership has to match it (one `chown`, since there is one volume),
  and the fat image's toolchain has to be installed for that user.
- The `exec` tool's baseline `PATH` is `/usr/local/bin:/usr/bin:/bin`;
  the toolchain must be installed there, not under a per-user rustup or
  nvm directory, or agents will not find it.
- Agent definitions on the live instance that rely on host tools not in
  the image stop working. The image's tool list is the compatibility
  contract, and it has to be read against every live definition before
  the move.
- The operator's `fq` on the host pairs to a published port rather than a
  loopback socket; nothing about pairing changes, but the address does.
- The `main-latest` tarball keeps being built for `install.sh` and for
  any host that is not containerised, so CI publishes two artifact
  families from one build.

**Interlocks.** Builds on ADR-0027 (drain on SIGTERM is what makes
`docker stop` safe) and on the daemon's existing environment-only
secrets model. Composes with
[#339](https://github.com/bricef/factor-q/issues/339) (the health gate
needs the probe in clause 8) and
[#342](https://github.com/bricef/factor-q/issues/342) (a metrics
endpoint is another loopback port on the same container). Prepares the
ground for ADR-0010 and ADR-0028 by making the daemon's container the
thing a runner container is later split from, and for the worker /
control-plane split, after which clause 4 collapses to one image per
role.

## Alternatives considered

- **systemd units** ([#553](https://github.com/bricef/factor-q/issues/553)).
  Cheapest single step to a supervised stack, and the review's
  recommendation. Rejected for the reasons above: throwaway surface, a
  pet host, and provisioning work that is not tested by anything.
- **Keep the `setsid` launchers and move as-is.** Moves the problem to a
  new machine without solving it; a reboot still loses the four
  processes.
- **One fat image only, no minimal image.** Simpler, but nothing then
  proves the daemon runs in a bare envelope, and the fat image quietly
  becomes the specification of what the daemon needs.
- **Thin daemon plus a runner container now.** The right end state and
  the wrong time; see the rationale. It is the next step after this
  one, not a substitute for it.
- **An orchestrator** (Kubernetes, Nomad). Nothing in a single-host,
  single-tenant deployment needs one; compose's clauses port to one
  unchanged if that changes.

## Open questions (deferred by decision)

- **Registry.** Where CI publishes images; the natural choice is the
  repository's own container registry, with the tarball channel's
  retention rule (the channel holds only the newest build, the host
  holds its history) applied to tags.
- **Runtime.** Docker or podman, root or rootless. Rootless narrows the
  blast radius of clause 7 further; whether the build subtree's
  performance survives it is untested.
- **Adapter images.** Whether the watcher, the scheduler and the
  dashboard get one image each from the same distroless base or share a
  single slim image with three entrypoints.
- **The probe.** Whether clause 8's liveness signal is a loopback HTTP
  endpoint on the daemon or an `exec`-form `HEALTHCHECK` that runs `fq`
  against the edge with a token from the state volume. The former needs
  daemon code; the latter needs no code and a credential in the probe.
- **Migrating the live instance.** Whether the edge identity is copied
  or rotated on the move. Rotating costs one re-pair of the operator's
  client and one re-mint of the dashboard's token, and is the
  recommended path; copying preserves every issued token. Either way
  the move is one copy of the `~/fq-dogfood` tree into the volume,
  minus `releases/`, `current` and `.secrets/`.
- **One volume for the whole stack.** Compose can mount subpaths of
  one named volume into several containers, which would fold the
  JetStream store and Caddy's data into the daemon's volume and make
  the instance exactly one volume. The images run as different users,
  so it costs an ownership scheme inside the volume; not decided.
