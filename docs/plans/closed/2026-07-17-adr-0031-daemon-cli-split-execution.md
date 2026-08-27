# Daemon / CLI split — execution plan for `fqd` + `fq` (ADR-0031)

> **Closed 2026-08-26 — filed, not finished.** This plan marked itself
> superseded on 2026-07-20 but stayed in `active/` for five weeks, so
> anyone reading the active set found two plans for one piece of work.
> Nothing here was executed under this document: the successor plan
> carried its survivable parts forward and ran them to completion.
> Moved on the strength of that successor closing — the migration gate
> (`fq-cli/tests/edge_migration_gate.rs`) reaching `REMAINING = 0` on
> 2026-08-14, and the binary split landing on 2026-08-23. Kept as the
> record of the `ControlService` design that ADR-0006's Appendix A
> replaced. The body below is unedited.

**Status:** superseded (2026-07-20) by the
[joint ADR-0006 + ADR-0031 execution plan](2026-07-20-registry-and-split-execution.md).
ADR-0006's Appendix A replaced the hand-enumerated `ControlService` this plan
builds around with the derived registry edge; the golden-master net, the
split-last ordering, and the settled auth decisions carry forward there.

Turning [ADR-0031](../../adrs/accepted/0031-daemon-cli-split.md) into PR-sized
slices: split the single `fq` binary into a daemon (`fqd`) and a thin operator
CLI (`fq`) that speaks one typed tarpc **`ControlService`**, authenticated by a
shared secret over TLS.

## The bet: most of the machinery already exists

This is less a build than a re-drawing of boundaries around parts that are
already here:

