# Access control in fq-store

How the store decides who may do what — the grants model, where authorization
is enforced, and (most importantly for operators) **what happens when you
revoke**. This implements
[ADR-0023](../adrs/accepted/0023-storage-and-vector-foundation.md) F4;
the build is tracked in the
[M2 plan](../plans/active/2026-07-03-m2-access-control.md).

> **Status:** the model, event log, and projection below are built and
> verified; capability tokens and the enforcement gate are in progress, and
> the `fq-cas grant` / `fq-cas token` CLI ships when M2 completes. The
> semantics documented here are the contract that work is built against.

## The model in one paragraph

Every request is a **principal** (an agent, by its runtime id) performing a
**verb** (`read`, `write`, `delete`, `list`, `grant`) on a **resource** (a
dotted name). Authorization is **default-deny**: an operation is allowed only
if a live **grant** covers it — a grant confers a set of verbs over a
**scope** (one exact name, or a namespace subtree like `research.papers` and
everything under it). The only exception is a principal's **own scope**
(`system.agents.<id>` and below), which needs no grant. Grants are issued by
the **operator** (the store owner — root authority) or **delegated** by an
agent that itself holds the `grant` verb over a covering scope; a delegation
can only *narrow* what its delegator holds, and revoking a grant instantly
invalidates every delegation standing on it, transitively.

## Where authorization is decided

Grants are event-sourced: every grant, delegation, and revocation is an event
in a durable, ordered log **on the store's own disk** — the authoritative
record. A queryable **projection** (current permissions, with delegation-chain
liveness precomputed) is updated **in the same database transaction as every
event append**, so the projection is never behind the log — not by a little,
not briefly, not at a crash point.

Events also fan out to NATS (`fq.store.grant.*`) for audit and other
consumers, but that feed is **never consulted for authorization** — a broker
outage delays external visibility, never local enforcement, and never grant
writes (they queue durably and drain when the bus returns).

## Revocation semantics and the token TTL

Two mechanisms carry authority, and they behave differently on revocation —
this is the part worth internalizing:

1. **The projection** — the live authority. Because it commits atomically
   with the revocation event, a revoke is effective the moment the call
   returns.
2. **Capability tokens** (biscuit) — portable, cryptographically-verifiable
   proof of identity + (possibly attenuated) authority, with an expiry.
   Tokens verify **offline** with only the public key, which is what makes
   them portable — and also means a token minted *before* a revocation stays
   cryptographically valid until it expires.

The store closes that gap with **belt-and-braces enforcement**: in-process,
the gate requires **both** a valid token **and** a live projection check on
every operation. The token proves who you are and what you attenuated
yourself down to; the projection decides whether that authority still stands.

The practical consequences:

| After you revoke… | Takes effect |
|---|---|
| Operations checked in-process (everything in M2) | **Immediately** |
| New token mints | **Immediately** (minting reads the projection) |
| Extant tokens at a *remote* verifier (future, M5) | Within the token TTL — **default 300 s**, configurable |

So today there is **no window** in which a revoked grant can act. The token
TTL is not the in-process revocation bound — it exists for the distributed
future (M5), where a remote verifier may hold only the public key: there,
expiry bounds how long a stale token can live, and biscuit's revocation-id
distribution is the planned complement. Choose the TTL accordingly: it prices
*remote* staleness, not local safety.

Two things follow for operators:

- **Revoke with confidence.** You do not need to reason about token
  lifetimes when cutting off an agent in-process — the projection check makes
  the revocation immediate, including for everything the agent had delegated
  onward.
- **Don't stretch the TTL for convenience.** A long TTL costs nothing
  in-process today, but it widens the future remote window; the 300 s default
  is deliberate.

## See also

- [Operating fq-cas](operating-fq-cas.md) — running the store day to day.
- [M2 — access control: implementation plan](../plans/active/2026-07-03-m2-access-control.md)
  — the claims (A1–A6), slices, and decisions behind these semantics.
- [ADR-0023](../adrs/accepted/0023-storage-and-vector-foundation.md) (F4) —
  the design fork this implements.
