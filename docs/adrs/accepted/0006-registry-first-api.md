# ADR-0006: Registry-first runtime API — typed operations, CQRS surface, derived interfaces

## Status

Accepted on 2026-07-20 by Brice — proposed 2026-07-18. Replaces the prior draft text of ADR-0006 (which answered the read half descriptively and left the write/admin half open).

Amends ADR-0031 (Appendix A). Informed by the interface inventory of
2026-07-18 and #84.

## Context

factor-q's operator surface is described many times over. A read operation
exists in four hand-maintained places — a `Views` method, a `ReadService` RPC,
its forwarding handler, and a clap variant with dispatch arm and renderer —
five counting the dashboard BFF route. Writes are worse: they travel three
unrelated mechanisms (NATS control subjects, the operator API over the bus,
JetStream trigger publish), and three paths bypass every boundary entirely
(`fq trigger`'s in-process default, `fq workers prune`'s direct store write,
`fq agent list`'s local-filesystem registry). The full census is in
`docs/design/interface-inventory.md`: 21 CLI verbs, 16 `Views` methods, 15
`ReadService` RPCs, 9 `CasService` RPCs, 24 `fq-cas` verbs, 11 dashboard
routes — roughly 21 runtime operations and 20 store operations at steady
state, before traversal operations arrive.

ADR-0031 (draft, 2026-07-17) settles the topology: split into `fqd` and a
thin `fq`, one authenticated control edge, TLS plus shared secret, tarpc as
house IPC. It currently specifies the interface as a hand-enumerated
`ControlService` trait — a sixth hand-written contract — and re-expresses
streams as cursor-polling.

Meanwhile the system underneath is already command/query-segregated: the
event log is the system of record (ADR-0026), the SQLite projections behind
`Views` are the read model, and every mutation that matters is an appended
event. The API surface is the only layer that does not admit this.

This ADR decides how every interface is *produced*, and fixes the design
principles new surfaces must honour.

## Decision

### D1 — One registry, one description per operation

Introduce `fq-ops`: a sqlx-free crate defining an `Operation` contract, a
registry, and generic adapters. It is also the wire-types-plus-client crate
ADR-0031 calls for. Every operator- or system-facing capability is defined
exactly once:

```rust
pub enum OpKind { Command, Query, Stream, Probe }

pub trait Operation {
    /// Must parse under the naming grammar (P8).
    const NAME: &'static str;                 // e.g. "invocation.drop"
    const KIND: OpKind;
    type Input:  Serialize + DeserializeOwned + JsonSchema;
    type Output: Serialize + DeserializeOwned + JsonSchema;
    const META: OpMeta;                       // permission (Verb × scope),
                                              // audit class, stability,
                                              // caveats (retention,
                                              // idempotency, semantics)
}
```

Handlers register daemon-side. Surface code that hand-describes an operation
is, from acceptance of this ADR, a defect.

### D2 — The surface is CQRS because the system is

Operations are typed as **Command**, **Query**, or **Stream**, with
deliberately asymmetric contracts (D3–D5). **Probe** exists for the two
live-infrastructure reads (`runtime.health`, `runtime.status` — NATS and
JetStream checks) that are neither projection queries nor commands; it is
kept deliberately small.

### D3 — Commands return receipts, never state

A command's output is a `Receipt`: references (subject, stream, sequence) to
the events it appended. Command handlers do not read projections to return
"fresh" state; composition happens through watermarks (D4). Two families:

- **Domain commands** (`invocation.drop`, `trigger.publish`,
  `deadletter.requeue`, `worker.prune`) mutate by appending to the log.
  Where a handler must also touch coordination state (prune's stale-row
  removal), it co-emits the corresponding events — **no silent mutation**
  (P3). Prune's current direct, event-free store write is exactly the defect
  class this retires.
- **Runtime-control commands** (`control.reload`, `control.down`) address the
  daemon's own lifecycle. Post-0031 they are in-process handlers — the NATS
  control-subject hop existed only because no RPC channel did. They
  acknowledge directly and are audited via system events
  (`fq.system.shutdown` exists; reload gains a sibling). The
  `fq.control.*` subjects retire from the operator path and remain, at most,
  an internal fan-out between `fqd` and workers.

### D4 — Queries answer from projections, with watermarks

Queries transplant the `Views` bodies and return the existing `*View` types.
Every query accepts an optional `min_seq`: the handler waits (bounded) until
the projection's applied sequence reaches the watermark, then answers. Since
a receipt *is* a sequence, `drop` → `show` composes into read-your-writes
without ever coupling the command path to the read model. Requires the
projection to track its applied sequence — plumbing shared with #139.

### D5 — Streams are first-class and sequence-addressed

`Stream` operations have a typed `Item`; **every item carries its event-log
sequence**; every stream input takes `from_seq`. That single invariant makes
each transport binding mechanical:

- **SSE**: `id:` is the sequence; `Last-Event-ID` resume falls out.
- **tarpc** (no server-streaming): long-poll `next_batch(from_seq, max_wait)`
  — push latency, zero transport work. This supersedes 0031's cursor-polling
  *convention*: polling remains the tarpc binding, but the contract is native.
- **MCP**: notifications.

Daemon-side, a stream is an ephemeral JetStream consumer starting at
`from_seq`. `event.tail` thereby *improves*: today's core-NATS subscribe
silently drops across disconnects; a sequence-resumable stream cannot.
The consumption idiom is snapshot-then-stream-from-watermark, as the
dashboard's transcript view already does. Streams are the exception, not the
default: an endpoint is `Stream` only when its subject matter is genuinely
unbounded and live (`event.tail`, transcript follow, traversal progress);
everything else is a `Query`.

### D6 — Surfaces are derived, in this order

1. **tarpc control edge** (the 0031 interface): a generic
   `invoke(name, input) / next_batch(...)` pair behind the 0031 auth
   middleware, with **generated typed client wrappers** so end-to-end static
   typing survives while the wire stays uniform — one choke point for auth,
   audit, versioning, and cost middleware.
2. **CLI**: parsing and dispatch derive from `Input` (the structs carry
   serde + clap + schemars derives together); the ~19 human renderers remain
   hand-written (P7). `--json` becomes uniform and free, structurally
   resolving #190.
3. **MCP server** (rmcp, already in-tree): one op per tool,
   capability-filtered — the #84 headline.
4. **REST + SSE**: shape specified by this ADR, activation deferred until an
   external consumer exists.

The dashboard re-points to the generated client; the `ReadService` mirror and
its forwarding layer are deleted.

### D7 — Authorisation and audit live in metadata

`OpMeta.permission` uses the `Verb { Read, Write, Delete, List, Grant }` ×
scope vocabulary and biscuit semantics already shipped in fq-store's gate;
enforcement is registry middleware on the edge. Commands are audited by their
own events, whose envelopes carry the authenticated operator identity;
queries and streams audit per an `OpMeta` flag (read audit is opt-in, not
noise). #183's "trim the RPC surface" becomes a permission declaration.
fq-store adopts the same `fq-ops` crate and vocabulary as a second registry
instance; unifying its transport under `fqd`'s edge is sequenced with M5
(open).

### D8 — NATS is internal infrastructure, not public API

The bus retreats inside the trust boundary. Exposing NATS externally would
mean a second, parallel auth system (accounts, nkeys, subject permissions)
beside the single audited edge, and would freeze subject topology and event
payload schemas into public compatibility surface — freedom exercised as
recently as #200, gone the day a third party pins a subject. Precedent runs
one way: Kubernetes does not expose etcd; Docker does not expose containerd.

**One carve-out**: `fq.trigger.<agent>` remains a documented **internal SPI**
for co-located, first-party adapters (the github-watcher) — durable
at-least-once ingress is JetStream's gift. Remote ingress goes through
`trigger.publish` on the authenticated edge. The watcher's outcome
subscription migrates to a first-class stream op when convenient; scoped NATS
credentials for a third party are a deliberate future exception, never the
default posture.

## Design principles (going forward)

- **P1 — Define once, derive interfaces.** One operation definition; every
  surface is a generic adapter. A new surface is an adapter, not a project; a
  hand-written per-op surface is a defect.
- **P2 — The surface is isomorphic to the architecture.** Commands end at the
  log; projections answer queries; the log streams back out. The API admits
  what the system is.
- **P3 — No silent mutation.** Domain state changes flow through the log;
  any operational-store mutation co-emits its events. If it isn't in the
  log, it didn't happen.
- **P4 — Receipts, not state.** Commands return event references. Freshness
  is the caller's to compose via watermarks, never the command path's to
  guarantee.
- **P5 — Sequence is the universal cursor.** Streams and watermarks speak
  event-log sequence. Resumability is a contract property, not a transport
  feature.
- **P6 — One audited edge.** All external interaction crosses the
  authenticated `fqd` boundary. Internal transports are replaceable
  implementation detail.
- **P7 — Derive the mechanical, keep the human.** Parsing, dispatch, wire,
  schema, docs derive. Renderers — the CLI's actual value — stay
  hand-written.
- **P8 — Names carry the grammar.** `<domain>.<imperative>` is a command
  (`invocation.drop`); `<domain>.<noun>` is a query (`invocation.list`);
  `<domain>.tail` is a stream. An op whose name misparses is misclassified.
- **P9 — Metadata is contract.** Permissions, audit class, stability,
  retention and idempotency caveats live in `OpMeta`, so every derived
  surface inherits them; help-text lore is promoted to contract.
- **P10 — One type language.** Op schemas, event schemas, and signature
  schemas are the same language, versioned `name@version` with additive-change
  rules. An operation is a signature with a host-side execution strategy —
  what keeps "agent, graph node, operator command" interchangeable.
- **P11 — Curate the registry.** Deprecation and versioning are first-class
  workflows (ADR-0016's warning, generalised). Growth without curation
  degrades the whole point.

## Consequences

### Positive

- Per-op descriptions collapse 4–5 → 1 (+ renderer); the `ReadService`
  mirror, forwarding handlers, and per-command JSON plumbing are deleted;
  the would-be hand-written `ControlService` is never written.
- The three boundary-bypassing paths are eliminated by construction:
  in-process trigger retires (or moves behind a dev-only `fqd` pathway),
  prune becomes an evented command, `agent.*` answers from the daemon's
  registry.
- #190 dissolves structurally; #183 becomes metadata; #261/#264 are subsumed
  one level deeper than 0031 predicted; the unversioned bincode dashboard
  wire is replaced by a versioned, schema'd contract.
- Traversal operations (`traversal.run`, `.status`, `.tail`) are born
  derived — the original reason this decision precedes the graph executor.
- M5's "SDK" becomes codegen from schemas rather than a project; the MCP
  face is ~an adapter.
- ADR-0016 convergence path: agent-facing built-ins become
  capability-filtered registry ops behind the MCP adapter — one registry,
  three audiences (operator, agent, external).

### Negative

- schemars adoption across ~30 types (net-new dependency; a few manual impls
  for `Cid`-class types).
- CLI ergonomics is where derivation fights reality; per-op clap attributes
  on input structs mitigate — and make the struct the single description
  point, which is the goal.
- The generic invoke gives up per-method wire types; mitigated by generated
  typed wrappers, at the cost of a codegen step.
- Watermarked queries and sequence-addressed streams need projection
  applied-seq tracking and JetStream consumer lifecycle management.
- The Command/Query/Stream/Probe discipline requires review vigilance;
  misclassification is now a reviewable defect, which is the point but also
  a cost.

### Neutral

- Renderers, `init`/`run` process lifecycle, and event bus internals are
  untouched. NATS remains fully load-bearing internally.
- Crash-domain separation of fq-store is preserved regardless of the
  transport-unification outcome.

## Migration

- **Phase 0** — adopt this ADR; amend ADR-0031 per Appendix A. Decision
  only.
- **Phase 1** — `fq-ops` crate + three exemplar ops, one per kind:
  `invocation.show` (Query), `invocation.drop` (Command),
  `invocation.transcript.tail` (Stream), wired through the generic edge
  behind 0031 auth. Old paths intact.
- **Phase 2** — fleet-sized migration of the remaining ops; CLI and
  dashboard flip to the generated client; delete the `ReadService` mirror.
  Completes #261/#264 as side effects.
- **Phase 3** — MCP server face; traversal ops land registry-native.
- **Phase 4** — fq-store registry instance; ADR-0016 built-in convergence;
  transport unification per M5 sequencing.

Phases 0–1 are human work (days). Phase 2 is ~17 near-identical PRs — fleet
fodder once the exemplars fix the pattern.

## Alternatives considered

- **ADR-0031 as drafted (hand-enumerated `ControlService`).** Adopts the
  right topology, transport, and auth — all retained here — but produces the
  sixth hand-written contract and re-freezes streams as a polling
  convention. Rejected as the *interface production method*; superseded by
  derivation.
- **Macro-expanding the registry into a per-method tarpc trait.** Preserves
  per-method wire types without wrappers; costs a macro-heavy layer and
  scatters the middleware seam across N methods. Held as fallback if generic
  invoke ergonomics disappoint in Phase 1.
- **Exposing NATS as the public API** (accounts/nkeys/subject permissions).
  Dual auth systems, public freeze of subjects and payload schemas, no
  single audited edge. Rejected (D8).
- **REST/OpenAPI-first.** Wrong first face: the browser edge is already
  BFF'd, tarpc is house IPC, and OpenAPI derives from the same schemas
  whenever needed. Deferred, not rejected.
- **Uniform request/response ops, no CQRS typing.** Invites read-after-write
  coupling in command handlers and forfeits receipt/watermark composition;
  discards structure the architecture already pays for. Rejected.
- **Streams by convention (snapshot + cursor pairs) rather than kind.**
  Works — it is today's dashboard — but hides resumability from the
  contract and multiplies ad-hoc cursor types. Rejected in favour of D5.

## Open questions (deferred by decision)

- Secret bootstrap/rotation UX and cert pinning — inherited from ADR-0031.
- CAS transport unification under the `fqd` edge — sequencing with M5.
- REST activation criterion (first external consumer).
- Server-side filter language for `event.tail` (subject globs vs. typed
  filters).
- Per-op cost controls at the edge (composes with ADR-0004) and retention
  class for opt-in read-audit events.

## References

See pul requests #84 · #166 · #183 · #190 · #200 · #261 · #264 · #139 ·
ADR-0002 · ADR-0004 · ADR-0016 · ADR-0022 · ADR-0026 · ADR-0027 · ADR-0031 ·
`docs/design/interface-inventory.md` (2026-07-18) ·
github-watcher trigger wire contract · fq-store M2 access-control model.

---

## Appendix A — Amendment to ADR-0031

Replace the paragraph beginning "The interface is one typed
**`ControlService`** (tarpc)…" with:

> The interface is the **derived tarpc binding of the operation registry**
> (ADR-0006): a generic `invoke`/`next_batch` pair carrying
> schema-versioned, registry-defined operations, fronted by generated typed
> client wrappers in the shared wire crate. The command surface (`reload`,
> `down`, `trigger`, `invocation drop`, `workers prune`, dead-letter
> `requeue`) and the read surface are registry entries, not trait methods.
> Streaming reads (`events tail`, `invocation transcript --follow`) are
> **sequence-addressed stream operations**; their tarpc binding is long-poll
> `next_batch(from_seq, max_wait)` — the cursor-polling previously described
> here, now as the transport binding of a native stream contract rather than
> the contract itself.

All transport, TLS, shared-secret, binding, and distribution decisions of
ADR-0031 stand unchanged. The sqlx-free "wire-types + client crate" it
introduces **is** `fq-ops`.

---

## Appendix B — Refinements from the operator-surface domain model (2026-07-21)

Adopted with the `fq-ops` contract crate (#346), whose design review
produced
[the committed domain model](../../design/committed/operator-surface-domain-model.md)
(review distilled in
[the design-review learnings](../../reviews/2026-07-21-fq-ops-design-review-learnings.md)).
Decisions D1–D8 stand; the *production method* they specify is refined:

- **D1's registry holds value declarations.** The `Operation` trait
  sketched in D1 became five value types — `Atom`, `View`, `Synthetic`
  (resources, whose nature is the type and whose generic read surface
  derives from one registration), `Command`, and `Report` — with
  constructors generic over each declaration's Rust types, capturing
  schemas at the single declaration site. The value registered **is**
  the definition; there is no descriptor projection.
- **D2's kinds become derived categories.** Get / List / Stream over
  catalogue resources, plus declared Commands and Reports. `Probe`
  dissolves into `control.get` — the synthetic Control resource
  describing the machinery in one generic read. **Create does not
  exist**: the generic surface is read-only, and every mutation is a
  declared command (`trigger.publish`, not `trigger.create`), keeping
  work-dispatch authority separately grantable from lifecycle
  authority.
- **Reports scope to their own domain** (`cost.summary` → Read on
  `Cost`), never to their inputs — aggregates are a privilege
  boundary, grantable without the raw data they compute from. Domains
  may exist purely as scopes.
- **D3's receipts are model-native.** `Receipt` carries
  `AtomRef { domain, seq }` — the universal cursor (P5) — with
  per-domain watermarks; bus coordinates (subjects, stream names)
  never appear in receipts, per D8.
- **P8 inverts.** Names derive from structure for documentation and
  string-addressed adapters; nothing parses them, and identity is
  native on the wire. Wire identity is *request vocabulary*: invalid
  combinations (a stream of a view-registered domain) remain
  representable and are refused at resolve time — version skew
  answers "not registered", never a parse error. Declarations, by
  contrast, keep invalid states unrepresentable.
- **D6's generic `invoke`/`next_batch` envelopes are edge artifacts**,
  designed with the Phase-2 tarpc service rather than speculatively in
  the contract crate.

### Typed op identifiers (2026-07-24)

The stringly halves of `OpId` — `Verb { domain, verb: String }` and
`Report { domain, name: String }` — are replaced by typed identities:
per-domain verb/report enums (`Invocation::Drop`,
`Cost::Summary`) wrapped in `VerbId`/`ReportId` sums. A verb is
now named at exactly one site (the declaration constructor takes the
typed id and derives the domain from it), and nonsense pairs
(`cost.drop`) are unrepresentable by construction. The registry stays
the single semantic source — schemas, authority, handlers, describe —
and the wire encoding is byte-identical to the pre-typed flat pair;
version skew parses to an `Unknown` variant the registry refuses as
not-registered, so unknown vocabulary degrades to a clean refusal,
never a wire error. Rationale: with client and daemon in one
workspace and BFFs fronting all other protocols, the open string
namespace bought nothing at the wire that the registry does not
already provide, while costing compile-time exhaustiveness in every
consumer.
