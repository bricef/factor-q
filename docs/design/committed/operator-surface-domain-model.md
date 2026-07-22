# Operator surface — domain model

**Status:** committed (2026-07-21) — realized by the `fq-ops` crate
(#346) and formally amended into
[ADR-0006](../../adrs/accepted/0006-registry-first-api.md) as its
Appendix B. Drafted the same day during the #346 design discussion,
after three review rounds each surfaced an ontology correction rather
than a code defect — the signal to model the domain unconstrained by
the implementation; the review that refined it is distilled in
[the design-review learnings](../../reviews/2026-07-21-fq-ops-design-review-learnings.md).
Basis for the
[registry+split execution plan](../../plans/active/2026-07-20-registry-and-split-execution.md)'s
registry work.

## The domain in one paragraph

The append-only event log is the system of record; everything else is
derived from it, and the log's **sequence is the domain's clock** — the
universal cursor and freshness watermark. The operator surface is a set of
named, versioned **promises** at the system boundary, each carrying its
contract (types, required authority, caveats). There are exactly four
categories of promise: generic verbs over **resources**, a short list of
bespoke **domain verbs**, **reports**, and a flat **meta surface** for the
machinery itself.

## Resources: atoms and views

A resource is a typed thing the surface can hand back. The catalogue
distinguishes two natures, and the distinction is load-bearing:

- **Atoms** are immutable once created. They are facts: once one exists it
  never changes and never disappears (retention aside). Atoms are the only
  streamable resources.
- **Views** (projections) have stable identity and changing state — but a
  view's state is a fold of atoms, so views change *only because new atoms
  exist*. A view is read as "the fold as of watermark W"; it is never
  streamed directly — you stream its atoms. ("Follow this invocation" is
  Stream(TranscriptEntry, invocation=I), not "stream the invocation." The
  dashboard's snapshot-then-cursor transcript already works exactly this
  way.)
- **Synthetic** resources stand for live machinery rather than recorded
  truth. There are no atoms behind them, no key (a machinery singleton
  needs none) and no filter; Get alone derives, answering with the
  machinery's current state. They exist to give the machinery's bespoke
  verbs a home — and a permission scope. Authority on a synthetic
  resource's verbs is always declared manually, given its nature.

In code the natures are **explicit value types** (`Atom` / `View` /
`Synthetic`), constructed with exactly the type parameters their
nature has and registered directly: the value handed to the registry
*is* the definition, and the verb set derives from the type.

The initial catalogue:

| Resource | Nature | Notes |
|---|---|---|
| Event | atom | the substrate; every other resource derives from it |
| Turn | atom | one action (an assistant output or a tool result), filtered by invocation; a **Round** is the bundle of Turns in one agent-loop iteration (the ADR-0027 step boundary is a Round boundary), recoverable via the `round` grouping key |
| DeadLetter | atom | born of trigger exhaustion |
| Trigger | atom | minted by `trigger.publish` (a domain verb) and by first-party adapters via the wire-contract SPI |
| Invocation | view | fold: phase, totals, archive status |
| Worker | view | fold: registration + heartbeats + ownership |
| Agent | view | the daemon's registry snapshot (reload swaps it) |
| Control | synthetic | the daemon machinery itself — its Get answers with the machinery state, and it carries the lifecycle verbs (down, reload; room for future ones such as peer join) |
| Operation | view | the surface describing itself: the catalogue of promises |

Domains need not all carry catalogue resources: `Cost` exists purely as
a permission scope for reports, as `Control` exists for the machinery.

That last row is deliberate self-similarity: "describe the registry" is
just List(Operation) — the catalogue is a resource like any other, read
through the same generic verbs it describes.

## Generic verbs and the stream overlay

Resources take **generic verbs** — defined once, derived for every
resource in the catalogue:

- **Get** — one resource by identity. Views answer as of a watermark;
  a synthetic resource's Get takes no input.
- **List** — resources matching a typed, per-resource filter (agent,
  status, since, limit — *not* a query language), plus the watermark the
  answer reflects.
- **Stream** — the overlay, atoms only: *"send me resources of type X, at
  or after sequence S, as soon as they exist."* Because atoms are
  immutable, streaming is creation-notification — nothing else needs
  modelling.

**The generic surface is read-only.** Creation is not a generic verb:
operators do not create rows, they command the machinery, and atoms
appear in the log as receipts — so every mutation on the whole surface
is a declared domain verb (`trigger.publish`, not `trigger.create`).
Derived authority is therefore always and only Read.

List and Stream compose into one idiom, not two operations: List answers
"what exists, as of watermark W"; Stream continues "and from W onward,
live." Snapshot-then-follow, resumable by construction because sequence is
the cursor.

## Domain verbs

Where the surface is genuinely bespoke, it stays bespoke — a short,
curated list of commands whose *semantics are the contract* (receipts,
idempotency, caveats), never hidden behind a generic verb:

| Verb | Authority | The contract that makes it bespoke |
|---|---|---|
| invocation.drop | Write invocation | kill-switch: archived as failed; workers observe at the next step boundary |
| trigger.publish | Write trigger | dispatch work: at-least-once with a bounded budget; the receipt references the appended trigger atom |
| deadletter.requeue | Write trigger | selects the newest dead letter; **not idempotent**; fresh delivery budget |
| worker.prune | Delete worker | evicts stale registrations; co-emits its events (no silent mutation) |
| control.down | Write control (manual) | drain-to-step-boundary then exit; confirmation is the shutdown event |
| control.reload | Write control (manual) | registry swap affects next trigger only |

(Verbs that mint atoms — `trigger.publish`, `deadletter.requeue` — are
still verbs, not generic creation: their semantics (delivery budget,
non-idempotency) are the contract, and `trigger.publish`'s authority
(Write trigger) stays separately grantable from the machinery's
lifecycle authority.)

Commands return **receipts** — model-native references to the atoms
they appended, never state: `AtomRef { domain, seq }`, the same
sequence that cursors streams and watermarks reads (P5). Bus
coordinates (subjects, stream names) are internal infrastructure (D8),
mapped by the edge, never exposed in a receipt. A receipt's watermark
is **per-domain** (sequences from different domains are not
comparable) and feeds the next Get/List of that domain for
read-your-writes.

## Reports

The kind the earlier taxonomy was missing. A report is a **named, typed
computation over resources**: `cost.summary`, `cost.by_agent`,
`control.doctor`. Reports are not Gets on a pretend-resource and not a
query language — each is an individually named promise with typed
parameters and a typed result, few by design, and watermarked like any
read.

A report attaches to a domain as its **permission scope** — authority
is Read on that scope, *not* on its inputs. That makes aggregates a
privilege boundary: `cost.summary` (scope `Cost`) is grantable without
granting the raw event log it computes from, which is much of the
point of having aggregates on the surface. Handlers read their inputs
with system authority regardless; input lineage is contract prose, not
machinery.

## The meta surface

Health, status, version — questions about the *machinery*, not the
records. This was the misfit "Probe" kind: probes were never operations
on this domain, and during realization they collapsed further than
first drafted: the machinery describes itself through **one generic
read, `control.get`**, answering with the control state (version,
liveness) — no separate meta category, no per-probe operations, the
same access-control semantics as everything else (Read control), on
the same resource whose lifecycle verbs write it. Bring taxonomy back
only if the machinery surface stops being small.

## Access control, uniformly

One vocabulary across the whole surface — verb × scope, where scope is
a domain (which may exist purely as a scope, like `Cost`):

- Get / List / Stream ⇒ Read on the resource's domain — derived, and
  the *only* derived authority: the generic surface is read-only
- Domain verbs ⇒ declare their verb (see table); verbs on the synthetic
  Control resource always declare manually
- Reports ⇒ Read on their own domain (never their inputs — aggregates
  are a privilege boundary)

## Deltas against ADR-0006 (recorded as its Appendix B)

- **D2's kinds refine.** Command / Query / Stream / Probe becomes:
  generic resource reads (Get, List) + Stream overlay + domain verbs
  (the Command survivors, including the atom-minting ones) +
  **Reports** (new). Probe dissolves entirely into `control.get`, and
  Create does not exist — the generic surface is read-only.
- **P8 inverts.** Names are derived from structure (resource + generic
  verb, or the declared `(domain, word)` of a verb/report), never
  parsed; grammar-by-vocabulary is gone entirely. Identity is native
  on the wire; requests are refusable, not unrepresentable.
- **D1's production method becomes value declarations.** The five
  entity kinds are value types with constructors generic over their
  Rust types; the value registered *is* the definition — no Operation
  trait, no descriptor projection.
- **D3's receipts are model-native.** `AtomRef { domain, seq }`;
  watermarks per-domain; bus coordinates never in receipts (D8).
- **Per-domain op enumerations dissolve.** `agent.list` / `worker.show` /
  `invocation.get` were never domain facts — they are the catalogue ×
  generic-verb cross-product, derivable. What remains hand-declared is
  exactly what is semantically bespoke: the catalogue itself, six
  domain verbs, three reports.
- **D6's generic envelopes are edge artifacts**, designed with the
  Phase-2 tarpc service rather than in the contract crate.
- Everything else stands: receipts (D3), watermarks (D4), sequence
  cursors (D5), derived surfaces (D6), the authority vocabulary (D7),
  NATS interior (D8).

## Out of scope

Process lifecycle (`fq init`, `fq run`/`fqd`) and local pure functions
(`fq agent validate`) are not surface promises. The ADR-0016 agent-facing
built-ins converge on this model later (plan Phase 7); the graph
executor's signature work should check itself against the Resource/Report
split when it arrives.


## Appendix — the roster, stress-tested

Every operation from the
[interface inventory](../../reviews/2026-07-20-interface-inventory.md)
mapped into the model (updated to the realized vocabulary). Most of
the roster dissolves into generic reads over the catalogue; what
remains declared is declared on purpose.

| Inventory op | In the model |
|---|---|
| `event.query` / `event.tail` | List(Event) / Stream(Event) |
| `invocation.transcript` / `.tail` | List / Stream(Turn, invocation=I) |
| `deadletter.list` | List(DeadLetter) |
| `invocation.list` / `.show` | List / Get(Invocation) |
| `worker.list` / `.show` | List / Get(Worker) |
| `agent.list` / `.show` | List / Get(Agent) |
| `registry.describe` | List(Operation) |
| `traversal.status` / `.tail` | Get(Traversal) / Stream(TraversalEvent) |
| `trigger.publish` · `traversal.run` · `invocation.drop` · `deadletter.requeue` · `worker.prune` · `control.down` · `control.reload` | domain verbs |
| `cost.summary` · `cost.by_agent` (scope `Cost`) · `control.doctor` (scope `Control`) | reports |
| `runtime.health` · `runtime.status` · `runtime.version` | `control.get` — one machinery read |

Findings worth keeping:

- **Traversal is the proof of "born derived":** the whole trio costs one
  catalogue row, not three op definitions — the original ADR-0006
  motivation, now literal.
- **The overlay mints unasked-for but useful surface:** Stream(DeadLetter)
  ("tell me the moment something dead-letters") and List(Trigger)
  (pending triggers) fall out free. Reads (Get/List, +Stream for atoms)
  are uniform across the catalogue.
- **An ADR-0006 open question resolves:** streams share List's typed
  per-resource filter — no subject-glob language, and today's raw NATS
  subject argument to `fq events tail` retires (D8 alignment).
- **Authority mostly derives:** the generic surface derives Read and
  nothing else; only domain verbs declare by hand, and reports derive
  Read on their own scope.
- **The one wobble resolved itself:** `control.down`/`reload` initially
  sat awkwardly beside a read-only meta surface — until Control became a
  synthetic resource. Verbs attach to resources everywhere else in the
  model; the machinery's verbs attach to the machinery's resource, with
  manual authority, and future control verbs (peer join, …) have a home.
- **Phase-7 preview:** CAS blobs/objects are atoms par excellence;
  object-version history is atoms under a named-view fold — the model
  extends to the fq-store registry instance without strain.
