# Production readiness, Phase 0 — execution plan

> **Opened 2026-09-04.** Executes Phase 0 of the
> [production-readiness review](../../reviews/2026-09-03-production-readiness-review.md)
> (PR #530) and queues Phase 1. Written as a hand-off: a session with no
> prior context should be able to read this file, then the review's
> "The plan" section, and start work. Line numbers below are as of
> `main@223c357`; treat them as pointers, not facts.

## How to use this plan

1. Read the review's **Verdict** and **The plan** sections. Everything
   here is sequencing and grounding on top of them; the reasoning and the
   evidence live there. Findings are cited by letter (A1, B3, E1) and
   refer to the review's "New findings" section.
2. Read [Standing practices](#standing-practices-for-this-work) below
   before delegating anything. Three agents wedged silently on this repo
   in August; the rules are the result.
3. Do the steps in order. Step 1 costs an hour and unblocks the fleet;
   Step 2 is three parallel work packages; Step 3 touches the live host
   and wants the maintainer present.

## Where things stand

- **Phase 0 item 1 is done.** #521, #524, #527, #529 merged, plus #531
  (capability-grant strictness) and #532 (whole-folder doc lint), which
  were not in the review but closed adjacent gaps.
- **#510 (#437, reasoning as message parts) also merged on 2026-09-04**,
  fifteen commits including `Message` becoming an enum over turn kinds —
  a breaking change to the event wire with `SCHEMA_VERSION` 2 → 3
  (ADR-0034, the [reasoning plan](../closed/2026-08-25-reasoning-as-message-parts.md)).
  It is not on the review's "what not to start" list; STATUS.md records
  why it went first. Phase 0 builds on top of it.
- **`main` is green on every Rust job**; the nightly live suites have
  ten or more consecutive green runs; CI now lints every markdown file
  under `docs/` (#532). The doc-lint job went red at `223c357` on three
  lines in the reasoning plan that start with an issue number; the PR
  that adds this file fixes them.
- **The dogfood instance** runs `9477254` (2026-08-25), ten days up,
  nine agents. Before #510 it was behind `main` only by parsing
  tightenings (#515, #522, #527, #531), each checked against every live
  definition. After #510 the gap includes the `Message` wire change, so
  the next deploy carries a schema bump: #409 says the read path
  ignores `SCHEMA_VERSION`, so a projection rebuild over mixed v2/v3
  events is silently lossy. Read #409 and the reasoning plan's phase 2
  row before running `deploy.sh`, and expect the projection to need
  attention.
- **Nothing from the review's plan is filed as an issue.** The letters
  exist only in the review document. Existing trackers it names: #406
  (audit/deny/Dependabot), #405 (`install.sh` fails open), #278 (429
  handling), #509 (second-SIGTERM escape), #37 (stuck-invocation
  detection), #327 (exactly-once), #399 and #400 (sandbox escapes, Phase 2).
- **Host disk** was at 97% mid-session on 2026-09-04 and is now at 75%
  with 79G free. Per-worktree cargo target dirs are the driver; check
  `df -h /` before launching parallel agents, and reclaim merged
  worktrees' `target/` first.

## Step 1 — File the plan as issues

The review's fleet marks only mean something once there is an issue for
the `m0-issue-fix` loop to claim. File these first. Each body should
quote the finding letter, link the review section, and carry the phase's
exit criterion as its acceptance test.

### Phase 0 issues

| Proposed title | Finding | Exists? | Fleet? |
|---|---|---|---|
| ci: cargo audit + cargo deny as a red `just` gate; Dependabot for cargo, gomod, actions | E1, #406 | **#406** — re-ground, do not duplicate | yes |
| deps: `cargo update`, bump `async-nats` past 0.38, pin `nats` image tags | E1 | new | yes |
| security: the broker credential is written into the event log, the banner and the daemon log; add `[nats] token_env` | A1 | new | no |
| security(mcp): stdio MCP servers inherit the daemon's whole environment, provider keys included | A2, A7 | new | no |
| ops(dogfood): token auth on the broker | A3 | new | live host |
| ops(dogfood): `admin off` in the Caddyfile | A4 | new | live host |
| security(cli): non-interactive `fq connect` must require `--fingerprint` | A5 | new | yes |
| security: write the admin token to a 0600 file, not stdout; `hide_env_values` on the dashboard token; export only what the dashboard needs | A6 | new | yes |
| bug: `install.sh` fails open when the `.sha256` fetch fails | #405 | **#405** — re-ground | yes |

Also `git rm .local` (a tracked file at the repo root); a checkbox on
the tracking issue, not an issue of its own.

### Phase 1 issues

File these now too, so the queue exists; they are not started until
Phase 0's exit criteria hold.

| Proposed title | Finding | Exists? | Fleet? |
|---|---|---|---|
| runtime: LLM HTTP call timeout, configured, mapped to a transient error; handle 429 `Retry-After` in the same code | B1, #278 | new (#278 is the 429 half) | no |
| runtime: tool and MCP call timeout in `run_tool`; wire `call_tool_cancellable`; `check_read` requires a regular file | B2 | new | no |
| mcp: start-up and discovery timeouts, page and tool caps, concurrent start, failed server marked unavailable | B3 | new | no |
| bus: `Nak(Some(delay))` with escalation and explicit `ack_wait` on every durable; health probes over all six consumers; summary consumer supervised | B4, F | new | no |
| daemon: bind the edge first and use the listener as the instance lock; deregister on every post-registration error path; accept-loop sleep and connection limits; second-SIGTERM escape and drain ordering | B5, B6, B9, #509 | new (#509 is one clause) | no |
| adapters: `MaxReconnects(-1)`, `RetryOnFailedConnect`, handlers; cron valve timer; watcher add-then-remove claim | B7, B8, C7 | new | yes |
| sandbox: process-group kill on `exec` timeout | A9 | new | yes |
| feat: stuck-invocation detection with "last step boundary" on the heartbeat | #37, F | **#37** — re-ground | no |
| ops: systemd units for `fqd`, watcher, dashboard and cron with `Restart=always`; retire the `setsid` launchers | B7 | new, **needs-decision** | no |

### Tracking issue

One issue, "Production readiness: Phase 0", with a checkbox per row above
and the exit criteria verbatim:

> `just ci` includes a red-on-advisory audit gate and is green; `fq
> events get` of a `system_startup` event contains no credential; the
> dogfood broker rejects an unauthenticated `PUB`; `curl
> localhost:2019/config/` on the dogfood host is refused.

Close it only when all four hold on the dogfood host, not when the PRs
merge.

## Step 2 — Three parallel work packages

These touch disjoint files, need no access to the live host, and are
each one PR. Three concurrent builds need roughly 30G; four is tight.
Each gets a delegated agent under the standing practices, with a
watchdog.

### WP-A — Dependency gate (E1, #406)

**Scope.** A `just audit` recipe running `cargo audit` and `cargo deny
check` that **fails** on an advisory, wired into `just ci` and the CI
workflow; Dependabot config for cargo, gomod and actions; `cargo update`;
`async-nats` bumped past 0.38; every `nats:latest` pinned to a digest or
version.

**Anchors.**

- `Cargo.toml:63` — `async-nats = "0.38"` (workspace pin).
- `nats:latest` in `infrastructure/docker-compose.yml:3`,
  `ops/dogfood/infra/docker-compose.yml:3`, and
  `services/fq-runtime/crates/fq-cli/src/templates/docker-compose.yml:16`
  (the template `fq init` writes — pinning here changes what new
  deployments get).
- No `cargo audit`, `cargo deny` or Dependabot reference exists anywhere
  in `justfile` or `.github/` today.

**Notes.** The review counts ten advisories and seven warnings against
the lockfile. Some will clear with `cargo update`; the rest need a
`deny.toml` with an explicit, commented ignore per advisory that cannot
be fixed yet, never a blanket allow. The `async-nats` bump may surface
API changes in `bus.rs`; the review says it also makes #327's PR-5
easier, so note anything learned there on #327. Run the full `just ci`,
not just `quality` and `runtime-ci`: the audit gate is new and must be
seen running.

### WP-B — Secret hygiene (A1, A2, A7)

**Scope.**

1. The broker credential never reaches the event log, the banner or the
   daemon log. Add `[nats] token_env` so the token comes from the
   environment and `[nats] url` carries no credential.
2. Stdio MCP servers start with `env_clear()`, a pinned `PATH`, only the
   `env:` the definition declares, and a neutral working directory.
3. `docs/guide/mcp.md` and the authoring guide say what MCP sandboxing
   actually does, in the same PR.

**Anchors.**

- `services/fq-runtime/crates/fq-daemon/src/hosted.rs:112` —
  `nats_url: config.nats.url.clone()` into `SystemStartupPayload`. This
  is the line that writes the credential into the event log.
- `services/fq-runtime/crates/fq-ops/src/events/payloads.rs:406` — the
  payload field. It is a wire type; changing its meaning is fine
  (pre-alpha), but say so in the PR.
- `services/fq-runtime/crates/fq-daemon/src/daemon.rs:61` — the banner
  prints the URL; `:99` puts it in an error context.
- `services/fq-runtime/crates/fq-daemon/src/cli.rs:83` — `FQ_NATS_URL`
  override, the natural neighbour for `FQ_NATS_TOKEN`.
- `services/fq-runtime/crates/fq-cli/src/events.rs:190` — renders the
  startup payload; whatever replaces the URL must still read well here.
- `services/fq-runtime/crates/fq-runtime/src/mcp.rs:1038` — the
  `Command::new(&config.command)` for stdio servers; `:1041` adds the
  declared `env:` on top of the inherited environment. There is no
  `env_clear()`.

**Notes.** This is the package most likely to break something subtle.
The redaction must be structural (the credential is never in the string)
rather than a regex over log lines. `env_clear()` will break any MCP
server that relied on inherited `HOME` or `PATH`; the smoke suite and
the `mcp_integration` tests exercise reference servers under `npx`, so
run `just smoke` locally too, with the key from `.env` and the NATS URL
only via `just`. The dogfood definitions in `~/fq-dogfood/agents/` are
the compatibility check: read them, do not modify them, and list any
`env:` they would now need to declare.

### WP-C — Install and connect hardening (#405, A5, A6)

**Scope.**

1. `install.sh` fails closed when the checksum fetch fails (#405).
2. `fq connect` without a TTY requires `--fingerprint`; trust-on-first-use
   stays interactive-only.
3. The daemon writes the admin token to a 0600 file under the state
   directory instead of stdout, and `fq init`'s guidance says so.
4. `hide_env_values` on the dashboard's token; the dogfood scripts export
   only what the dashboard needs.
5. `git rm .local`.

**Anchors.**

- `install.sh` at the repo root.
- `services/fq-runtime/crates/fq-cli/src/cli.rs:222-236` — `connect`,
  its `--fingerprint` flag and the TOFU description;
  `services/fq-runtime/crates/fq-cli/src/connections.rs:161` — the "no
  token for {addr}" error that tells the operator the daemon printed the
  admin token.
- `services/fq-runtime/crates/fq-daemon/src/edge_identity.rs:55` —
  `mint_admin_token()`; `services/fq-runtime/crates/fq-cli/src/project.rs:102`
  — `fq init` text promising the daemon prints it.
- `ops/dogfood/dashboard.sh`, `ops/dogfood/env.example:56-65` — the
  attenuated-token story the dashboard already follows.

**Notes.** Changing where the admin token appears changes the operator
guide and `ops/dogfood/README.md`; update both in the same PR. The
`error_commands_gate` test will fail if any new message names an `fq`
verb that does not parse — that is the intended check, not a flake.

## Step 3 — Live-host changes (A3, A4)

Do these with the maintainer present. Both restart something that is
serving the fleet.

1. **Broker token auth (A3).** `ops/dogfood/infra/docker-compose.yml`
   mounts `nats.conf` and carries a comment that the broker is
   loopback-only and unauthenticated. Add token auth to `nats.conf`,
   supply the token to the daemon through WP-B's `token_env`, then:
   `fq drain` per the deploy SOP in `ops/dogfood/deploy.sh` (never kill a
   busy daemon), restart the broker, restart `fqd` with the token, verify
   `fq status` and that an unauthenticated `nats pub` is refused.
2. **Caddy admin API (A4).** `ops/dogfood/infra/Caddyfile` has no
   `admin` directive, so the API listens on its default `localhost:2019`.
   Add `admin off` in the global block, reload Caddy, verify
   `curl localhost:2019/config/` is refused.
3. **Deploy `main`** at the same time, since the daemon restarts anyway.
   Note the known deploy.sh defect in #512 (a successful drain is
   reported as a failure); confirm the worker is `shutdown`, not `stale`,
   before believing the script.

WP-B must land before item 1, since the daemon needs `token_env` to
connect to an authenticated broker without putting the token in its URL.

## Decisions the maintainer owns

- **systemd units (Phase 1, item 9)** reverse a documented decision to
  use `setsid` launchers. The review calls it the cheapest step toward
  the 30-day bar. Decide before Phase 1 starts; the issue is filed
  `needs-decision`.
- **`async-nats` bump scope.** If the bump cascades into `bus.rs` beyond
  a mechanical update, WP-A should stop at the audit gate and file the
  bump separately rather than grow.
- **What replaces `nats_url` in the startup event.** Host and port only,
  or nothing. The review's exit criterion only requires that no
  credential appears.

## After Phase 0

Phase 1 is the timeouts and the supervision: nothing may wedge the
daemon. It is additive, mostly independent of Phase 0, and its fleet
items can start as soon as their issues exist. Phase 2 (the record is
trustworthy) touches `runner.rs` and the WAL schema, is done by hand one
PR at a time, and is the gate before any storage, memory, multi-node or
container-isolation work resumes. The review's "What not to start before
Phase 2 is done" list is the standing answer to "can we also…".

## Standing practices for this work

These are repo conventions and the lessons of three wedged agents, in
one place so a hand-off prompt can point at them.

- **Worktrees, never the primary copy.** All work in
  `.claude/worktrees/<name>`, created with `git -C <repo> worktree add`;
  never `git checkout`, `switch`, `rebase` or `stash` in the user's
  working copy. Use `git -C <path>`, never `cd <path> && git`.
- **Delegated agents: no sub-delegation, no background tasks, no
  monitors of their own, `timeout` on every command that can hang,
  commit before every gate, small batches.** The dispatcher, not the
  agent, runs a watchdog that polls the transcript's mtime and the
  branch's commits; 25 minutes of silence is a wedge.
- **Gates are the `just` recipes**: `just quality`, `just runtime-ci`
  (plus `store-ci` or `dashboard-ci` when those crates change), and
  `just lint-docs` whenever any markdown changes — the Rust gates do not
  run markdownlint. Bare `cargo test` at workspace level produces
  phantom NATS failures; go through `just`. A Node `EPIPE` trace from an
  MCP stdio child is a known flake; trust the exit code.
- **Size ratchets go one way.** Never raise a budget in
  `.file-size-baseline`; pay by extracting a module. Never run
  `just sizes-bless`; it destroys the hand-written rationale.
- **Docs travel in the same PR as the code**, including inline docs,
  guides and the container build. Doc comments on schema'd types are
  published through `describe` and the MCP face; nothing internal goes
  in a `///`.
- **Pre-alpha.** Backward compatibility, rollback and migration are not
  concerns. Wire-strictness changes are fine; say so in the PR.
- **Disk.** `df -h /` before launching agents. A full build is 8–20G per
  worktree. Reclaim merged worktrees' `target/` directories first; that
  is reversible and needs no permission.
- **Dogfood host.** Read `~/fq-dogfood/agents/` for compatibility
  checks; never modify it from a work package. Deploys follow
  `ops/dogfood/deploy.sh` and the SOP in the ops README.

## Exit criteria (from the review, verbatim)

`just ci` includes a red-on-advisory audit gate and is green; `fq events
get` of a `system_startup` event contains no credential; the dogfood
broker rejects an unauthenticated `PUB`; `curl localhost:2019/config/` on
the dogfood host is refused.
