# ADR-0031: Split the runtime and CLI into `fqd` and `fq`, over a typed tarpc control interface

## Status

Draft — proposed 2026-07-17.

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

The interface is one typed **`ControlService`** (tarpc), extending today's
read-only `ReadService` with the command surface (`reload`, `down`,
`trigger`, `invocation drop`, `workers prune`, dead-letter `requeue`).
Streaming reads (`events tail`, `invocation transcript --follow`) are
re-expressed as **cursor-polling** over the interface (the existing
`transcript_since` pattern), so `fq` needs no NATS connection at all.

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
