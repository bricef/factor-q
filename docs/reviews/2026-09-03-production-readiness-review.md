# factor-q — production-readiness review and plan

*Reviewed 2026-09-03 against `main` @ `43726bb` (2026-08-28). Method: every
prior review in this folder and all 120 open issues were read first, and
nothing they already record is repeated below except as a one-line status
where the status has changed. Six parallel cold reads covered disjoint slices
(edge/auth/lifecycle, bus/control plane, worker/LLM/config, tools/sandbox/MCP,
store/dashboard, adapters/ops/CI); every finding that drives a plan step was
then re-verified by hand at the cited line. Executed evidence: the workspace
builds, the test suites and Go gate were run, `cargo audit` was run against
`Cargo.lock`, and the CI/nightly history was pulled from GitHub Actions.
Limits: no Docker and no provider key in the review sandbox, so the image
build, the smoke suite and the drain drill are reported from CI rather than
re-run here; `govulncheck` could not fetch its database through the sandbox
proxy.*

---

## Verdict

**The project is closer to production than any earlier review saw it, and
the remaining distance is operational, not architectural.** Since the 25 July
review the fleet landed 129 commits: the `fq`/`fqd` split, the registry-first
edge as the only client path, the dashboard as a second attenuated
principal, top-level frontmatter strictness, the size ratchets, a nightly live
suite that now actually runs (ten consecutive green runs), and a full
documentation reconciliation. CI on `main` is green. The verification culture
the earlier reviews praised has not slipped.

What stands between this tree and something an operator can run unattended
is a specific, finite list, and most of it is cheap:

1. **The daemon can be wedged, and nothing notices.** There is no timeout on
   an LLM HTTP call, on a tool call, on an MCP call, or on MCP server start-up
   at boot; a hung provider connection parks the only worker slot forever
   while the heartbeat keeps reporting it alive.
2. **Two durability holes in the record.** A `completed` event whose publish
   fails is never retried and the projection loses the outcome; and an
   invocation that serviced a sampling request cannot be resumed after a
   drain or crash — it is re-flagged ambiguous on every restart.
3. **Same-host trust is open in the flagship deployment.** The dogfood broker
   has no authentication and the Caddy admin API is reachable from loopback,
   while agent subprocesses run as the same user; separately, stdio MCP
   servers inherit the daemon's entire environment, provider keys included,
   and the broker credential is written into the event log.
4. **Growth is unbounded where it matters.** The worker WAL is never pruned
   and stores the whole conversation on every model call; the owner table has
   no retention and no index for its hottest query.
5. **Dependencies are unscanned and currently vulnerable** — ten advisories
   against the lockfile, seven warnings, and the edge wire format is an
   unmaintained crate.
