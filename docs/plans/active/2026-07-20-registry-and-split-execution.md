# Registry-first API + daemon/CLI split — joint execution plan (ADR-0006 + ADR-0031)

**Status:** accepted (2026-07-20, via #335). Supersedes the
[2026-07-17 ADR-0031 execution plan](2026-07-17-adr-0031-daemon-cli-split-execution.md):
ADR-0006's Appendix A replaced that plan's central artifact — the
hand-enumerated `ControlService` trait — with the derived tarpc binding of the
operation registry, and declared that the sqlx-free wire crate ADR-0031 calls
for **is** `fq-ops`. This plan carries forward everything from the old plan
that survives that amendment (the golden-master net, the split-last ordering,
the settled auth decisions) and re-sequences the middle around the registry.

Sources: [ADR-0006](../../adrs/accepted/0006-registry-first-api.md) ·
[ADR-0031](../../adrs/accepted/0031-daemon-cli-split.md) ·
[interface inventory](../../reviews/2026-07-20-interface-inventory.md) ·
code survey of `docs/adr-0006-rewrite` (2026-07-20, summarised below).

## Where the code actually is (surveyed 2026-07-20)

Neither ADR has begun in code: there is **no `fqd` binary, no `fq-ops`, no
registry, no `ControlService`, and no server-side TLS or shared-secret
anywhere in the tree** (repo-wide grep, zero hits). What exists is precisely
the substrate the ADRs bet on:

- **Landed.** `Views` (16 read methods, opens all three stores read-only);
  CLI reads as formatters over `open_views` (#105/#261); the in-daemon
  `ReadService` (15 RPCs, loopback-only, plaintext bincode, non-loopback
  refused); `fq-dashboard` as its only client; the golden-master harness
  (`fq-cli/tests/golden.rs`) — **reads only**; the `store_open_gate.rs`
  source gate with 8 sanctioned `allow-direct-store-open` sites; the operator
  write library (`control_plane/operator.rs`: `drop_invocation`,
  `list_dead_letters`, `requeue_dead_letter`) already lifted out of the CLI.
- **Not started.** Old-plan Phases 1–4 in their entirety; the commands half
  of Phase 0. `fq-cli` still links `fq-runtime`, `async-nats`, and
  (transitively) `sqlx`; `events tail` / `transcript --follow` are still raw
  NATS subscriptions.
- **The three boundary-bypassing paths** (all verified live): `fq trigger`
  defaults to in-process execution of the whole runtime; `fq workers prune`
  deletes control-plane rows CLI-side with no event; `fq agent list` reads
  the local filesystem, not the daemon's registry.
- **Auth reality.** The only in-process auth primitive is fq-store's biscuit
  tokens + `Verb { Read, Write, Delete, List, Grant }` × scope grant model —
  shipped but wired to *no* transport. The dashboard's "auth front" is a
  deployment-layer Caddyfile, not code. `OpMeta.permission` binds to the
  fq-store vocabulary; nothing needs inventing.

## Target shape

```
 fq (thin client: fq-ops types + generated wrappers, no sqlx/NATS)
  │
  │  tarpc: invoke(name, input) / next_batch(from_seq, max_wait)
  │  TLS (pinned self-signed cert) + shared secret — the one audited edge
  ▼
 fqd (fq-runtime: stores + NATS + registry handlers + auth middleware)
  ├─ fq-dashboard ──▶ same edge, generated client
  └─ MCP face (#84) ─▶ same registry, capability-filtered
```

## Sequencing logic

1. **Net first** (unchanged from the old plan): every later phase must be
   provably output-preserving, so the golden net closes over commands before
   any handler moves.
2. **Contract before wire.** `fq-ops` lands as a pure crate with property
   tests before anything serves it; pattern arguments happen in review of a
   leaf crate, not across a live edge.
3. **The edge is born authenticated** — inverting the old plan's auth-last
   ordering, per ADR-0006 Phase 1 ("wired through the generic edge behind
   0031 auth"). Auth-last made sense when *migrating* an existing interface
   in place; the generic edge is *new*, so there is no compatibility burden,
   no later wire flag-day, and every subsequent phase exercises the real
   security posture.
4. **Exemplars fix the pattern; the fleet copies it.** One op per kind gets
   full human scrutiny (and is where the generic-invoke-ergonomics fallback
   is judged); the remaining ~17 are near-identical, tightly-specced PRs.
5. **Split last-but-one.** The thin client is only possible after the last
   CLI verb speaks the edge; distribution churn (ADR-0022, install.sh,
   dogfood deploy) happens once, after the surface is stable.

## Phased plan (PR-sized slices)

### Phase 0 — close the net: golden-master over commands

Extend the golden harness to the write/control verbs it does not cover:
`reload`, `down [--now]`, `trigger --via-nats`, `invocation drop`,
`dead-letters list/requeue`, `workers prune [--dry-run]` — human and
`--json` forms. `daemon_shutdown.rs` (spawns `fq run` + private broker via
`fq-test-support`) is the template for the daemon-backed ones. Do **not**
snapshot in-process `fq trigger` output as contract — that mode is scheduled
to retire (D-1 below); snapshot the `--via-nats` form. Determinism (fixed
IDs, normalised timing) is the real work, as it was for reads.
**Acceptance:** every CLI verb has golden coverage in both formats;
`just ci` green. *(1–2 PRs.)*

### Phase 1 — `fq-ops`: the contract crate

New leaf crate `services/fq-runtime/crates/fq-ops` (also the ADR-0031 wire
crate; sqlx-free as a build fact, like #264's gate). Contents: `OpKind`,
the `Operation` trait, `OpMeta` (permission = fq-store's `Verb` × scope
vocabulary; audit class; stability; caveat text), `Receipt`, the registry
(duplicate-name rejection, P8 name-grammar validation at registration),
`WireError`, the generic `invoke`/`next_batch` envelope types, and
`name@version` schema-versioning rules with schemars integration.
**Tests set the bar for the whole migration:** property tests for the name
grammar (P8: `<domain>.<imperative>` ⇒ Command, `<domain>.<noun>` ⇒ Query,
`.tail` ⇒ Stream — an op whose name misparses must fail registration),
registry invariants, and schema snapshots as the compatibility oracle.
**Acceptance:** crate builds with no sqlx/NATS in its tree; contract tests
green; zero behaviour change anywhere else. *(1–2 PRs.)*

### Phase 2 — the authenticated generic edge

The tarpc `invoke`/`next_batch` service hosted by the daemon (still
`fq run` at this point), alongside the untouched `ReadService`:

- **2a — transport + secret.** `fqd` auto-provisions a self-signed cert and
  mints a shared secret on first run (no operator crypto toil); TLS
  termination; bearer-secret check as middleware *beneath* the RPC contract
  (the mTLS-later seam). Non-loopback bind allowed only with auth
  configured; loopback remains the default.
- **2b — client + pinning.** Client-side connect with TOFU pin persisted to
  config, overridable by explicit fingerprint; wrong/absent secret refused
  with a distinct, tested error.
- **2c — first op: `registry.describe`.** The registry describing itself
  (names, kinds, schemas, meta) — proves the edge end-to-end without
  touching domain logic, and every later phase's tooling (docs, codegen,
  drift checks) wants it anyway.

**Acceptance:** a non-loopback client authenticates against a test daemon;
negative-auth tests pass; golden untouched. *(2–3 PRs.)*

### Phase 3 — exemplars: one op per kind

The pattern-fixing slices, with full review scrutiny:

- **3a — watermark plumbing.** The projection tracks its applied event-log
  sequence (shared with #139); queries gain bounded `min_seq` waiting.
  Prerequisite for D4 composition.
- **3b — Query: `invocation.show`.** Transplant the `Views::invocation`
  body; typed client wrapper; CLI verb flips behind golden.
- **3c — Command: `invocation.drop`.** Transplant
  `operator::drop_invocation`; returns a `Receipt` (subject, stream,
  sequence), never state; envelope carries operator identity.
  Integration test: `drop` → `show(min_seq = receipt.seq)` composes
  read-your-writes through the public surface alone.
- **3d — Stream: `invocation.transcript.tail`.** Ephemeral JetStream
  consumer from `from_seq`; every item carries its sequence; tarpc binding
  is long-poll `next_batch`. `--follow` flips to it behind golden.
- **3e — wrapper codegen.** Decide and land the generation mechanism for
  typed client wrappers (D-3 below). This phase is also the checkpoint for
  ADR-0006's held fallback: if generic-invoke ergonomics disappoint,
  macro-expanding to a per-method trait is the named alternative — decide
  here, not after seventeen migrations.

**Acceptance:** three ops live end-to-end through the authenticated edge;
flipped verbs golden-identical; read-your-writes test green. *(3–5 PRs.)*

### Phase 4 — fleet migration (~17 near-identical PRs)

Migrate the remainder per the inventory's disposition table, one op (or one
tight cohort) per PR, each specced from the exemplar template: define op →
transplant handler → flip CLI verb behind golden → delete the old path.
This is deliberately fleet-fodder — each PR is tightly specced, oracle-backed,
and independent. Order:

1. **Queries** (lowest risk, highest count): `invocation.list`,
   `worker.list/show`, `event.query`, `cost.summary/by_agent`,
   `agent.list/show`, `deadletter.list`, `runtime.version`,
   `runtime.doctor`, and Probes `runtime.health`/`runtime.status`
   (JetStream introspection moves daemon-side — it cannot live in a thin
   client). Per-method call on `failures`/`recovery`/`executions`/
   `event_count`: op vs. internal to the doctor/status composites.
2. **Commands:** `trigger.publish`, `deadletter.requeue` (non-idempotency
   → `OpMeta` caveat), `control.reload`/`control.down` (become in-process
   handlers; the NATS control-subject hop retires from the operator path —
   `daemon_shutdown.rs` re-targets the RPC), and the three bypass-path
   resolutions: `worker.prune` becomes an evented command (co-emitted
   events, audit for free), `agent.list` answers from the daemon's
   `SharedRegistry`, in-process trigger retires (D-1).
3. **Streams:** `event.tail` (upgrades from silent-drop core-NATS subscribe
   to sequence-resumable), transcript follow already done in 3d.
4. **Deletions:** dashboard re-points to the generated client (its in-crate
   tests pin the old wire and re-point with it); then delete `ReadService`,
   its forwarding layer, and per-command JSON plumbing (#190 dissolves;
   #183 becomes a permission declaration; #261/#264 complete as side
   effects).

**Acceptance per PR:** golden-identical (or a reviewed, justified golden
update); the retired path deleted in the same PR — no parallel-path
lingering. **Phase acceptance:** `fq-cli` calls no `operator::*`, opens no
store, publishes nothing to NATS; `ReadService` is gone.

### Phase 5 — split the binary

Introduce `fqd` (the guts of `fq run` move); reduce `fq` to client crate +
renderers; drop its `fq-runtime`/`async-nats`/`sqlx` dependencies (build
fact, greppable in `Cargo.toml`). `fq init` and `fq agent validate` stay
local (pure functions on local files). The daemon becomes required — no
local-store fallback. Distribution follows in the same phase: ADR-0022
release matrix and `install.sh` go two-binary; `scripts/package.sh` specs,
`justfile` release/package-main recipes, and the dogfood deploy scripts
(`ops/dogfood/*.sh`) update together; dogfood host upgrades via the drain
SOP. `smoke.rs`/`sigpipe.rs`/`daemon_shutdown.rs` re-target `fqd`.
**Acceptance:** golden green against a running `fqd`; release bundle ships
both binaries; the dogfood loop runs a split deployment. *(2–3 PRs.)*

### Phase 6 — MCP operator face (#84)

One op per tool through the rmcp adapter, capability-filtered on
`OpMeta.permission`; denied ops are invisible, not erroring. The dogfood
loop (Claude operating the ops instance) is the natural first consumer.
**Acceptance:** an MCP client lists and invokes exactly the permitted set;
audit events carry the MCP caller identity. *(1–2 PRs.)*

### Phase 7 — horizon (sequenced separately; listed so nothing is orphaned)

- **fq-store registry instance** on the same `fq-ops` crate
  (`cas.* / object.* / grant.* / token.*`); transport unification under
  `fqd`'s edge sequenced with **M5** (NATS auth posture — its own plan).
- **ADR-0016 convergence:** agent-facing built-ins become
  capability-filtered registry ops behind the MCP adapter.
- **Traversal ops** (`traversal.run/.status/.tail`) land registry-native
  when the graph executor arrives — the original reason ADR-0006 precedes
  it.
- **REST + SSE** activation on first external consumer; **mTLS** swap when
  multi-operator arrives (both non-breaking behind existing seams).

## Test surfaces that gate the work

- `fq-cli/tests/golden.rs` — the oracle; Phase 0 completes it, every flip
  cites it.
- `fq-cli/tests/store_open_gate.rs` — the 8 `allow-direct-store-open` sites
  shrink as access moves daemon-side; the gate's allowlist is updated *down*
  each time, never up.
- `fq-cli/tests/daemon_shutdown.rs` — pins the NATS control-subject paths;
  re-targets in Phase 4.2.
- `fq-dashboard` in-crate tests — pin the `ReadService` wire; re-point in
  Phase 4.4.
- `fq-test-support` — the private-broker harness all daemon-backed tests
  ride; unchanged but on every critical path.

## Decision points ahead (gates, with leanings)

- **D-1 (gates Phase 4.2): in-process `fq trigger`.** Retire it (leaning),
  or keep behind a dev-only `fqd` pathway. The dev loop already has
  `just up`; a second execution path in the CLI contradicts the thin
  client.
- **D-2 (gates Phase 4.2): NATS binding for `control.*`.** Retire from the
  operator path entirely (leaning, per D3/D8); `fq.control.*` remains at
  most internal daemon↔worker fan-out.
- **D-3 (gates Phase 3e): wrapper codegen mechanism.** Generator consuming
  `registry.describe` output vs. build.rs vs. macro. Leaning: a small
  generator binary checked against `registry.describe` in CI (no proc-macro
  layer; keeps the schema the single source).
- **D-4 (Phase 2 detail): secret bootstrap/rotation UX and TOFU-vs-
  fingerprint default.** Leaning: print secret + fingerprint on first
  `fqd` run, store client-side in config; rotation becomes a
  `control.secret.rotate` op later.
- **D-5 (Phase 4.1 detail):** which of `failures`/`recovery`/`executions`/
  `event_count` are ops vs. internals of `runtime.doctor`. Leaning:
  internal until a consumer outside doctor/status exists (P11: curate).

## Interlocks

- **#261 / #264** — absorbed: read-centralisation is done; sqlx-free is
  realised by Phases 1+5 as build facts.
- **#139** — projection applied-seq tracking shared with Phase 3a.
- **#190 / #183 / #84** — dissolved structurally / becomes metadata /
  becomes Phase 6.
- **ADR-0022** — two-binary distribution, Phase 5.
- **ADR-0026/0027** — the log remains the system of record (commands append,
  receipts reference); dogfood deploys keep the drain SOP through Phase 5.
- **github-watcher + fq-cron** — the `fq.trigger.<agent>` wire contract is
  a compatibility boundary (D8 carve-out); unchanged through Phase 5; the
  watcher's outcome subscription migrates to a stream op when convenient.
- **Graph executor plan**
  ([2026-07-07](2026-07-07-graph-executor-two-node-vertical.md)) —
  traversal ops are born derived (Phase 7).
- **M5** — this plan hardens the operator edge only; `fqd↔NATS` and
  `worker↔NATS` stay named as M5 scope.
