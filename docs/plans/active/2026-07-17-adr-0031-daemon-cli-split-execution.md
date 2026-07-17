# Daemon / CLI split — execution plan for `fqd` + `fq` (ADR-0031)

Turning [ADR-0031](../../adrs/draft/0031-daemon-cli-split.md) into PR-sized
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

## Phased plan (PR-sized; net first, then inward-out, auth last)

Not strictly riskiest-first: the behaviour-preservation **net** goes first
because the whole migration must be provably output-preserving, and **auth**
goes last because it slots in beneath a finished interface without touching
handlers.

### Phase 0 — golden-master CLI harness (the net)

The regression net for every later phase. Generalises #261's Phase 0 to the
**whole** CLI surface (reads *and* commands): seed a deterministic fixture
(`events.db` + a scripted NATS or a fake bus), drive the built binary
(`CARGO_BIN_EXE_fq`, hermetic env — `smoke.rs` is the template), and snapshot
stdout for every command in human and `--format json`. Determinism (fixed
UUIDs/timestamps, normalised durations) is the real work. Land green before
touching a handler. **Acceptance:** snapshots cover the surface; `just ci` green.

### Phase 1 — extract the sqlx-free client crate (rescopes #264; #261 is the on-ramp)

New leaf crate (e.g. `fq-control-api`) holding the `*View` types,
`TranscriptEntry`, the service trait, and the generated client — **no `sqlx`**.
`fq-runtime` and `fq-cli` both depend on it. Finish routing CLI reads through the
reader (the #261 work) so no read handler holds a raw store. Still in-process /
loopback. **Acceptance:** golden-master byte-identical; `fq-cli` reads only via
the reader; the new crate has no SQL dep.

### Phase 2 — `ReadService` → `ControlService`

Add the command RPCs — `reload`, `drain`, `down`, `trigger`, `invocation drop`,
`workers prune`, dead-letter `requeue` — with `fqd` handlers fanning out to NATS
and the stores internally. Re-express `events tail` and `transcript --follow` as
cursor-polling over `transcript_since`/`events`. Point the CLI's command
handlers at the RPCs. Still loopback. **Acceptance:** golden-master
byte-identical (including the tail/follow commands driven against a fixture);
`fq-cli` no longer calls `operator::*` or publishes to NATS directly.

### Phase 3 — split the binary

Introduce `fqd` (the daemon; `fq run`'s guts move here) and reduce `fq` to the
client. `fq` drops its `fq-runtime`, `sqlx`, and `async-nats` dependencies. The
daemon becomes **required** for the CLI (no local fallback). Update
distribution: two binaries in the ADR-0022 release matrix + `install.sh`.
**Acceptance:** `fq`'s `Cargo.toml` has no SQL/NATS dep (the grep gate from #264
becomes a build fact); golden-master still green against a running `fqd`.

### Phase 4 — auth: TLS + shared secret (unlocks remote)

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
