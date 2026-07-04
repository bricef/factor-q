# M2 — access control: implementation plan

**Status:** active (2026-07-03). The slice-by-slice build of grants + capability
tokens, on branch `m2-access-control`.

The **"what"** and **"why"** live elsewhere — the resolved design in
[ADR-0023](../../adrs/accepted/0023-storage-and-vector-foundation.md) (F4:
event-sourced grant claims → permission projection, biscuit capability tokens as
the wire mechanism) and the milestone in the
[storage + vector foundation plan](2026-06-27-storage-vector-foundation.md).
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
  the revoked grant succeeds (new mints fail; extant tokens die with their TTL,
  which bounds the revocation window).
- **A4 — delegation is grant-gated:** only a principal holding `grant` on a
  scope can delegate within it, and only attenuated (⊆ its own scope).
- **A5 — projection ≡ replay:** the projection equals a fresh rebuild from the
  event log + pending outbox, after any event sequence and at any crash point.
- **A6 — availability:** data-plane operations never touch the bus; with the
  publisher down, reads/writes/authorization proceed and grant writes still
  commit locally (outbox), draining when the bus returns.

## Slices

| # | Slice | Validates | Status |
|---|---|---|---|
| 1 | Verification harness — the authorization oracle + property/DST scaffold | the net itself | done |
| 2 | Grant events + the event-log seam + durable outbox | A5, A6 | done |
| 3 | The grant projection (SQLite #2): apply, rebuild, idempotency | A3, A5 | done |
| 4 | Biscuit tokens: mint / verify / attenuate; key config | A1, A2, TTL | **next** |
| 5 | The op-boundary gate — `can()` at the named layer | A1, A3, A4 end-to-end | |
| 6 | CLI UX (`grant` / `token`) + user + operator docs | — | |
| 7 | Fault DST — bus outage, crash-replay, revocation races + soak | A5, A6, recovery | |

## Decisions taken up front

- **The event log sits behind a seam.** A small trait (append + replay) with an
  in-memory implementation for tests and a NATS/JetStream implementation for
  real — the `ContentStore`/`NameIndex` conformance pattern. fq-store stays
  hermetically testable; NATS-backed integration tests run against the local
  infra via the `just` recipes.
- **Publisher failure never affects store availability.** Two rules. The data
  plane (read/write/authorize) makes **no bus calls** — verification is local
  (token + projection). Grant *writes* append to a **durable local outbox** in
  the same transaction that updates the projection, and an async drain publishes
  to NATS with retry; a bus outage delays publication, never grant acceptance.
  The rebuild contract is therefore `replay(published log) + pending outbox`
  (A5), and the oracle checks it under outage + crash injection.
- **The gate sits at the Repository/service level, not the CAS.** A gating
  layer wraps the named operations and evaluates `can(Principal, Op, Resource)`
  before delegating; `ContentStore` (and the raw `Repository` beneath the gate)
  remain **preserved internal APIs** — same layering lesson as M1c's
  `BlockStore` split. Prefix/glob grants match the dotted names
  (`research.papers.*`; `system.agents.<id>.files.*` for harness-only scopes).
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

## Sequencing note

M2 touches fq-store only (plus its new NATS dependency behind the seam); it can
proceed in parallel with M3 (extraction) per the parent plan. The `fq-cas serve`
boundary stays localhost-only through M2 — remote exposure of the *named*
service (where these tokens ride the wire) is M5's charter.

## References

- [ADR-0023 — storage and vector foundation](../../adrs/accepted/0023-storage-and-vector-foundation.md)
  (F4, F6) · [ADR-0024 — separate databases](../../adrs/accepted/0024-separate-databases-storage-foundation.md)
- [Storage + vector foundation plan](2026-06-27-storage-vector-foundation.md) (M2)
- [M1c implementation plan (closed)](../closed/2026-06-30-m1c-gc-implementation.md)
  — the slice/verification pattern this plan mirrors
