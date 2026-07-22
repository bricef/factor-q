# ADR-0031: Split the runtime and CLI into `fqd` and `fq`, over a typed tarpc control interface

## Status

Accepted 2026-07-20 by Brice — proposed 2026-07-17.

## Context

Today `fq` is one binary that is both the **daemon** (`fq run`) and the
**operator CLI**. The CLI reads by opening the three SQLite stores locally
(directly or via `Views`) and mutates via NATS plus the control-plane
`operator` API. It therefore links `sqlx`, the whole of `fq-runtime`, and
NATS — the operator surface is coupled to storage and transport internals.

This is the debt behind #261 (route CLI reads through the `Views` reader)
and #264 (make the CLI link no SQL). But grounding those showed the real
shape is larger: `Views` *is* the sqlx wrapper, so no amount of read-routing
removes `sqlx` from the CLI while the daemon lives in the same binary. And the CLI's
mutating commands still go through NATS directly, so there is no single
boundary to authenticate.

Two localhost tarpc services already exist — `ReadService` (runtime, #105) and
`CasService` (`fq-store`) — both plaintext-bincode-over-loopback and
**unauthenticated**, with non-loopback binds *refused* pending "broader auth
work" (M5). factor-q is intended to be operated **remotely** (an operator
laptop driving a runtime on a server), which the current localhost-only,
unauthenticated posture cannot support.

## Decision

Split into two binaries:

- **`fqd`** — the daemon. Owns the runtime, the three SQLite stores, and *all*
  NATS interaction. Serves a single typed control interface over tarpc.
- **`fq`** — the operator CLI. A thin client that speaks only that interface.
  No `sqlx`, no store handle, no direct NATS.

The interface is the **derived tarpc binding of the operation registry**
(ADR-0006): a generic `invoke`/`next_batch` pair carrying
schema-versioned, registry-defined operations, fronted by generated typed
client wrappers in the shared wire crate. The command surface (`reload`,
`down`, `trigger`, `invocation drop`, `workers prune`, dead-letter
`requeue`) and the read surface are registry entries, not trait methods.
Streaming reads (`events tail`, `invocation transcript --follow`) are
**sequence-addressed stream operations**; their tarpc binding is long-poll
`next_batch(from_seq, max_wait)` — the cursor-polling previously described
here, now as the transport binding of a native stream contract rather than
the contract itself.

Access is authenticated by a **shared secret over TLS**. `fqd` terminates TLS —
encryption plus server authentication via a self-signed certificate the client
**pins** — and requires a shared secret (bearer token) that authorises control.
Authentication is a **transport / middleware layer beneath the RPC contract**,
so the mechanism can evolve (→ mTLS for a multi-operator fleet) without
touching the interface or the handlers. With auth in place, `fqd` may bind a
**non-loopback** address. The daemon is **required** for `fq` — there is no
local-store fallback (that would re-link `sqlx` into `fq`).

## Rationale

- **Clean layering.** The operator surface depends on a typed contract, not on
  storage internals; schema and backend changes stay behind `fqd`. This retires
  the #261/#264 debt at the root rather than centralising it.
- **Remote operation is the point.** `fq` on a laptop controls `fqd` on a
  server, encrypted and authenticated — impossible with the current posture.
- **Consistent with the codebase.** It extends the established tarpc discipline
  (`ReadService`/`CasService`, `bind`/`serve`, `WireError`) and honours ADR-0016
  (typed operations, no free-form APIs). The interface *is* the contract.
- **Right-weight auth.** For single-tenant remote, TLS gives transport security
  and server identity; one shared secret gives operator authorisation. mTLS's
  per-client identity and revocation aren't needed until there are multiple
  operators — and the middleware seam keeps that a later, non-breaking swap.
- **Precedented shape.** dockerd/docker, containerd/ctr: a required daemon and a
  thin client over an authenticated endpoint.

## Consequences

- `fq` becomes a small, independently distributable binary (client + wire
  types); no SQL or NATS dependency.
- NATS and store access consolidate behind `fqd` — a single audited edge, and
  the natural place to enforce authz, cost, and policy on operator actions.
- The daemon becomes **required** for the CLI — a change from today's
  daemon-optional local reads. Acceptable and docker-like; called out so it is a
  decision, not a surprise.
- Hardening the `fq↔fqd` edge is only *coherent* as part of the broader M5
  posture: `fqd↔NATS` and `worker↔NATS` remain unauthenticated on loopback, so
  this ADR hardens one edge and **names the rest as M5** (a local attacker can
  still reach NATS directly until then).
- A self-signed server cert means the client **pins** it (trust-on-first-use or
  an explicit fingerprint) to avoid MITM; `fqd` auto-provisions the cert and the
  secret on first run, so there is no operator crypto toil.
- A new sqlx-free **wire-types + client crate** is introduced; the `*View`
  types, `TranscriptEntry`, the `ControlService` trait, and the client live
  there. `fqd` depends on it and on `fq-runtime`; `fq` depends on it alone.
