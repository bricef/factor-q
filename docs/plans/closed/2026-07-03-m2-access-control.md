# M2 — access control: implementation plan

**Status:** Closed (2026-07-05). All seven slices shipped and merged to `main`
in `627a5ec` (branch `m2-access-control`, no-ff). The Slices table below records what
landed; "Decisions taken while building" and the pre-merge review are the
durable record. The gate's first production caller — token-gated remote
exposure of the named service — is M5's charter.

The **"what"** and **"why"** live elsewhere — the resolved design in
[ADR-0023](../../adrs/accepted/0023-storage-and-vector-foundation.md) (F4:
event-sourced grant claims → permission projection, biscuit capability tokens as
the wire mechanism) and the milestone in the
[storage + vector foundation plan](../active/2026-06-27-storage-vector-foundation.md).
This doc is the **"how, in what order"**: the slices, the decisions taken up
front, and where each is verified.

**Verification leads** (the M1c pattern, minus TLA⁺ — the subtlety here is
event ordering and Datalog semantics, which property tests cover). The
**authorization oracle** (slice 1) states the security claims as executable
properties and gates every later slice; each slice is green on the fq-store
`just ci` gate (fmt, clippy, doc, test) before commit.

## The claims (the oracle checks these)

- **A1 — default-deny:** absent a grant, every cross-agent `can()` is false; an
  agent's own `system.agents.<id>.*` scope needs no grant.
- **A2 — attenuation never widens:** an attenuated token authorizes a subset of
  its parent, always.
- **A3 — revocation wins:** after a revoke event is applied, no operation under
  the revoked grant succeeds. In-process the gate checks the live projection on
  every op (belt-and-braces), so the revocation gap is **zero**; the token TTL
  bounds only future *remote* verifiers (M5), where new mints already fail and
  extant tokens die with their TTL.
- **A4 — delegation is grant-gated:** only a principal holding `grant` on a
  scope can delegate within it, and only attenuated (⊆ what the delegator
  holds — verbs and scope).
- **A5 — projection ≡ replay:** the projection equals a fresh replay of the
  local event log, after any event sequence and at any crash point (the outbox
  is a `published` flag on the same log rows, not a separate source — see the
  slice-2 decision below).
- **A6 — availability:** data-plane operations never touch the bus; with the
  publisher down, reads/writes/authorization proceed and grant writes still
  commit locally (outbox), draining when the bus returns.

## Slices

