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
| 1 | Verification harness — the authorization oracle + property/DST scaffold | the net itself | **next** |
| 2 | Grant events + the event-log seam + durable outbox | A5, A6 | |
| 3 | The grant projection (SQLite #2): apply, rebuild, idempotency | A3, A5 | |
| 4 | Biscuit tokens: mint / verify / attenuate; key config | A1, A2, TTL | |
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