- Two-binary distribution touches the ADR-0022 release matrix and `install.sh`.

## Alternatives considered

- **Keep one binary, only route reads through `Views` (the original #261).**
  Centralises DB access but keeps `sqlx`/NATS in the CLI and offers no remote or
  auth story. Insufficient for a remote operator surface; this ADR subsumes it
  (#261 becomes the read-centralisation on-ramp to the client-crate extraction).
- **Unix domain socket + filesystem permissions (local-only auth).** Simpler and
  kernel-enforced, and it would have been the v1 pick *if* the target were
  local-only — but it cannot serve a remote operator. Rejected on the remote
  requirement.
- **mTLS now.** Stronger per-client identity and revocation, but heavier than
  single-tenant needs (a CA plus per-client cert lifecycle). Deferred behind the
  middleware seam until a multi-operator fleet warrants it.
- **`fq` keeps its own NATS connection for commands and tails.** Avoids proxying
  but re-couples the CLI to NATS and its credentials, defeating the
  single-interface goal. Rejected: commands become RPCs, tails become
  cursor-polling.
- **HTTP/REST or gRPC instead of tarpc.** tarpc is already the codebase's IPC; a
  second RPC stack is unjustified. Browser-facing HTTP stays a separate concern
  in `fq-dashboard`.

## Open questions (deferred by decision)

- Secret bootstrap and rotation UX — printed on first run, config file, a
  `rotate` command? (Execution detail.)
- Server-cert trust — TOFU pin vs. an explicit fingerprint in `fq` config.
- Whether `fqd` fronts the CAS `ReadService`/`CasService` under the *same*
  transport-auth layer, or those stay separate; likely unify, sequencing TBD.
- The rest of the M5 posture (NATS authentication) — a separate ADR/plan.
- Migration to mTLS when the multi-operator/fleet story lands.

---

## Appendix A — Amendment: capability tokens and the wire-crate split (2026-07-22)

Recorded ahead of the Phase-2 edge implementation
([execution plan](../../plans/active/2026-07-20-registry-and-split-execution.md)),
superseding two details of the original decision. TLS with a pinned
self-signed certificate, the auth-beneath-the-RPC-contract seam, the
required daemon, and everything else stand unchanged.

### Capability tokens replace the shared secret

The original decision authenticated with a single shared secret,
reasoning that per-client identity was a multi-operator fleet concern.
That premise was already false at the time of writing: the surface has
multiple *clients* with different privilege needs today — the
operator CLI (full authority), `fq-dashboard` (a strictly read-only
service), and the MCP face to come (capability-filtered per
declaration). One secret collapses every caller into "the secret
holder", which:

- makes the registry's declared authority (`Verb` × domain, derived
  Read on the generic surface, own-scoped reports) unenforceable at
  the edge — a read-only dashboard would hold a credential that can
  invoke `control.down`;
- leaves D7's "envelopes carry the authenticated operator identity"
  with a constant where an identity should be.

Access is therefore authenticated by a **bearer biscuit capability
token**, presented at connection establishment after the TLS
handshake — the same transport mechanics the secret would have used,
but the bytes are a verifiable, attenuable capability carrying
`(verb, domain)` grants and a principal identity fact. This is reuse,
not invention: fq-store already ships biscuit mint/verify/attenuate
(Ed25519) over the same `Verb` × scope vocabulary the registry's
authority declarations mirror. The edge middleware verifies the token
and subset-checks the resolved operation's required authority against
the token's grants; audit identity comes from the token's principal.

Bootstrap keeps the one-line UX the secret promised: on first run the
daemon mints a root keypair and prints an **admin token** alongside
the certificate fingerprint. Scoped clients come from **offline
attenuation** — the operator attenuates their admin token down to,
e.g., Read-everything for the dashboard, with no daemon round-trip; a
minting/rotation verb can follow when needed. Revocation is the known
bearer-capability cost: expiry caveats and root-key rotation (the
shared secret had the same cost, without the blast-radius reduction
of scoped tokens). mTLS-later remains available behind the unchanged
middleware seam, though per-client identity no longer depends on it.

### The wire crate is two crates

ADR-0006's Appendix A declared the sqlx-free wire-types + client
crate **is** `fq-ops`. Realizing ADR-0006 (its Appendix B) made
`fq-ops` a deliberately transport-free leaf — its dependency gate
forbids tarpc itself — so the transport half lives in a sibling:
**`fq-edge`** (the tarpc service trait, the generic
`invoke`/`next_batch` envelopes, the wire-error vocabulary, and the
client with TLS, pinning, and token presentation), depending on
`fq-ops` and still free of sqlx and NATS. The thin `fq` of the split
links `fq-edge` → `fq-ops`. This supersedes the corresponding
sentence of ADR-0006 Appendix A.