| # | Slice | Validates | Status |
|---|---|---|---|
| 1 | Verification harness — the authorization oracle + property/DST scaffold | the net itself | done |
| 2 | Grant events + the event-log seam + durable outbox | A5, A6 | done |
| 3 | The grant projection (SQLite #2): apply, rebuild, idempotency | A3, A5 | done |
| 4 | Biscuit tokens: mint / verify / attenuate; key config | A1, A2, TTL | done |
| 5 | The op-boundary gate — `can()` at the named layer | A1, A3, A4 end-to-end | done |
| 6 | CLI UX (`grant` / `token`) + user + operator docs | — | done |
| 7 | Fault DST — bus outage, crash-replay, revocation races + soak | A5, A6, recovery | done |

## Decisions taken up front

- **The event log sits behind a seam.** *(Refined in slice 2 — see below: the
  local SQLite log is authoritative and the seam is the publish-only `GrantBus`,
  not an append+replay trait.)* An in-memory bus implementation for tests and a
  NATS/JetStream one for real — the `ContentStore`/`NameIndex` conformance
  pattern. fq-store stays hermetically testable (`just ci`); the NATS-backed
  integration test runs against the local infra via `just test-bus`.
- **Publisher failure never affects store availability.** Two rules. The data
  plane (read/write/authorize) makes **no bus calls** — verification is local
  (token + projection). Grant *writes* append to a **durable local outbox** in
  the same transaction that updates the projection, and an async drain publishes
  to NATS with retry; a bus outage delays publication, never grant acceptance.
  The rebuild contract is therefore a fresh **replay of the local log** (the
  `published` flag is metadata on those rows, not a second source), A5, and the
  oracle checks it under outage + crash injection.
- **The gate sits at the Repository/service level, not the CAS.** A gating
  layer wraps the named operations and evaluates `can(Principal, Op, Resource)`
  before delegating; `ContentStore` (and the raw `Repository` beneath the gate)
  remain **preserved internal APIs** — same layering lesson as M1c's
  `BlockStore` split. Grants are scoped by exact **name** or by **namespace**
  subtree (`research.papers.*`; `system.agents.<id>.files.*` for harness-only
  scopes) — segment-aware, not raw prefixes.
- **Keys are config: CLI args + env vars.** `--biscuit-private-key` /
  `FQ_BISCUIT_PRIVATE_KEY` (mint path only) and `--biscuit-public-key` /
  `FQ_BISCUIT_PUBLIC_KEY` (verify path — public key only, per F4). No key
  generation/rotation machinery in v1 beyond a `key generate` helper; a
  coherent, service-wide config story is a **separate discussion**, deliberately
  not settled here.
- **No TLA⁺; the strong-verification posture stays.** The oracle (A1–A6) +
  `proptest` sequences over grant/delegate/attenuate/revoke/mint/verify, and a
  seeded DST in the M1c style with fault injection (bus outage, crash mid-grant,
  replay) and a soak run. Biscuit's Datalog gives A1/A2 mostly by construction;
  the tests prove *our wiring* of it.
- **Token shape:** biscuit (`biscuit-auth`), short TTL by default (the
  revocation bound), carrying principal + verb set + resource-prefix facts;
  attenuation is the only way to derive a narrower token offline.
- **Event naming** follows the established conventions (`factor-q/granted@1`
  -style schema ids, `fq.*` subjects), settled concretely in slice 2 alongside
  the schemas.

## Decisions taken while building

- **(Slice 1) One `Granted` event; "delegated" = an agent grantor.** The domain
  event is `Granted { grantor: Operator | Agent, … } | Revoked { id }`; the
  wire layer (slice 2) may still emit distinct `granted`/`delegated` schema ids,
  but the semantics live in one variant. `Grantor::Operator` is the bootstrap
  root authority (the local store owner via CLI/service); `Principal` itself
  stays agent-only per ADR-0023.
- **(Slice 1) Liveness is evaluated at query time, chains ordered by id.** A
  delegation is live only while an **earlier, still-live** grant backs it
  (grantor holds `Grant`, covering scope, superset verbs) — so upstream
  revocation kills the delegated subtree transitively (A3), and chains are
  well-founded (no cycles). Attenuation-violating delegations are inert.
- **(Slice 1) The log tolerates garbage; garbage confers nothing.** `apply` is
  total and deterministic (duplicate grant id: first wins; revocation wins
  regardless of event order, even revoke-before-grant). The API gate (slice 5)
  rejects invalid requests up front, but no safety property depends on that.
- **(Slice 1) Own scope needs no grant:** `system.agents.<id>` and below is the
  principal's own subtree (A1's carve-out).

- **(Slice 2) The local log is authoritative; the bus is a feed.** A deliberate
  refinement of ADR-0023's dataflow: `SqliteGrantLog` (SQLite #2 per ADR-0024)
  is the durable, ordered source of truth — appends assign `GrantId`s and
  rebuild replays it — while NATS carries `WireGrantEvent` envelopes outward
  for external consumers. Nothing in the store ever *reads* the bus, so A6
  holds by construction; `drain` is the outbox pump (publish in log order,
  mark on ack, resume after failure exactly where it stopped).
- **(Slice 2) `seq` = `GrantId`.** One AUTOINCREMENT sequence is both the log
  position and a granted event's id (monotone, never reused) — delegation
  chains order by it, revocations reference it. Events are stored relationally
  (no JSON blob), so replay decodes without a parser and malformed rows surface
  as `Corrupt`.
- **(Slice 2) Wire naming settled:** schema ids `factor-q/granted@1` /
  `delegated@1` (agent grantor) / `revoked@1`; subjects `fq.store.grant.<kind>`;
  stream `FQ_GRANTS`. The NATS impl lives behind the `bus` feature
  (`async-nats 0.38`, matching fq-runtime) so the crate stays hermetic without
  a broker; the integration test round-trips through real JetStream.