6. **The security fixes the 25 July review asked for are still open six
   weeks on** (#399, #400, #405, #406) while 129 commits landed — the
   "review loop loses to velocity" finding now applies to security work, which
   is the strongest argument for making those items merge gates rather than
   issues.

None of this changes the architecture, and none of it is a rewrite. The plan
at the end is five phases; the first two are days of work each and remove
every "wedge" and "silent loss" path above.

## What "production level" should mean here

`STATUS.md` says, correctly, that there are no external users and no
compatibility promise. A production bar that ignores that would be theatre.
The bar this review plans toward is:

> **Production v0.1:** a tagged release a second operator can install from
> binaries and run unattended for 30 days on the dogfood workload, where every
> documented security boundary is either enforced or loudly declared
> unenforced; a stuck or failing component is detected within minutes and
> diagnosable from logs and a metrics endpoint; state is backed up and has
> been restored once in a drill; dependencies are scanned on every merge; and
> the only manual intervention in the 30 days is merging pull requests.

That is deliberately single-tenant and single-host. Multi-node, Memory, Skills
and container isolation are not on the path to it and are called out below as
things to *not* start until it is met.

## Where the project is — executed evidence

| Check | Result on `43726bb` |
|---|---|
| CI (`ci.yml`) on `main`, last 15 runs | all green (2 cancelled by superseding pushes, 0 failed) |
| Nightly live suites (smoke 6 cases + drain drill 15 checks, real model) | 10 consecutive green runs, 2026-08-26 → 2026-09-03 |
| Rolling `main-latest` artifacts | green; deployed by `ops/dogfood/deploy.sh` |
| Tagged release workflow (`release.yml`) | **has never run**; no git tags exist; workspace version `0.1.0` |
| Workspace build (`cargo build --tests`, every suite) | clean, 4m54s cold |
| Go gate (gofmt, vet, test, both adapters) | clean |
| Rust suites, every crate and target (see appendix) | green once the pinned `nats-server` was installed by hand — the sandbox proxy blocks the `just` installer, so `just install-nats` could not run, which is itself a small portability finding: every gate assumes `just` |
| Quality gate (`just quality`: source policy, size ratchets, fmt, clippy, creep, coupling) | green in 2m12s; the advisory coupling report shows `fq-runtime`'s import cycle at **15 modules**, up from the 11 recorded when #415/#424 were filed |
| `cargo audit` | **10 vulnerabilities, 7 warnings** (detail in §4 E1 and the appendix) |
| Open issues / PRs | 120 open issues; 5 open PRs, three of them small fixes from 2026-08-27 (#521, #524, #527) not yet merged |
| Commit cadence | 129 commits since 2026-07-25; none in the six days before this review |
| Test surface | ~1,295 Rust test functions, 70 Go tests; production LOC ≈ 41k Rust + 4k Go |

## What changed since the last reviews

The status of every finding the July reviews left open, so the plan below
sequences against reality rather than against the July snapshot.

**Fixed since 2026-07-25.** `fq-cli/src/lib.rs` split (233 lines now — #189
can close); the `fq`/`fqd` split with the thin-client and store-open gates;
dashboard reads over the edge as an attenuated principal; top-level
frontmatter `deny_unknown_fields`; live suites and the container image now
run in CI; `resume` replay ordering (a v9 `seq` column); `stop_reason` mapped
at the adapter; `self_inspect` gained a `context` section; hand-rolled
entropy replaced in the runner; `build_invocation_setup` extracted; projection
retention with cost rows exempt; registry refuses shadowing and namespaces
MCP tools; roots derived from the bound sandbox; exec drain bounded after
timeout; `write_atomic` fsyncs the parent; the store gate is scope-kind aware
and the object claim protocol (ADR-0030) is built; the watcher resets its retry
counter, uses a 30 s HTTP client instead of `gh`, rejects the wrong
`schema_version`, and no longer swallows malformed env; `deploy.sh` matches
`/proc/<pid>/exe`; ADRs carry an `Implementation:` line.

**Still open, tracked, and blocking the bar above.** Dangling-symlink write
escape #399 · `/proc` environment read #400 · `install.sh` fail-open #405 ·
no `cargo audit`/`deny`/Dependabot #406 · unpinned pricing #408 ·
`SCHEMA_VERSION` write-only #409 · edge token expiry/revocation #404 ·
network enforcement #208/#209 · exactly-once dispatch #327 (ADR-0032 draft,
unstarted) · `worker.orphaned` unconsumed #475 · second-SIGTERM escape #509 ·
provider 429 handling #278 · stuck-invocation detection #37 · metrics #342 ·
MCP dedup key #170/#523 (fix PR #521 open) · nested frontmatter strictness #520
(PR #527 open) · `turn.list` ceiling #465 (PR #524 open) · transcript
walk from sequence 1 #525 · `runner.rs` and `mcp.rs` still over the ratchet
cap #78/#191.

**Still open and not tracked as an issue** (from the July reviews): resume
returning permanent conditions as transient `WorkerStore(String)`; `map_error`
never yielding `RateLimited`; blocking `std::fs` in `workspace.rs`; relative
`[workspace] path`; `unsafe env::set_var` in tests; the store's raw-`put`
content reaped after grace, `.tmp.*` orphans, silent denials, quadratic
`recompute_liveness`; `datastar.js` without a checksum; the `find_browser`
`set -e` trap; the smoke sandbox-denial test that asserts nothing.

## New findings

Grouped by the property they break, ranked within each group. Every item
below was checked at the cited line on `43726bb`; the few that rest on a
reviewer's read of a longer path say so.

### A. Security

**A1 · HIGH · The broker credential is written into the event log, the
daemon banner and the INFO log.** `fq-daemon/src/hosted.rs:112` copies
`config.nats.url` — which by design carries the token or password as URL
userinfo (`bus.rs:202-224`, and the `fq init` template ships
`nats://fq-dev-token@…`) — into `SystemStartupPayload.nats_url`
(`fq-ops/src/events/payloads.rs:406`). `daemon.rs:61` prints the same URL and
`bus.rs:246` logs it. `event.get` serves whole payloads, so any token with
`read:event` — the dashboard's, for instance — can recover the broker
credential from `fq events query --event-type system_startup`, and with it
publish triggers and events directly, bypassing edge authorization entirely.
*Fix:* strip userinfo before the value leaves `Config` (one redaction helper
used by the banner, the tracing field and the payload); better, add
`[nats] token_env` and refuse userinfo in `url` at validation.

**A2 · HIGH · stdio MCP servers inherit the daemon's full environment and
cwd.** `fq-runtime/src/mcp.rs:1044-1049` builds the child with
`Command::new(&config.command)` plus `cmd.env(k, v)` for the declared
variables and nothing else — no `env_clear()`, no `current_dir`. Every
`command:` in any agent definition therefore receives `ANTHROPIC_API_KEY`,
`GH_TOKEN`, the NATS token and whatever else `.secrets/env` exports, with no
`/proc` trick needed; the `exec` built-in gets this right (`exec.rs:321`).
The authoring guide presents `env:` as *the* process environment. *Fix:*
mirror `exec`: `env_clear()`, the pinned `PATH` baseline, the declaration's
`env:`, a neutral `current_dir`, stderr piped into tracing.

**A3 · HIGH · The dogfood broker has no authentication.**
`ops/dogfood/infra/nats.conf` has no `authorization` block (the dev broker in
`infrastructure/nats/nats.conf` does). Loopback binding is the only control,
and agent `exec` children run on the same host as the same user with ambient
network: `bash`'s `/dev/tcp/127.0.0.1/4223` plus a `PUB fq.trigger.<agent>`
line triggers any agent (the groomer runs with a \$12 budget), forges
`fq.agent.<agent>.completed` to move an issue to in-review, or issues
`$JS.API.STREAM.DELETE`. A token would actually hold here, because `exec`
clears the child environment. *Fix:* token auth in `nats.conf` and the three
NATS URLs.

**A4 · HIGH · The Caddy admin API is reachable by every local process.**
`ops/dogfood/infra/docker-compose.yml` runs Caddy with `network_mode: host`
and the `Caddyfile` global block contains only `auto_https
disable_redirects`, so Caddy's default admin endpoint listens on
`localhost:2019`. Any local process — the same agent children as A3 — can
`GET /config/` (which contains the substituted `DASH_HASH` and `DASH_COOKIE`)
or `POST /load` a config without `basic_auth`. *Fix:* `admin off` in the
global block.

**A5 · MEDIUM · `fq connect` pins whatever the network presents when stdin
is not a terminal, then sends the admin token to it.**
`fq-cli/src/connections.rs:192-194` falls through to "non-interactive:
pinning automatically" and `edge_client` writes the bearer token in the
preamble. Every scripted pairing without `--fingerprint` is a
trust-on-first-use against an active attacker, and the daemon is explicitly
allowed to bind non-loopback. *Fix:* non-interactive requires `--fingerprint`
or an explicit `--trust-first-use`.

**A6 · MEDIUM · Secrets land in the wrong sinks.** The admin token is printed
to stdout once (`fq-daemon/src/edge_identity.rs:66-67`), i.e. into journald
or `docker logs`, and an operator who missed it can only recover by deleting
the identity and re-pairing every client. `fq-dashboard --help` renders the
edge token's value because `FQ_EDGE_TOKEN` lacks `hide_env_values`
(`fq-dashboard/src/main.rs:78`; the store CLI got this right).
`ops/dogfood/dashboard.sh` exports every secret in `.secrets/env` into the
one web-facing process. *Fix:* write `admin.token` 0600 beside `root.key` and
print the path; `hide_env_values = true`; export only the three variables the
dashboard reads.

**A7 · MEDIUM · MCP tools are not sandboxed, and the guide says they are.**
Both MCP `Tool` impls take `_ctx` and never consult the sandbox
(`mcp.rs:682-686`, `:821-825`), while `docs/guide/mcp.md:68` promises "the
same sandbox/budget/event machinery as built-ins" and the authoring guide's
own example points `server-filesystem` at `/data` with no relation to
`fs_read`. Combined with A2 a filesystem or shell MCP server is an
unsandboxed agent running with the daemon's privileges. *Fix now:* correct
the two documents (MCP is server-enforced, per-server trust). *Fix later:*
derive server arguments from the sandbox, or refuse filesystem-shaped servers
whose arguments fall outside it.

**A8 · MEDIUM · Server-initiated spend is bounded only after the fact.**
`sampling_budget`/`elicitation_budget` are optional (`fq-agent/src/definition.rs:144-150`)
and nothing requires either when `sampling: true` is declared; there is no
request counter, so a server that holds one `tools/call` open can stream
`sampling/createMessage` requests for as long as the invocation lives, each
with the server's own `max_tokens` unclamped
(`runner/server_request.rs:456-483`). The agent-turn budget check is itself
post-hoc (`runner/llm.rs:53-71`): a turn is still dispatched when
`total_cost` already exceeds `budget`. *Fix:* refuse to load a definition
that grants sampling or elicitation without both budgets; a per-invocation
and per-tool-call request cap; clamp `max_tokens`; a pre-flight refusal when
`total_cost >= budget`.

**A9 · LOW · `exec` kills only the direct child.** `exec.rs:326/378` uses
`kill_on_drop` and `start_kill` on one pid; there is no `process_group` or
`killpg`. A timed-out `["bash","-c","<payload> & sleep 999"]` leaves the
payload running as the daemon's user indefinitely, invisible to the runtime.
The module doc defers this to ADR-0010, but it is a five-line fix
(`process_group(0)`, `killpg` on timeout) that should not wait for
containers.

**A10 · LOW · `file_list`/`file_search` follow symlinked directories.** The
`glob` walk in `discovery.rs:80-85` recurses into symlinks, so `ln -s . a; ln
-s . b` makes `**` exponential and `ln -s / root` walks the whole filesystem
before any `check_read` runs. #188 covers the blocking/unbounded half; the
symlink half is new. *Fix:* a walker that does not follow links and applies
the limit during the walk.

**A11 · LOW · `fq-cas serve` neither enforces loopback nor bounds work.**
`service.rs:158/164`: 256 MiB frames, `for_each_concurrent(None, …)`, no bind
check (the dashboard has one). Not deployed today, so exposure is nil; a
pre-deployment blocker for the store, alongside the tracked #183.

### B. Availability — things that wedge the daemon

**B1 · HIGH · No timeout on LLM HTTP calls.** `llm/genai.rs:40` builds
`provider::Client::default()`, whose web config has no request, connect or
read timeout, and the runner awaits `chat` with no `tokio::time::timeout`
(`runner/llm.rs`). `RetryingLlmClient` only acts on returned errors. A
half-open connection or a stalled provider parks the invocation inside
`dispatch_llm`; drain cannot suspend it (step boundaries only), so `fq down`
waits out its deadline and hard-stops; with the default
`max_concurrent_invocations = 1` the daemon does no work until a human
restarts it. Restart is safe (the WAL row is still `intent`), but nothing
detects the condition: the heartbeat measures process liveness, not progress
(`worker/heartbeat.rs:83-101`), so the stale-worker sweep never fires.
*Fix:* a `[worker] llm_timeout_secs` applied at the client and as a `timeout`
around `chat`, mapped to a transient error so the retry policy applies.

**B2 · HIGH · No timeout on tool or MCP calls, and `file_read` accepts a
FIFO.** `run_tool` awaits `tool.execute` with no deadline; `McpTool::execute`
is a bare `call_tool(...).await` (`mcp.rs:704-708`) although
`call_tool_cancellable` (`mcp.rs:1387`) exists with no production caller.
`check_read` only canonicalises, so `file_read` on a `mkfifo` path blocks in
`open()` forever (`file_read.rs:66`). Only `exec` has a deadline. *Fix:* a
`[tools] call_timeout` applied in `run_tool` to every tool, MCP calls routed
through `call_tool_cancellable`, and `check_read` requiring a regular file.

**B3 · HIGH · Daemon boot blocks on one unresponsive MCP server.**
`fq-daemon/src/daemon.rs:223` starts shared servers sequentially, before the
edge or any consumer is up, and `mcp.rs` has no timeout around the
`initialize` handshake or `list_all_tools`, which follows `next_cursor`
without bound; the stdio codec is built with an unlimited line length. A
remote server that accepts TCP and never answers freezes `fqd` at startup
with every agent down; a server returning a cursor forever spins discovery
with unbounded memory, re-triggerable via `tools/list_changed`. *Fix:*
timeouts around start-up and discovery, page and tool-count caps, concurrent
start, and a failed server marked unavailable rather than blocking boot.

**B4 · HIGH · Transient consumer errors NAK with no delay on durables with
unlimited redelivery.** `control_plane/durable_consumer.rs:386` answers
`HandlerError::Transient` with `Nak(None)`, which the dispatcher's own comment
(`dispatcher.rs:69`) says "redelivers immediately", and every event-stream
durable is created with default `max_deliver` and `ack_wait`. A
`SQLITE_FULL` on the projection, or a failed ack publish in the coordination
consumer, becomes a hot loop at broker round-trip speed: two `error!` lines
per iteration, the watermark frozen so every `min_seq` read fails `Lagging`,
and `control.status` reporting nothing wrong because `health.rs:20-23`
probes only the projector and dispatcher, not the coordination, heartbeat,
summary or advisory consumers. The daemon does not exit, so no supervisor
would help. *Fix:* `Nak(Some(delay))` escalating on `delivered`, explicit
`ack_wait`, rate-limited error logging, and health coverage of every durable.

**B5 · MEDIUM · Boot side effects precede the edge bind, and there is no
instance lock.** `daemon.rs:173` registers the worker and `:188` runs recovery
(publishing `system.recovery` and taking ownership of resumable invocations)
before `hosted.rs:188` binds the edge. A bind failure — the port held by a
draining predecessor, a typo, a privileged port — unwinds with a live worker
row that ages into `stale`, resume tasks cancelled mid-step, and no
`system.shutdown`. `hosted.rs`'s doc says the edge "now binds before the
first spawn", which is true inside `run_hosted` and false for the process.
There is no pid or lock file, so with `[edge] enabled = false` two daemons
can open one store. *Fix:* bind the listener first in `run_daemon` and let it
double as the single-instance lock; `mark_worker_shutdown` on every error path
after registration.

**B6 · MEDIUM · The edge accept loop has no backpressure.**
`fq-edge/src/server.rs:220-222` does `continue` on any `accept` error, and
tokio does not clear readiness on `EMFILE`, so file-descriptor exhaustion
becomes a 100 % worker thread; nothing caps pre-auth connections (each an fd,
a TLS handshake and a task) and tarpc's `max_concurrent_requests` is not set,
so one authenticated client can queue unbounded in-flight requests. *Fix:*
sleep on accept error, a semaphore on pre-auth handshakes and total
connections, `max_concurrent_requests` on the channel.

**B7 · MEDIUM · Both Go adapters die or wedge after two minutes without the
broker, and nothing restarts them.** `adapters/fq-cron/main.go:62` and
`adapters/github-watcher/publisher.go:24` call `nats.Connect(url)` with no
options; nats.go's defaults give up after 60 reconnects at 2 s. Past that
`fq-cron` fails `loadState` and exits; the watcher keeps polling with a dead
connection, relabelling ready→in-progress and reverting on every cycle while
its outcome subscriptions are gone. Neither uses `RetryOnFailedConnect`, and
`deploy.sh` launches all four processes concurrently while only `run.sh`
waits for the broker's `/healthz`. The launchers are `setsid … &`: no
restart on crash and nothing at host reboot except the containers. *Fix:*
`MaxReconnects(-1)`, `RetryOnFailedConnect(true)`, disconnect handlers; and
systemd units with `Restart=always` — the README calls systemd out of scope,
and that decision is the single largest gap between "dogfood" and
"production".

**B8 · MEDIUM · `fq-cron` stalls permanently once its per-hour valve
trips.** `plan.go:93-103` returns no fires when `used >= limit`, and the loop
(`loop.go:26-47`) then waits only on context or a config reload; no timer
re-plans when the window slides. After a burst the scheduler is silent until
someone edits the file. *Fix:* arm a timer for when the oldest in-window fire
leaves the window.

**B9 · LOW · Drain ordering erodes the deadline.** `hosted.rs:516-613`
computes the drain deadline, then runs up to eight sequential 5 s joins
(heartbeat already stopped) before joining the dispatcher against that same
deadline — up to 40 s of the default 120 s gone before the drain wait starts,
with the worker not heartbeating while still executing steps.

**B10 · LOW · The dashboard amplifies onto the edge.** Every request dials a
fresh TLS connection with no connect timeout (`pages.rs:87-91`,
`fq-edge/src/client.rs:231-261`); every open tab polls each 5 s, two RPCs per
tick on two pages; a hand-typed `/transcript/stream` URL for a finished run
long-polls indefinitely. Behind Caddy auth this is operator-inflicted, not
anonymous, but twenty tabs are four handshakes a second on the daemon.

### C. Durability and correctness of the record

**C1 · HIGH · A lost `completed` publish loses the outcome forever.**
`runner.rs:1250-1253` marks the WAL row terminal (`phase_and_terminal_from`
→ `upsert_invocation_state`) *before* `:1288` publishes `completed`. If that
publish fails, `run_loop_inner` returns `Err(Bus)`, `reclaim_if_terminal`
deletes the workspace, and the archive sweeper later republishes only
`invocation.archived`, which the projection maps to `Fields::default()` — so
`task_status`, `result_summary` and the totals are gone while the
coordination store says archived. `emit_failed` uses the opposite order and
self-heals. *Fix:* persist the terminal outcome on the row and let the
sweeper republish a missing terminal event; or publish first and accept an
idempotent duplicate on crash.

**C2 · HIGH · Resume cannot survive an invocation that serviced sampling or
elicitation.** `llm_dispatch` has no origin column
(`worker/store.rs:142-154`; `write_llm_intent` takes none), so server-initiated
calls land in the same table as agent turns, and `resume` replays every
completed row into the harness in sequence order. A replay of
`[Model(turn), Model(sample), Tool(result)]` fails with "expected ToolResult
after CallTool, got ModelResult", the daemon publishes `invocation.ambiguous`,
and every restart repeats it; a failed sampling row is additionally treated as
the invocation's fate although sampling failures are non-fatal live. No test
covers resume after sampling. This is the resume/fresh equivalence invariant
(#410) with a hole the verification plan's oracle does not model. *Fix:* a
schema bump adding `origin` to `llm_dispatch`; replay agent-turn rows only,
summing cost from the rest; extend the resume-equivalence DST with sampling.

**C3 · HIGH · No `Nats-Msg-Id` on any publish, so a lost publish ack cannot
be retried safely — and ADR-0032 says otherwise.** There is no `Nats-Msg-Id`
header and no `duplicate_window` anywhere under `services/` (only the Go
scheduler sets one). `dead_letter.requeue` releases its claim on a publish
error "so the operator can try again" and the edge maps other publish errors
to a retryable `Internal`; JetStream's ack times out at 5 s while the message
may well be stored. The retry therefore publishes a second trigger with a
fresh id — the duplicate-invocation class of #327, from the publish side.
ADR-0032's alternatives section states publish-side dedup is "already in
place, 120s window"; it is not, and the ADR's argument should be corrected
when the fix lands. *Fix:* `Nats-Msg-Id: <trigger_id>` / `<event_id>` on
every publish, an explicit `duplicate_window` on both streams, and a timed-out
ack treated as unknown rather than failed in the requeue path.

**C4 · MEDIUM · An oversized event wedges an invocation permanently and is
classified transient.** `llm.request` embeds the full message history;
`file_read` is an uncapped `read_to_string` (`file_read.rs:66`) and MCP text
results are joined uncapped (`mcp.rs:713-718`); only `exec` truncates. Past
the broker's `max_payload` (1 MiB on a stock broker; the dogfood host raised
it to 16 MiB after exactly this incident) the pre-flight guard returns
`PayloadTooLarge`, `run_loop_inner` exits with no terminal event, and
`ExecutorError::Bus` is `is_transient() == true` (`worker/mod.rs:178`), so
recovery replays the same oversized request on every restart: a zombie
flagged ambiguous, workspace never reclaimed. *Fix:* a runtime-wide tool
result cap applied in `run_tool` with the honest "kept X of Y" note `exec`
already uses; `PayloadTooLarge` classified permanent with a `failed` event;
and `llm.request` carrying the delta rather than the whole history.

**C5 · MEDIUM · Recovery and operator resumes run outside the dispatcher's
concurrency permit.** `fq-daemon/src/recovery.rs:312` and `resume.rs` spawn
detached tasks; the semaphore is private to the dispatcher
(`dispatcher.rs:243`). The per-invocation-workspace requirement is only
enforced when `max_concurrent_invocations > 1`, so after a crash the resumed
invocation and the next dispatched trigger share one directory — the exact
clobbering the config comment warns about — and effective concurrency is
`resumed + max_concurrent` against a rate limit sized for one. *Fix:* hand
the dispatcher's permit to resume, or require per-invocation workspaces
whenever recovery can resume.

**C6 · MEDIUM · The summary consumer replays the whole retained log with one
paid model call per event.** `summary_consumer.rs:149` binds the production
durable with `DeliverFrom::Beginning`, and the shared loop pulls with the
client's default batch of 200 under the default 30 s `ack_wait`. Enabling
`[summary] model` on an existing deployment, or losing the durable, pays one
summariser call for every lifecycle event of the last 30 days; under any
burst, messages past the first ~15 exceed the ack window before they are
handled and are redelivered as further paid calls. (#453 is a different
defect in the same consumer.) *Fix:* deliver from now, batch of one, an
`ack_wait` above worst-case model latency, or ack before the call — its own
charter says a missing line is cosmetic.

**C7 · MEDIUM · The watcher's relabel is not an atomic claim.**
`adapters/github-watcher/github.go:86-99` removes `status:ready` (404
tolerated) and then adds `in-progress`. Two watchers — the overlap
`deploy.sh` itself records — both remove, both add, both publish: a double
trigger. A failure between the two calls leaves an issue with no status label
that no list ever sees again. *Fix:* add-then-remove (both labels present is
already skipped), a 404 on remove treated as "claim lost", and a
single-instance lock.

**C8 · LOW · `max_tokens` is hard-coded and truncation is invisible to the
harness.** `harness.rs:489` sends `max_tokens: Some(4096)` with no agent or
daemon knob, and no production code in the harness reads `stop_reason` now
that the adapter reports it. A truncated text turn is answered with the
"continue working" notice and another full-context turn; a `file_write`
whose arguments exceed ~4 K tokens can never complete and loops to
`max_iterations`.

**C9 · LOW · `HOST_STEP_BUDGET` silently overrides large `max_iterations`.**
`runner.rs:100-104` caps steps at 1,000 with two or more steps per turn, so
any `max_iterations` above ~500 (unvalidated) ends as a `RuntimeError` rather
than `MaxIterations`.

**C10 · LOW · Store-only, not on the runtime path yet.** The GC grace is a
wall-clock cutoff (`audit.rs:69-85`) that zeroes in-flight reservations older
than `--grace`, which accepts `0` and is not re-verified when `bind` inserts
edges (per the store reviewer's read of `index.rs`), so a writer slower than
the grace — or a forward clock step — loses live data; read-then-write
transactions use deferred `BEGIN`, so the documented "gc from cron on a live
store" can fail with `SQLITE_BUSY_SNAPSHOT` and leak reservations on that
error path; the audit counts an I/O error on `has()` as "present"
(`verify.rs:243-253`) and reports every invariant holding; the grant outbox
pump has no production caller although the access-control guide says it is
wired. Fine while nothing depends on the store; blockers before Memory or
Skills do.

### D. Growth and capacity

**D1 · HIGH · `worker.db` grows without bound, quadratically per
invocation.** The only `DELETE` in the worker store is `invocation_state`
(`worker/store.rs:1150`); `tool_dispatch`, `llm_dispatch` and `host_notice`
rows are kept forever by design for the transcript view, and the
`[state] retention_days` sweep covers neither. Each `llm_dispatch.request_payload`
is the *entire* serialised chat request, so a 100-turn invocation with a 100 K
context persists roughly 100 × 400 KB of near-duplicate JSON, and the state
blob is rewritten every step. A modest fleet fills tens of gigabytes a month
with no knob and no warning, and every per-invocation scan slows with it.
*Fix:* a WAL retention sweep keyed on archive ack; store the request as the
delta since the previous call (the transcript reads only the first prompt);
`PRAGMA incremental_vacuum`.

**D2 · MEDIUM · `coordination_invocation_owner` has no retention and no
index for its hottest query.** Every archived invocation upserts an owner row;
only `drop` ever deletes one. The sole index is `(worker_id, status)`
(`control_plane/store.rs:70`), so `invocation.list` and `recovery()` — which
every `control.status`, `control.doctor` and dashboard poll calls — are full
scans plus sort, and `sweep_archive` is one unbatched `DELETE` holding the
write lock (the projection's sweep is batched). *Fix:* sweep terminal owner
rows on the archive cutoff, `(status, assigned_at)` and `(assigned_at)`
indexes, a batched archive delete.

**D3 · LOW · Streams have no byte bound and the projection fsyncs three
times per event.** Only `max_age` is set on the three streams, so
`discard: Old` never engages and storage exhaustion is a hard stop for every
publish; `projection.db` runs sqlx's default `synchronous=FULL` with up to
three autocommit statements per event under a one-in-flight consumer, which
caps throughput at fsync latency for a disposable store.

**D4 · MEDIUM · Host operations have no rotation, no backups, no lock.**
Logs append unbounded (`deploy.sh:288-295`; `cron.sh` double-appends);
nothing backs up the JetStream volume or the SQLite stores, although the
event-schema doc now says `projection.db` is the only copy of cost history;
`deploy.sh` has no `flock`, so two concurrent deploys each bring the stack
down and up; and a daemon that never reaches "Runtime ready" leaves `current`
flipped with the watcher, dashboard and cron running on a build that has no
daemon.

### E. Supply chain and release engineering

**E1 · HIGH · The lockfile is currently vulnerable, and the edge wire format
is an unmaintained crate.** `cargo audit` on `Cargo.lock` reports ten
advisories: `h2 0.4.15` (unbounded empty DATA frames, RUSTSEC-2026-0258),
`rustls-webpki 0.103.11` ×3 (name-constraint bypasses and a reachable panic
in CRL parsing, 2026-0098/0099/0104) — this is the TLS stack under the edge,
the LLM client and the MCP HTTP transport — `rustls-webpki 0.102.8` ×4 pulled
by `async-nats 0.38` alone, plus `quinn-proto` and `rsa` entries that exist
only in the lockfile (no built feature reaches them). Seven warnings:
`bincode 1.3.3` unmaintained — the crate tarpc's `serde-transport-bincode`
frames every edge request with — `rustls-pemfile`, `paste`,
`proc-macro-error2` unmaintained, and `anyhow`, `event-listener`, `rand 0.8`
unsound. A dry-run `cargo update` patches `h2`, `quinn-proto` and
`rustls-webpki 0.103`; the `0.102` line needs `async-nats` past 0.38, which
the exactly-once plan already wants for its pull-buffer behaviour. #406
tracks adding the tooling; the point here is that the tooling would be red
today, and that it should fail CI, not report.

**E2 · MEDIUM · The build is not reproducible or attested.** Every
third-party action is pinned to a mutable tag (`actions/checkout@v4`,
`dorny/paths-filter@v3`, `Swatinem/rust-cache@v2`, `extractions/setup-just@v3`
with no version, `mozilla-actions/sccache-action@v0.0.10`); `build-release`
(`justfile:646-649`) runs without `--locked`, so a release build can resolve
a different graph than the reviewed lockfile; no SBOM or provenance
attestation exists on either channel, and the `.sha256` `deploy.sh` checks is
published by the same token that publishes the tarball, so it catches
corruption, not tampering; `publish-main` deletes the release and tag before
recreating them under `cancel-in-progress: true`, so a superseding merge in
that window leaves the channel empty; `nats:latest` is the image in all three
compose files while the test broker is pinned by checksum.

**E3 · MEDIUM · The container story is behind the daemon.** The `Dockerfile`
sets no `FQ_STATE_DIR` and declares no volume for it, so the edge identity —
the one thing the README says must never be regenerated — lands in the
ephemeral non-root home and every recreation orphans all paired clients;
`FQ_NATS_URL=nats://nats:4222` carries no token and cannot reach the
token-requiring dev broker; there is no `HEALTHCHECK`. `Dockerfile.shell-test`
omits the `fq-agent`, `fq-daemon` and `fq-lint` member manifests and stubs a
binary that moved crates — the exact failure the `docker` CI job was added to
catch — and no job builds it.

**E4 · LOW · Release hygiene.** The release workflow has never run and no tag
exists; the release bundle omits both adapters; neither workflow runs
`--version` on what it built (the only coherence check is on the dogfood
host, and it excludes `fq-cron`, which has no version flag); three smoke
content assertions cannot fail because `transcript --full` echoes the prompt
and tool output they grep for (`smoke.sh:379/412/446`);
`.local/state/gh/device-id` is committed agent scratch (`4db1955`).

### F. Observability specifics

The floor is tracked (#342, #37); three specifics are new: `control.status`
probes two of the six durables (B4); the summary consumer is outside the
supervised `select!`, so its stream ending exits silently; and the heartbeat
reports process liveness rather than the last step boundary, so a wedged
invocation (B1, B2) never looks stale. The dispatcher's ack/NAK log lines
carry no `trigger_id` or subject.

### G. Process

Two observations, both new since July. First, the security items the 25 July
review ranked first (#399, #400, #405, #406) are all still open after 129
commits, three of them `fleet:candidate` and none dispatched — structural
work was the July finding, and issue-shaped security work turns out to lose
to velocity the same way. Second, three small fix PRs from 2026-08-27
(#521, #524, #527) sit unmerged, and the tree has been quiet for six days; the plan
below starts by merging them because two of them close items on this list.
Third, the coupling report that #424 made advisory has moved the wrong way:
the `fq-runtime` import cycle is now 15 of the crate's top-level modules
(`bus, config, control_plane, db, dead_letter, events, llm, mcp, pricing,
prompt, tools, transcript, trigger, views, worker`), against 11 in July. It
does not block the plan, but it is the measurement the July metrics review
said to gate on, and it is still only reported.

## The plan

Five phases, each with an exit criterion that is a query or a command, not
an opinion. Items marked **fleet** are small and well-specified enough for
the `m0-issue-fix` loop; the rest need the maintainer. Effort is
maintainer-days, deliberately rough. Issue numbers are the existing trackers;
letters are findings above.

### Phase 0 — Stop the bleeding (2–3 days)

1. Merge #521, #524, #527 and #529. **fleet-adjacent**
2. `cargo update`, bump `async-nats` past 0.38, and add `cargo audit` +
   `cargo deny` as a `just` target that CI runs and **fails** on; add
   Dependabot for cargo, gomod and actions (#406, E1). Pin `nats` image tags.
   **fleet**
3. Redact the broker credential from the startup payload, banner and log;
   add `[nats] token_env` (A1). Token auth on the dogfood broker (A3).
   `admin off` in the Caddyfile (A4). `hide_env_values` on the dashboard
   token; export only what the dashboard needs (A6). **fleet** except A1.
4. `env_clear()` + pinned `PATH` + declared `env:` + neutral cwd for stdio
   MCP servers (A2); correct `docs/guide/mcp.md` and the authoring guide on
   MCP sandboxing (A7).
5. Make `install.sh` fail closed (#405), require `--fingerprint` for
   non-interactive `fq connect` (A5), write the admin token to a 0600 file
   instead of stdout (A6), `git rm .local`. **fleet**

*Exit:* `just ci` includes a red-on-advisory audit gate and is green; `fq
events get` of a `system_startup` event contains no credential; the dogfood
broker rejects an unauthenticated `PUB`; `curl localhost:2019/config/` on the
dogfood host is refused.

### Phase 1 — Nothing can wedge the daemon (1–2 weeks)

1. LLM call timeout, configured, mapped to a transient error (B1); handle
   429 with `Retry-After` while in the same code (#278).
2. Tool and MCP call timeout in `run_tool`, `call_tool_cancellable` wired,
   `check_read` requires a regular file (B2).
3. MCP start-up and discovery timeouts, page and tool caps, concurrent start,
   failed server marked unavailable (B3).
4. `Nak(Some(delay))` with escalation and explicit `ack_wait` on every
   durable; health probes over all six consumers; summary consumer supervised
   (B4, F).
5. Bind the edge first and use the listener as the instance lock;
   deregister on every post-registration error path (B5); accept-loop sleep
   and connection/request limits (B6). The second-SIGTERM escape (#509) and
   drain ordering (B9) belong in the same file and the same PR.
6. Adapters: `MaxReconnects(-1)`, `RetryOnFailedConnect`, handlers (B7);
   cron valve timer (B8); watcher add-then-remove claim (C7). **fleet**
7. Process-group kill on `exec` timeout (A9). **fleet**
8. Stuck-invocation detection (#37) as the safety net, with "last step
   boundary" carried on the heartbeat (F).
9. systemd units for `fqd`, watcher, dashboard and cron with `Restart=always`
   and the drain deadline as `TimeoutStopSec`; retire the `setsid` launchers
   (B7). This reverses a documented decision; it is the cheapest single step
   toward the 30-day bar.

*Exit:* a fault-injection test per wedge class (hung provider, hung MCP
server at boot, hung tool, FIFO read, consumer `SQLITE_FULL`) that ends in a
terminal event or a stale worker within one timeout; `fq doctor` reports
every consumer; `systemctl status` shows five units.

### Phase 2 — The record is trustworthy (1–2 weeks)

1. Terminal outcome persisted on the WAL row and republished by the sweeper
   when the terminal event is missing (C1).
2. `origin` on `llm_dispatch`; replay agent turns only; the resume-equivalence
   DST extended with sampling and elicitation (C2, #410).
3. `Nats-Msg-Id` on every publish, explicit `duplicate_window`, timed-out ack
   treated as unknown in requeue; correct ADR-0032's text (C3). **fleet**
4. Tool result cap in `run_tool`; `PayloadTooLarge` permanent with a `failed`
   event; `llm.request` carries the delta (C4).
5. Resume under the dispatcher's permit, or per-invocation workspaces
   required whenever recovery can resume (C5).
6. Summary consumer: deliver from now, batch of one, sane `ack_wait`, and
   the subject fix (#453, C6). **fleet**
7. `SCHEMA_VERSION` read path and a golden event corpus in CI (#409, #411).
8. Sampling and elicitation require budgets, a request cap, clamped
   `max_tokens`; pre-flight budget refusal (A8). `max_tokens` configurable and
   `MaxTokens` handled (C8); `max_iterations` validated against the step
   budget (C9). **fleet** for the last two.
9. The dangling-symlink write escape (#399) and `PR_SET_DUMPABLE` (#400):
   half a day each, and the review that found them is six weeks old.
10. Then, and only then, the ADR-0032 claim registry, PR-1 through PR-3 of
    the exactly-once plan (#327). It is the largest item on this list and the
    one the dogfood has run without since July; the pull-discipline PR-5
    becomes easier after the `async-nats` bump in Phase 0.

*Exit:* the crash-DST suite includes a lost-`completed`-publish case, a
resume-after-sampling case and an oversized-tool-output case, all green; the
incident-replay integration test from the exactly-once plan passes; `cargo
audit` still green.

### Phase 3 — Bounded growth and operable state (1 week)

1. WAL retention sweep keyed on archive ack; delta request payloads;
   incremental vacuum (D1).
2. Owner-table retention and indexes; batched archive delete (D2). **fleet**
3. `max_bytes` with `discard: Old` on the streams; `synchronous=NORMAL` and
   one transaction per event on the projection (D3). **fleet**
4. logrotate, nightly `nats stream backup` plus SQLite `.backup` to
   off-host storage, a restore drill documented and run once, `flock` in
   `deploy.sh`, and the post-deploy health gate with rollback (#339, D4).
5. A Prometheus endpoint on the daemon with consumer lag, in-flight count,
   oldest in-flight age, spend per hour, and one alert on lag (#342). Not a
   platform: one endpoint, five gauges.
6. Decide the event trail's lifetime: either wire the ADR-0026 archive (the
   store is built; the daemon does not call it) or write the 30-day window
   into README's "auditing, replay" claim.

*Exit:* `worker.db` and `control-plane.db` sizes are flat over a
seven-day dogfood window; a restore from backup on a clean host boots and
answers `fq status`; the alert has fired once in a drill.

### Phase 4 — Release engineering (1 week)

1. SHA-pin every action; `--locked` on release builds; provenance
   attestation on both channels; upload-then-retarget in `publish-main`
   (E2). **fleet**
2. `Dockerfile`: `FQ_STATE_DIR` under the volume, `HEALTHCHECK`, a token in
   the broker URL; delete or fix `Dockerfile.shell-test` and build it in CI
   (E3). **fleet**
3. Adapters in the release bundle with `--version`; CI runs `--version` on
   every built artifact; fix the three smoke assertions to grep the model's
   output, not the transcript (E4). **fleet**
4. Edge token expiry and revocation (#404), because a release means the
   perpetual admin token starts existing on machines other than the
   maintainer's.
5. Cut `v0.1.0` through `release.yml` — its first run — and install it on a
   clean VM with `install.sh`; write the upgrade and rollback SOP even if the
   answer is "stop, delete the stores, re-pair".
6. `SECURITY.md` gains the residual list the 25 July review asked for (#401),
   updated for A2/A3/A4 and for `exec` dominating the sandbox.

*Exit:* a tagged release exists, was installed from binaries on a host that
never had the source, and ran the smoke suite there.

### Phase 5 — Prove it (30 days, ~1 day of work)

Run the dogfood instance on the tagged release for 30 days with the
capability-ladder instrument's minimum viable version (#413: an attempt
ledger and a hand-kept intervention log). Exit: zero interventions of type
`unblock`, `restart` or `fix` attributable to a runtime defect; every consumer
healthy in `fq doctor` on every daily check; backups restored once mid-window;
`cargo audit` green on every merge. Then update `STATUS.md`'s maturity
section — its own text names "the first external consumer" as the revisit
trigger, and a release is how one appears.

### What not to start before Phase 2 is done

The storage M3–M5 track, Memory and Skills, the multi-node vertical (#414
is already held), FUSE/VFS (ADR-0028/0029), and network enforcement via
containers (#209). Each is real work with a clear finish line, which is
exactly why they are tempting; none of them makes the daemon harder to wedge
or the record harder to lose. The one exception is #208, the egress proxy,
which is the highest-value control for agents that ingest public issue text —
schedule it as the first item after Phase 2.

### Sequencing note

Phases 0 and 1 are almost entirely additive (timeouts, limits, config, ops
files) and can run in parallel with the fleet on the **fleet** items. Phase
2 touches `runner.rs` and the WAL schema and should be done by hand, in
order, one PR each — the same reason the July review gave for the
`runner.rs` split, which remains open (#78) and would make C1/C2 cheaper if
it went first.

---

## Appendix — executed evidence

**Build.** `cargo build --tests` over every workspace member with
`fq-store/cli,service`: clean, 4m54s cold on the pinned 1.95.0 toolchain.

**Go gate.** `gofmt -l` empty, `go vet` clean, `go test ./...` ok for
`adapters/fq-cron` and `adapters/github-watcher`.

**Rust suites.** All green with the pinned broker present: the runtime
crates (`--no-fail-fast`, every target, including the 542-test `fq-runtime`
library suite and the 42 NATS-backed integration tests) exit 0 in 158 s; the
store suite (108 library tests plus the integration targets, the failpoint
interleavings and the `grant_bus` broker test); the dashboard (44 + 1); and
`fq-test-support` (3). The first pass in the sandbox showed 173 failures, all
of them spawn errors at `fq-test-support/src/lib.rs:154` because
`.tools/nats-server` was absent: the `just` installer is blocked by the
sandbox proxy, so `just install-nats` never ran. Installing the pinned broker
by hand (checksum verified against `.nats-checksums`) turned every one of them
green. Two portability notes fall out: every gate assumes `just`, and the
test-support crate's spawn error names the missing binary, which is what made
the diagnosis a one-liner.

**Quality gate.** `just quality` green in 2m12s on the pinned toolchain:
the `include!` scan, both size ratchets, `cargo fmt --check`, clippy per
crate with each crate's feature set, and the two advisory reports. Across the
clean run, 1,066 tests passed, 0 failed, 1 ignored in the runtime,
`grant_bus` and test-support targets, on top of the store and dashboard
suites run earlier.

**`cargo audit` (advisory DB as of 2026-09-03).** RUSTSEC-2026-0258 h2
0.4.15 · RUSTSEC-2026-0185 quinn-proto 0.11.14 (lockfile-only) ·
RUSTSEC-2023-0071 rsa 0.9.10 (lockfile-only, no patch) · RUSTSEC-2026-0049,
-0098, -0099, -0104 rustls-webpki 0.102.8 (via async-nats 0.38) ·
RUSTSEC-2026-0098, -0099, -0104 rustls-webpki 0.103.11 (via rustls 0.23
under fq-edge, reqwest, tokio-rustls). Warnings: bincode 1.3.3, paste 1.0.15,
proc-macro-error2 2.0.1, rustls-pemfile 2.2.0 unmaintained; anyhow 1.0.102,
event-listener 5.4.1, rand 0.8.5 unsound. `cargo update --dry-run` would move
h2 → 0.4.19, quinn-proto → 0.11.17, rustls-webpki → 0.103.15. 514 packages in
the lockfile; 60 crates present in two or more versions.

**CI history.** `ci.yml` runs 890–907 on `main` all `success` or
`cancelled`; `live-suites.yml` runs 2–11 `success` (run 1 failed on the
key-presence check and was fixed the same day); `main-artifacts.yml` runs
223–228 `success`; `release.yml` zero runs.
