# Access control in fq-store

How the store decides who may do what — the grants model, where authorization
is enforced, and (most importantly for operators) **what happens when you
revoke**. This implements
[ADR-0023](../adrs/accepted/0023-storage-and-vector-foundation.md) F4;
the build is tracked in the
[M2 plan](../plans/closed/2026-07-03-m2-access-control.md).

> **Status:** built, verified, and soak-tested — the model, event log,
> projection, capability tokens, the enforcement gate, and the operator CLI
> below. The gate is a library API: it is exercised in tests and driven by the
> CLI's operator paths, but is not yet wired to a remote transport (that is
> M5's charter — see [Where authorization is decided](#where-authorization-is-decided)).

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

Two details worth knowing up front. **Listing** a namespace enumerates its
whole subtree, so it needs a `list` grant over the **namespace** (a
`Namespace` scope) — a grant on a single `Name` lets you read that one object
but not enumerate a tree. And an **agent id** is a plain slug (no dots,
wildcards, or whitespace); the dot-free rule is what keeps one agent's own
scope from nesting inside another's.

## Where authorization is decided

Grants are event-sourced: every grant, delegation, and revocation is an event
in a durable, ordered log **on the store's own disk** — the authoritative
record. A queryable **projection** (current permissions, with delegation-chain
liveness precomputed) is updated **in the same database transaction as every
event append**, so the projection is never behind the log — not by a little,
not briefly, not at a crash point.

Every event is also queued in a durable **outbox** for fan-out to NATS
(`fq.store.grant.*`) — an audit/replication feed for external consumers. That
feed is **never consulted for authorization**: a broker outage delays external
visibility, never local enforcement, and never grant writes (they commit
locally regardless and the outbox drains when the bus returns). The outbox
pump (`drain` / the `bus`-feature NATS publisher) is wired into a running
deployment as part of the service integration; the CLI and library do not
publish on their own.

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

## Attenuation and scope kinds

Attenuation narrows a token offline — to fewer verbs, and/or to a smaller
scope — and can never widen (checks are conjunctive; claim A2). Scope
attenuation follows the same two-kind model as grants, and the two kinds
bound **different operation shapes**:

- Attenuated to a **namespace** (`research.papers.*`): the token permits
  point operations on names inside the subtree *and* subtree operations
  (`list`, a namespace-scoped `grant` or `revoke`) anchored at or below
  the root. A subtree operation on a strict parent is outside the
  attenuation.
- Attenuated to a **name** (`research.papers`): the token permits point
  operations on exactly that name — and **nothing with namespace
  semantics**, including a `list` or a namespace delegation anchored at
  the very same string. A point capability never licenses a subtree.

The gate evaluates every operation against the token with its scope
**kind**, not just its resource string (the `scope_kind` datalog fact), so
these bounds hold structurally — the token layer agrees with the domain's
`Scope::covers_scope` case-for-case. The same rule applies to the offline
(`authorizes`) semantic a future remote verifier (M5) will use: a `name`
right never satisfies a namespace-semantics operation.

> **Upgrade note.** Tokens attenuated to a *name* by builds prior to this
> rule carry the older, kind-blind check and keep the wider behaviour
> until they expire — a window bounded by the token TTL (default 300 s).
> Namespace attenuations behave identically before and after. If a store
> ran with long-TTL name-attenuated tokens in circulation, rotate the
> store keypair to invalidate them at once.

## Managing grants and tokens

The `fq-cas` CLI is the **operator** surface — root authority by possession of
the store, no token needed. (Agent-issued *delegation* is an API operation
through the enforcement gate, not a CLI command.)

Scopes use one piece of sugar: a trailing `.*` means the namespace subtree
(`research.papers.*` covers `research.papers` and everything under it); a bare
dotted name means exactly that name. Verbs are a comma list
(`read,write,delete,list,grant`) or `all`.

```console
$ fq-cas key generate                # once per store: keep private secret
private: 8a4b…
public:  f31c…

$ fq-cas grant add bob read,write research.papers.*
7                                    # the grant id
$ fq-cas grant ls bob
7	read,write	research.papers.*
$ fq-cas grant check bob read research.papers.doc1
allowed                              # exit 0 (denied -> exit 1): scriptable
$ fq-cas grant rm 7                  # immediate, cascades through delegations
```

Tokens are minted from an agent's **live** grants (a mint after a revocation
carries nothing) and verified with only the public key:

```console
$ export FQ_BISCUIT_PRIVATE_KEY=$(cat store.key)   # avoid the literal in shell history
$ fq-cas token mint bob --ttl 300
<base64 token>
$ fq-cas token attenuate <token> --scope research.papers.reviews.* --verbs read \
    --biscuit-public-key f31c…
<narrower base64 token>              # attenuation can only narrow (offline)
$ fq-cas token inspect <token> --biscuit-public-key f31c…
principal: bob
```

Keys come from `--biscuit-private-key` / `FQ_BISCUIT_PRIVATE_KEY` (minting
only) and `--biscuit-public-key` / `FQ_BISCUIT_PUBLIC_KEY` (verification).
The grant history and current permissions live in `<root>/grants.db`,
alongside the storage index.

## See also

- [Operating fq-cas](operating-fq-cas.md) — running the store day to day.
- [M2 — access control: implementation plan](../plans/closed/2026-07-03-m2-access-control.md)
  — the claims (A1–A6), slices, and decisions behind these semantics.
- [ADR-0023](../adrs/accepted/0023-storage-and-vector-foundation.md) (F4) —
  the design fork this implements.
