# Production readiness, Phase 0 — execution plan

**Status:** closed 2026-09-10. Phase 0 ("stop the bleeding") completed on
2026-09-04 with all four exit criteria verified on the dogfood host;
Phase 1 ("nothing can wedge the daemon"), whose queue this plan filed,
reached code-complete on `main` on 2026-09-06 and its follow-ups landed
by 2026-09-09 — see [Outcome](#outcome) below. What this plan did not do
is put Phase 1 on the live instance: that is the move onto the compose
stack, tracked by the
[dogfood host migration plan](../active/2026-09-05-dogfood-host-migration.md)
(#587). Phase 2 (the record is trustworthy) is next, by hand, one PR at a
time, per the review. Originally: active (2026-09-04). Tracking issue:
[#554](https://github.com/bricef/factor-q/issues/554).

> **Opened 2026-09-04.** Executes Phase 0 of the
> [production-readiness review](../../reviews/2026-09-03-production-readiness-review.md)
> (PR #530) and queues Phase 1. Written as a hand-off: a session with no
> prior context should be able to read this file, then the review's
> "The plan" section, and start work. Line numbers below are as of
> `main@223c357`; treat them as pointers, not facts.

## Outcome

Recorded at close from the evidence on #554; the sections that follow
are the plan as written on 2026-09-04 and are left as they were.

### Phase 0 — complete 2026-09-04 13:32Z

- **Step 1** filed #539–#553 and re-grounded #406, #405, #278, #509 and
  #37 by comment; #554 tracked the phase.
- **Step 2** landed as #555 (WP-C), #556 (WP-B) and #557 (WP-A), with
  review follow-ups #559 and #560; `main` at `77c3389` on 2026-09-04.
  The `async-nats` 0.38 → 0.50 bump traced the CONNECT frame, token
  included, at `trace` level; `fqd` now caps `async_nats` below `trace`
  regardless of `RUST_LOG`, and #556's `nats_token_hygiene` test is the
  guard.
- **Step 3** was one 26-second window (13:31:52Z → 13:32:18Z): `fq down`
  drained clean, the adapters stopped, `nats` recreated 2.14.0 → 2.14.3
  with token auth, `caddy` recreated with `admin off`, `deploy.sh`
  installed `5afeacaf42b8`. The adapters read the broker URL from the
  environment rather than argv from then on (ops PR #575). Host config
  is committed to the ops repo; the runbook sections survived into the
  compose-era `ops/dogfood/README.md`.

| Exit criterion (review, verbatim) | Evidence |
|---|---|
| `just ci` includes a red-on-advisory audit gate and is green | `main@5afeaca`, CI run 33876523067: Dependency audit inside the required Rust CI job (#557); seen red on the pre-update lockfile and again with an ignore commented out |
| `fq events get` of a `system_startup` event contains no credential | event `01a06c9e-a06e-79a2-a49f-a8fdab705078`: payload `nats_url: "nats://localhost:4223"`, token absent from the payload and from all four logs |
| the dogfood broker rejects an unauthenticated `PUB` | raw-socket `PUB` to 127.0.0.1:4223 → `-ERR 'Authorization Violation'`; `connz` shows three clients, all authenticated |
| `curl localhost:2019/config/` on the dogfood host is refused | connection refused (HTTP 200 before the window); the dashboard still serves behind basic-auth |

### Phase 1 — code complete 2026-09-06, `main@3b9f319`

Decisions made 2026-09-04 and recorded on each issue:

- docker compose, not systemd
  ([ADR-0035](../../adrs/accepted/0035-container-image-and-compose-supervision.md);
  issue #553 closed against it);
- a re-signalable stop mode (#509);
- a 10 s connect and 600 s response deadline on LLM calls, a timeout
  being transient with its own two-attempt budget, `Retry-After`
  honoured up to about two minutes and deferral beyond that left to
  #278 (#546, #607);
- the `mcp.rs` split (#191) as the first PR, so the rest had room.

| Item | PR(s) | Closes |
|---|---|---|
| LLM call deadlines, 429 + `Retry-After` cap | #606, #608 | #546, #607 |
| tool/MCP call deadlines, cancellable MCP calls, regular-file `check_read`, progress correlation | #610, #614 | #547, #605 |
| MCP boot never blocks: concurrent start, deadlines, caps, unavailable state + retry, refusal at dispatch, post-boot death detection | #621 | #548 |
| escalating NAK + explicit `ack_wait` on every durable, health over every consumer, summariser supervised | #611 | #549 |
| edge listener as instance lock, error-path deregistration, accept-loop limits, re-signalable stop, drain ordering | #597 | #550, #509 |
| adapters reconnect forever, cron valve re-arms, watcher add-then-remove claim | #609, #613 | #551 |
| process-group kill on exec timeout/drop; teardown graces configurable | #615 | #552, #618 |
| stuck-invocation detection, threshold derived from the timeouts, last step boundary on the heartbeat | #619 | #37 |
| supervision | ADR-0035 / #586 and the compose stack | #553 |

The review's exit criterion asked for a fault-injection test per wedge
class, `fq doctor` over every consumer, and five supervised units:

| Wedge class | Test on `main` |
|---|---|
| hung provider | `a_hung_provider_ends_the_invocation_within_the_timeout` (real TCP mock that stalls; default attempt policy) |
| hung MCP server at boot | `fqd_boots_with_a_hung_mcp_server_and_reports_it_unavailable` (`fqd_smoke.rs`; real binary, `command: sleep`) |
| hung tool | `a_hung_tool_times_out_and_the_invocation_continues`, `consecutive_timeouts_end_the_invocation_naming_the_count` |
| FIFO read | `fifo_is_refused_immediately_and_never_blocks` (FIFO, symlink-to-FIFO, directory, device) |
| consumer `SQLITE_FULL` | `a_permanently_failing_handler_backs_off_and_reports_stuck` + `consumer_log_rate.rs` |
| `fq doctor` reports every consumer | `edge_reports::control_doctor_*` (live daemon, five durables named) |
| five units | superseded by ADR-0035: the compose services are the supervised set |

Also from this phase: an MCP server that dies after boot is detected
and redialled, and `invocation.stuck` fires once per stall at a
threshold derived from the configured deadlines (4,210 s on the dogfood
config). #327's root cause is recorded: the trigger durable's real
first-delivery window is `backoff[0]` = 1 s.

**Follow-ups after the close-out, all merged by 2026-09-09:**

- #622 (#612): the cron valve counted jobs rather than fires.
- #624 (#617): a long server-request servicing consumed the tool's
  backstop grace.
- #628 (#115): the reference server's startup is retried once in the
  test harness.
- #632 (#623): a truncated config read deleted every cron job's state.
- #638 (#630): integration tests stranded `fqd` children when the test
  binary was killed.
- #639 (#634): the config watcher seeded itself from a second read.
- #641 (#635): a partial write that parsed still deleted the missing
  jobs' ledgers.

**Open by decision, on their own issues:** #278 (429 deferral and a
per-provider limiter), #593 (`nats_url` stays in `system_startup`
through alpha; Beta gate milestone), #594 (dependency audit reported as
a PR comment before thresholds), #640 (`just lint-docs` covers only the
ADR tree), #643 (`gate-adapters` returns only the last adapter's
status), #664 (startup and `--check` accept a job-less cron config that
a reload refuses).

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
  (ADR-0034, the [reasoning plan](2026-08-25-reasoning-as-message-parts.md)).
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
- `services/fq-runtime/crates/fq-runtime/src/mcp/stdio.rs:62` —
  `stdio_command` builds the child for stdio servers. Since #541 it does
  `env_clear()`, sets a pinned `PATH`, then adds the declared `env:` on
  top; the child inherits nothing else.

**Notes.** This is the package most likely to break something subtle.
The redaction must be structural (the credential is never in the string)
rather than a regex over log lines. The MCP child environment is already
cleared (#541), so any server that relied on an inherited `HOME` or
`PATH` is already broken rather than about to be; the smoke suite and
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

- *(Decided 2026-09-04: no systemd — [ADR-0035](../../adrs/accepted/0035-container-image-and-compose-supervision.md),
  container images under docker compose, built out under #587; #553 is
  closed against it.)*
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
  MCP stdio child is a known flake; trust the exit code. The reference
  server's *startup* flake is retried once inside the test harness
  (<https://github.com/bricef/factor-q/issues/115>); anything after
  startup still fails on the first try. The retry prints the first
  failure's reason, but libtest captures it while the test passes, so it
  surfaces only when the retry fails to save the run — or on demand, via
  `cargo test -p fq-runtime --test mcp_integration -- --nocapture`, which
  is how to find out how often it is firing.
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

**Added during Phase 1 (2026-09-05 → 2026-09-10),** from what the work
actually needed; the next plan should start from this list.

- **Draft PRs until reviewed.** An agent opens its PR as a draft. A
  second, read-only agent reviews it: findings by severity, each claim
  tested rather than read, temporary edits reverted, tree verified clean.
  The dispatcher marks the PR ready only after the review fixes land and
  CI is green on the *full* check set. Three PRs were merged between
  first push and review fixes before this rule (#556, #609, #610 →
  follow-ups #559, #613, #614).
- **Prove a guard by breaking it.** A fix that adds a guard says in its
  PR that the guard was disabled and the new test failed, with the
  failure text; the reviewer repeats it. Twice this round the reviewer's
  own probe found a defect the author's tests could not (#638's
  forgotten exit status, #641's stale-config deadline).
- **Parallel agents in one package get disjoint regions**, named by file
  and function in each brief, with the shared file's owner named; the
  dispatcher rebases whichever lands second (#634 and #635 in
  `adapters/fq-cron`).
- **Kill test processes by executable path, never by name.** The devbox
  runs the dogfood `fqd`; `readlink /proc/<pid>/exe` under the
  worktree's `target/` is the only safe selector. A test binary killed by
  a signal used to strand its daemons for ever — 42 of them, 1.3 GB,
  found a day later (#630, closed by the `TestChild` fixture in #638).
  `git worktree remove` does not kill processes rooted in the worktree.
- **Which recipe is the gate.** `just runtime-ci` does not run
  `test-support-ci`; use `just rust-ci` when `fq-test-support` changes.
  `just go-ci` returns only the last adapter's status (#643): run
  `go vet ./... && go test ./...` in the changed adapter as well until
  that is fixed. Neither Go gate runs `-race`; run it by hand on
  concurrency changes — two real races were found that way this round.
- **A CI poll must see the full check set** before "no pending" means
  green: a conflicting PR registers a handful of checks and suppresses
  the rest, and one PR was flipped to ready on six of eighteen.
- **Watchdog details.** `stat -L` on the transcript path — it is a
  symlink, and the link's own mtime never changes. A session forked or
  compacted from the dispatcher does not own the dispatcher's agents:
  it must not salvage, commit, gate or push in their worktrees; it
  messages the parent.
- **When a decision changes an issue's fix shape, edit the issue body**,
  not just a comment: #606 followed #546's stale body and #607/#608 had
  to correct it.

## Exit criteria (from the review, verbatim)

`just ci` includes a red-on-advisory audit gate and is green; `fq events
get` of a `system_startup` event contains no credential; the dogfood
broker rejects an unauthenticated `PUB`; `curl localhost:2019/config/` on
the dogfood host is refused.