- **(For slice 5) Principal ids are runtime agent slugs — and the gate must
  re-validate them.** Agent identity is the runtime's `AgentId`: a validated
  NATS-subject token (non-empty, **no dots**/wildcards/whitespace), not a UUID
  (UUIDs identify events/invocations). The store treats the id as opaque, but
  the dot-free rule is load-bearing for the own-scope boundary: a dotted
  "agent id" like `alice.files` would compute an own-namespace *inside*
  another agent's `system.agents.alice.*` subtree. The slice-5 gate therefore
  enforces the same token validation at the store boundary (defense in depth —
  never trusting the caller's id shape), and `Principal` stays
  `#[non_exhaustive]` for any future differently-keyed principal type.

- **(Slice 3) The projection commits with the append.** It lives in the same
  grants DB and updates in the **same transaction** as every log append, so
  projection ≡ replay holds at every commit point (A5) — there is no window in
  which they can diverge. An `applied_seq` cursor + catch-up at `open` recovers
  a projection that lags the log (older binary, or wiped); `rebuild_projection`
  is wipe + replay with the log untouched.
- **(Slice 3) Liveness is cached, recomputed as one forward pass.** Each grant
  row carries a `live` flag re-derived on every event by a single pass in id
  order — exact, because liveness is a pure function of the final
  grant/revocation sets and a grant's chain depends only on strictly earlier
  grants. `projected_revocations` mirrors the model's revoked-id set, so
  revoke-before-grant behaves identically to the model.
- **(Slice 3) One semantics source.** `can` / `live_grants_for` decode rows
  into the domain types and evaluate with `Scope::covers` / verb sets — never
  parallel SQL string matching — and the differential proptest holds the
  projection equal to `GrantModel` over random sequences, before and after
  rebuild. `live_grants_for` is the feed token minting (slice 4) embeds.

- **(For slices 4–5) Belt-and-braces enforcement.** The in-process gate requires
  **both** a valid token (identity, attenuation, expiry) **and**
  `projection.can()` on every operation. The projection updates in the same
  transaction as the revocation event, so revocation is effective immediately
  in-process — no TTL-sized window. The token TTL (default 300 s, configurable)
  matters only for M5's distributed verifiers, where biscuit revocation-id
  distribution is the planned complement. Pure offline verification is a remote
  optimisation, never the in-process authority.

- **(Slice 4) Two token semantics, named and separate.** `authorizes()` is the
  offline/remote semantic (an embedded `right` must cover the operation —
  M5's path, TTL-bounded); `permits()` is the in-process gate semantic (TTL +
  the bearer's attenuation only, with the live projection as authority). The
  gate composes `verify → permits ∧ projection.can` — which is also how
  own-scope operations work with an unattenuated token.
- **(Slice 4) Token shape:** rights flattened one fact per verb
  (`right(verb, kind, value)`); segment-aware namespace matching expressed in
  the authorizer's policies; attenuation = appended conjunctive checks
  (scope / verb-set), so widening is structurally impossible (A2). Every
  caller-supplied string enters datalog via builder **parameters** — a hostile
  name or agent id cannot inject datalog.
- **(Slice 4) biscuit-auth 6.0, with explicit run limits.** Biscuit's default
  datalog `max_time` is 1 ms, which under load fails evaluations
  *nondeterministically* — and a timeout reads as deny. Found by the property
  suite as a flake; every authorizer now runs with explicit limits (100 ms
  wall clock, default fact/iteration caps). Keys are hex Ed25519
  (`generate_keypair` backs the slice-6 CLI helper); `DEFAULT_TOKEN_TTL` =
  300 s per the belt-and-braces decision.

- **(Slice 5) The gate is `GatedRepository`**, wrapping the named operations;
  every method takes the caller's token and runs `verify → id-shape check →
  permits ∧ projection.can`. Error taxonomy: `Token` (the credential is bad —
  including a dotted/forged principal id) vs `Denied` (valid credential, no
  authority). The raw `Repository`, CAS, and grant log stay reachable as
  documented **trusted accessors** for in-process callers (operator CLI,
  collector, audit).
- **(Slice 5) Delegation authority uses `covers_scope`, not `can()`.** The
  gate checks the delegator's live grants directly (`Grant` verb ∧ superset
  verbs ∧ scope-covering) — a `can()` check on the scope's root name would
  wrongly let a Name-scoped grant delegate the Namespace anchored at the same
  string. Delegation is also bounded by the token's own attenuation.
- **(Slice 5) Revocation rule v1:** the operator may revoke anything; an agent
  may revoke only grants **it issued**. (Upstream authorities already kill
  whole subtrees by revoking their own delegation — no supervisory revoke
  needed in v1.) Listing with an empty prefix is operator-only: no grantable
  scope can cover the root.

- **(Slice 6) The CLI is the operator surface only.** `key generate`,
  `grant add/ls/check/rm`, `token mint/attenuate/inspect` — root authority by
  possession of the store, no token required. Agent-issued *delegation* is a
  gate API affordance (`GatedRepository::grant`), deliberately not a CLI
  command. `grant check` prints allowed/denied and exits 0/1 (scriptable,
  mirroring `has`); `grant rm` refuses unknown ids loudly.
- **(Slice 6) Scope sugar:** a trailing `.*` is the namespace subtree
  (`research.papers.*` ⇒ `Namespace("research.papers")`); a bare dotted string
  is an exact `Name`. Bare `*`, empty, and `.*`-alone are rejected — nothing
  grants the root. The same sugar round-trips in `grant ls` output. Agent ids
  are validated in the CLI with the same rule as the gate.

- **(Slice 7) The grants DST drives the real stack against a lockstep model.**
  Seeded steps interleave operator grants, unchecked "delegations" (the log
  tolerates garbage; the projection must keep them inert), revocations of
  plausible-or-bogus ids, crash-reopens, bus outages, drains, mid-stream full
  rebuilds, and held tokens. Every step asserts projection ≡ model (A5), log
  replay ≡ the appended events, **outbox conservation** (published ∪ pending =
  everything, ordered — the bus survives crashes as an external broker), and
  the belt-and-braces composition for stale tokens ≡ the model (the
  zero-revocation-gap claim under races). CI runs 12 seeds × 40 steps; the
  deep soak (64 × 80 ≈ 5,120 steps) passed.
- **(Slice 7) Soak finding: drain over an empty outbox is `Ok(0)` even while
  the bus is down** — no publish is attempted, so nothing can fail. The deep
  soak caught the DST's original assertion being over-strict (seed 21 flipped
  the bus down after everything had drained); the semantics are now pinned in
  the harness. At-least-once fan-out on a crash between publish and mark is
  documented: consumers de-duplicate by `seq`.

**Status: all seven slices done.** The A1–A6 claims hold end-to-end — model,
log + outbox, projection, tokens, gate, CLI, and the fault DST + soak.

## Pre-merge review (2026-07-04)

A four-dimension subagent review (security, code quality, docs, consistency),
every candidate finding then independently verified against the code. Outcome
and fixes applied:

- **Security: clean.** No authorization bypass and no injection — datalog
  injection, SQL injection, the dot-free own-scope defense, attenuation-can-
  only-narrow (checked against biscuit-auth 6.0's `TrustedOrigins`), revoke
  DoS bounds, and delegation escalation were all verified sound.
- **Behavior fixes.** `revoke` now enforces the caller's token TTL and
  attenuation over the grant's scope like every other op (it previously
  checked only the signature + issuer — fail-safe, but it broke the "every
  operation" contract); `list` now requires a `List` grant covering the
  **namespace** (a point `Name` grant no longer enumerates a subtree); token
  key-parse errors no longer echo supplied key material. New tests pin each,
  with an injected clock making TTL testable end-to-end.
- **Refactors.** The delegation-support predicate, the verb↔string encoding,
  and the scope/wire-kind encodings are each defined once and shared (model,
  projection, gate, CLI); the reference model's liveness became a forward pass
  (no more exponential worst case), matching the projection's own algorithm.
- **Docs.** Stale "until M2" server strings, the "soak remaining" status, the
  NATS fan-out/drain framing, and rustdoc→guide links corrected; the guide
  gained the `list` and agent-id-shape rules.
- **Acknowledged, not a bug:** the gate is a library API with no production
  caller yet — `serve` exposes only the unauthenticated CID-level CAS, and
  token-gated remote exposure of the named service is M5's charter. Every
  gate/token finding is therefore latent-M5; the logic is what this milestone
  verifies.

## Sequencing note

M2 touches fq-store only (plus its new NATS dependency behind the seam); it can
proceed in parallel with M3 (extraction) per the parent plan. The `fq-cas serve`
boundary stays localhost-only through M2 — remote exposure of the *named*
service (where these tokens ride the wire) is M5's charter.

## References

- [ADR-0023 — storage and vector foundation](../../adrs/accepted/0023-storage-and-vector-foundation.md)
  (F4, F6) · [ADR-0024 — separate databases](../../adrs/accepted/0024-separate-databases-storage-foundation.md)
- [Storage + vector foundation plan](../active/2026-06-27-storage-vector-foundation.md) (M2)
- [M1c implementation plan (closed)](2026-06-30-m1c-gc-implementation.md)
  — the slice/verification pattern this plan mirrors