- **The RPC discipline is established.** `ReadService` (runtime, #105) and
  `CasService` (`fq-store`) are both `#[tarpc::service]` traits with a
  `bind`/`serve` split and a serializable `WireError`. The new `ControlService`
  is `ReadService` grown a command surface — not a new stack.
- **The reader already exists.** `Views` (`fq-runtime/src/views.rs`) backs
  `ReadService`; the CLI's reads mostly route through it already (`open_views`).
- **Streaming already has a cursor form.** `ReadService::transcript_since`
  (index cursor) is how the dashboard does live transcript. `--follow` and
  `events tail` become polling loops over that shape — so `fq` needs no NATS.
- **The operator writes already have a library API.** `control_plane::operator`
  (`drop_invocation`, …) and the drain/reload/down control paths are functions
  today; they become `ControlService` handlers, unchanged underneath.

What is genuinely new: a sqlx-free client crate, and the TLS + shared-secret
transport (the tree has **no** server TLS today — both services are plaintext
bincode over loopback, non-loopback refused).

## Target shape

```
  fq  ──ControlService (tarpc, TLS + shared secret)──▶  fqd
 (client crate only:                                   (fq-runtime: stores + NATS,
  wire types + client,                                  ControlService handlers,
  no sqlx, no NATS)                                     auth middleware)
```

`fq` is a pure client. `fqd` is the sole holder of the SQLite stores and the
sole speaker to NATS. The interface is the only edge between them, and the only
edge to authenticate.

## The two argument surfaces (decided 2026-08-23)

ADR-0031 settles the architecture and says nothing about the command
lines. They are decided here, because the split is only real to an
operator at the point where the two binaries stop looking alike.

### `fqd` — the daemon

No subcommands. It starts, it runs, it drains on signal. Everything it
takes is about where its state lives and what it talks to.

```
fqd [--config fqd.toml] [--agents-dir DIR] [--nats-url URL]
    [--cache-dir DIR] [--state-dir DIR] [--log-format text|json]
fqd --version
```

### `fq` — the operator CLI

```
fq [--config fq.toml] [--addr HOST:PORT] [--log-format text|json] <command>
```

Commands are what they are today minus `run`: `init`, `connect`,
`token`, `status`, `doctor`, `events`, `costs`, `invocation`, `workers`,
`agent`, `dead-letters`, `trigger`, `down`, `ops`, `version`.

**`--nats-url`, `--cache-dir` and `--state-dir` leave the client.** A
thin `fq` has no broker to reach, no store to open and no identity to
persist; each of those flags today describes something it will no longer
own. `--agents-dir` goes too — `agent list` answers over the edge, and
`agent validate` takes a path argument, so neither needs a configured
directory.

### The config files split by owner

`fq` reads **`fq.toml`**; `fqd` reads **`fqd.toml`**. One file per
binary, named for it.

The alternative was for `fq` to keep reading the daemon's config to find
`[edge] bind`. That is the coupling the split exists to remove, and it
does not survive contact with a remote daemon: an operator on another
machine has no `fqd.toml` to read, so a client that needs one cannot
reach a daemon it does not co-habit with.

Credentials stay out of both. `connections.toml` remains what `fq
connect` writes — address, fingerprint, token, chmod 600 — because a
secret rotates on a different schedule from a setting, and mixing them
means either the settings file inherits 600 or the secrets stop being
private. So: `fqd.toml` is the daemon's configuration, `fq.toml` is the
client's preferences, `connections.toml` is the client's credentials.

**Open sub-question:** what `fq.toml` actually holds on day one. If the
pairing store answers "which daemon, with what token", the client may
have nothing else to configure yet, and the file could start empty or
absent. Decide when writing Phase 3 rather than inventing settings to
justify a file.

### `fq run` is removed, not deprecated

Not a shim. `fq` genuinely cannot start a daemon once it links neither
the runtime nor a store, so a `fq run` that survived would be a command
that lies about what the binary is. This is alpha; the break is cheap
now and gets more expensive every week it is deferred.

Callers to update in the same change: `ops/dogfood/run.sh`
(`exec ./current/fq run` → `exec ./current/fqd`), the justfile's dev-run
recipe, `install.sh`, and the ADR-0022 release matrix.

### Migration this forces on the live stack

The dogfood host runs a daemon configured by `fq.toml` **today**, and
that file was updated on 2026-08-23 to carry the `[edge] bind = 9470`
the dashboard needs. Renaming it is a live migration, not a rename in
the repo: `$DOGFOOD/fq.toml` → `$DOGFOOD/fqd.toml`, plus `deploy.sh`
(which asserts the file exists) and `ops/dogfood/README.md`. Sequence it
with the deploy that ships the two binaries, so the host never has a
config whose name does not match the binary reading it.

## Phased plan (PR-sized; net first, then inward-out, auth last)

Not strictly riskiest-first: the behaviour-preservation **net** goes first
because the whole migration must be provably output-preserving, and **auth**
goes last because it slots in beneath a finished interface without touching
handlers.

### Phase 0 — golden-master CLI harness (the net)

**DONE.** The harness is `fq-cli/tests/golden*.rs` with snapshots under
`tests/golden/`, driving the built binary in a hermetic env.

The regression net for every later phase. Generalises #261's Phase 0 to the
**whole** CLI surface (reads *and* commands): seed a deterministic fixture
(`events.db` + a scripted NATS or a fake bus), drive the built binary
(`CARGO_BIN_EXE_fq`, hermetic env — `smoke.rs` is the template), and snapshot
stdout for every command in human and `--format json`. Determinism (fixed
UUIDs/timestamps, normalised durations) is the real work. Land green before
touching a handler. **Acceptance:** snapshots cover the surface; `just ci` green.

### Phase 1 — extract the sqlx-free client crate (rescopes #264; #261 is the on-ramp)

**DONE (#491), in `fq-ops` rather than a new crate.** ADR-0031 Appendix A
had already named `fq-ops` as the wire crate, and it carried a
dependency gate before the types arrived. The `*View` types,
`TranscriptEntry`, the turn atom, the roster shapes and the declared
contract shapes all moved there; `fq-runtime` re-exports them, so no
call site moved. The dashboard proved the shape by dropping
`fq-runtime` entirely — `cargo tree -p fq-dashboard -i sqlx` finds no
such package, held by `thin_reader_gate`.

New leaf crate (e.g. `fq-control-api`) holding the `*View` types,
`TranscriptEntry`, the service trait, and the generated client — **no `sqlx`**.
`fq-runtime` and `fq-cli` both depend on it. Finish routing CLI reads through the
reader (the #261 work) so no read handler holds a raw store. Still in-process /
loopback. **Acceptance:** golden-master byte-identical; `fq-cli` reads only via
the reader; the new crate has no SQL dep.

### Phase 2 — `ReadService` → `ControlService`

**DONE, by a different route (Phase 4 cohorts, #489).** The command and
read surfaces became *registry operations* on the authenticated **edge**
rather than trait methods on a `ControlService`, which is what ADR-0006
and ADR-0031's own Decision section describe. `ReadService` was retired
outright rather than grown into. Streaming is `next_batch(from_seq,
max_wait)` on the edge, as the Decision says.

Add the command RPCs — `reload`, `drain`, `down`, `trigger`, `invocation drop`,
`workers prune`, dead-letter `requeue` — with `fqd` handlers fanning out to NATS
and the stores internally. Re-express `events tail` and `transcript --follow` as
cursor-polling over `transcript_since`/`events`. Point the CLI's command
handlers at the RPCs. Still loopback. **Acceptance:** golden-master
byte-identical (including the tail/follow commands driven against a fixture);
`fq-cli` no longer calls `operator::*` or publishes to NATS directly.

### Phase 3 — split the binary — **the only phase left**

**This is bigger than "`fq run`'s guts move here", which is how both this
plan and #254 described it.** `fq-cli` is three things today, not two:
the client verbs, the operator-surface **handlers the daemon executes**,
and `run_daemon`. The handlers are why `fq-cli` still calls
`EventBus::connect` with the migration gate at zero — they are daemon
code that happens to live in the client's crate. Roughly half of
`fq-cli`'s ~9,000 lines go to `fqd`:

| Moves to `fqd` | Lines | Stays in `fq` | Lines |
|---|---|---|---|
| `lib.rs` (`run_daemon`) | ~890 | `cli.rs` (arg parsing) | 576 |
| `event_atom.rs` | 918 | `events.rs` (rendering) | 458 |
| `trigger_command.rs` | 759 | `status.rs` (rendering) | 370 |
| `operator_surface.rs` (the registry) | 740 | `connections.rs` (pairing) | 345 |
| `dead_letter_atom.rs` | 583 | `edge_call.rs`, `edge_identity.rs` | ~300 |
| `dead_letter_requeue.rs` | 441 | the remaining renderers | ~950 |
| `recovery.rs`, `resume.rs` | ~580 | | |

The report builders (`doctor_report.rs`, `cost_report.rs`,
`status_report.rs`) are pure over views but are what the *daemon* serves,
so they go with the handlers. Sequence the move so each step compiles:
handlers out first (they are the bulk and have no client callers), then
`run_daemon`, then delete the client's dependency.

Also in this phase: the two argument surfaces above, the config-file
split (`fq.toml` / `fqd.toml`), the removal of `fq run`, and the live
migration of `$DOGFOOD/fq.toml`.

**Acceptance:** `fq`'s `Cargo.toml` names no `fq-runtime`, `sqlx` or
`async-nats` — a build fact, held by a gate in the shape of
`fq-dashboard`'s `thin_reader_gate`, which already proves a reader can
live without them. `cargo tree -p fq-cli -i sqlx` finds no such package.
Golden-master green against a running `fqd`. `just smoke` green — it
drives the real binaries and is now the end-to-end check it was not
before.

### Phase 4 — auth: TLS + shared secret (unlocks remote)

**DONE, superseded in mechanism by Appendix A.** The edge terminates TLS
with a self-signed certificate the client pins by fingerprint, and
authorises with **biscuit capability tokens** rather than a shared
secret — attenuable offline, which is what lets the dashboard hold a
six-grant read-only token instead of admin authority. Auto-provisioned
on first run; the admin token is printed once.

Add a transport/middleware layer beneath the RPC contract: `fqd` terminates TLS
with a self-signed cert it **auto-provisions on first run**, and requires a
shared secret it also mints; `fq` pins the cert (TOFU or configured fingerprint)
and presents the secret. Gate the non-loopback bind on auth being configured.
The `ControlService` trait and handlers are untouched. **Acceptance:** a remote
`fq` authenticates to `fqd`; a wrong/absent secret is refused; loopback default
preserved for the local case.

## Decisions settled (ADR-0031 + discussion)

- **Shared secret over TLS**, not UDS (target is remote) and not mTLS (heavier
  than single-tenant needs) — mTLS is a later, non-breaking swap behind the
  middleware seam.
- **Auth is transport-level middleware**, below the RPC trait, so the mechanism
  is swappable without interface churn.
- **The daemon is required**; no local-store fallback in `fq` (it would re-link
  `sqlx`).
- **`fq` speaks only `ControlService`**; all NATS/store access lives in `fqd`;
  tails are cursor-polling, not NATS subscriptions.

## Deferred / open questions

- Secret bootstrap & rotation UX; cert-trust model (TOFU vs. fingerprint).
- Whether `fqd` fronts the CAS services under the same auth layer (likely unify).
- The rest of the **M5 posture**: NATS auth (`fqd↔NATS`, `worker↔NATS`) — a
  separate ADR/plan; until then this hardens one edge only.
- Multi-operator → mTLS.

## Interlocks

- **#261** (route CLI reads through `Views`) — the read-centralisation on-ramp,
  folded into Phase 1.
- **#264** (SQL-free CLI) — realised by Phases 1 + 3.
- **ADR-0022** — two-binary distribution (release matrix, `install.sh`).
- **`fq-dashboard`** — also a `ReadService` client; benefits from (and should
  adopt) the same transport-auth layer in Phase 4.
- **M5 / NATS auth** — the coherent posture this edge is one part of.
