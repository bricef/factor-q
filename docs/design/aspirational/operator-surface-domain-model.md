# Operator surface — domain model

**Status:** working model, agreed 2026-07-21 (design discussion on #346,
after three review rounds on the registry crate each surfaced an ontology
correction rather than a code defect — the signal to model the domain
unconstrained by the implementation). This document is the basis for the
[registry+split execution plan](../../plans/active/2026-07-20-registry-and-split-execution.md)'s
registry work. Adopting it formally means a small amendment to
[ADR-0006](../../adrs/accepted/0006-registry-first-api.md) (the kind
taxonomy, see "Deltas" below); do that in the change that lands the
reworked registry crate, and move this doc to `committed/` per the
[design-docs process](../README.md).

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

The initial catalogue:

| Resource | Nature | Notes |
|---|---|---|
| Event | atom | the substrate; every other resource derives from it |
| TranscriptEntry | atom | filtered by invocation |
| DeadLetter | atom | born of trigger exhaustion |
| Trigger | atom | the one operator-creatable resource (see Create) |
| Invocation | view | fold: phase, totals, archive status |
| Worker | view | fold: registration + heartbeats + ownership |
| Agent | view | the daemon's registry snapshot (reload swaps it) |
| Operation | view | the surface describing itself: the catalogue of promises |

That last row is deliberate self-similarity: "describe the registry" is
just List(Operation) — the catalogue is a resource like any other, read
through the same generic verbs it describes.

## Generic verbs and the stream overlay

Resources take **generic verbs** — defined once, derived for every
resource in the catalogue:

- **Get** — one resource by identity. Views answer as of a watermark.
- **List** — resources matching a typed, per-resource filter (agent,
  status, since, limit — *not* a query language), plus the watermark the
  answer reflects.
- **Create** — rare by design: operators create Triggers; the system
  creates everything else.
- **Stream** — the overlay, atoms only: *"send me resources of type X, at
  or after sequence S, as soon as they exist."* Because atoms are
  immutable, streaming is creation-notification — nothing else needs
  modelling.

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
| deadletter.requeue | Write trigger | selects the newest dead letter; **not idempotent**; fresh delivery budget |
| worker.prune | Delete worker | evicts stale registrations; co-emits its events (no silent mutation) |
| control.down | Write meta | drain-to-step-boundary then exit; confirmation is the shutdown event |
| control.reload | Write meta | registry swap affects next trigger only |

(`deadletter.requeue` *could* be read as Create(Trigger, provenance=
dead-letter) — it stays a domain verb precisely because that reading would
bury its non-idempotency, which is the fact the caller must know.)

Commands return **receipts** — references (subject, stream, sequence) to
the atoms they appended, never state. A receipt's watermark feeds the next
Get/List for read-your-writes.

## Reports

The kind the earlier taxonomy was missing. A report is a **named, typed
computation over resources**: cost.summary, cost.by_agent, doctor. Reports
are not Gets on a pretend-resource and not a query language — each is an
individually named promise with typed parameters and a typed result, few
by design, and watermarked like any read.

## The meta surface

Health, status, version — questions about the *machinery*, not the
records. This was the misfit "Probe" kind: probes were never operations on
this domain. They form a **flat surface**: a closed set of perhaps a dozen
operations, no taxonomy (bring one when the set stops being closed — for
now it is overkill), served behind the same edge with the **same access
control semantics** as everything else. For permission modelling they
scope to a `meta` pseudo-resource (Read meta for health/status/version;
control.down/reload already write it).

## Access control, uniformly

One vocabulary across all four categories — verb × scope, where scope is
a resource type (or `meta`):

- Get / List / Stream ⇒ Read on the resource's scope
- Create ⇒ Write on the resource's scope
- Domain verbs ⇒ declare their verb (see table)
- Reports ⇒ Read on their input scope(s)
- Meta ⇒ Read or Write on `meta`

## Deltas against ADR-0006 (to record on adoption)

- **D2's kinds refine.** Command / Query / Stream / Probe becomes:
  generic resource verbs (Get, List, Create) + Stream overlay + domain
  verbs (the Command survivors) + **Reports** (new) — and Probe leaves the
  registry taxonomy for the flat meta surface.
- **P8 inverts.** Names are derived from structure (resource + generic
  verb, or the declared name of a domain verb/report), never parsed;
  grammar-by-vocabulary is gone entirely.
- **Per-domain op enumerations dissolve.** `agent.list` / `worker.show` /
  `invocation.list` were never domain facts — they are the catalogue ×
  generic-verb cross-product, derivable. What remains hand-declared is
  exactly what is semantically bespoke: the catalogue itself, five domain
  verbs, three reports, the meta set.
- Everything else stands: receipts (D3), watermarks (D4), sequence
  cursors (D5), derived surfaces (D6), the authority vocabulary (D7),
  NATS interior (D8).

## Out of scope

Process lifecycle (`fq init`, `fq run`/`fqd`) and local pure functions
(`fq agent validate`) are not surface promises. The ADR-0016 agent-facing
built-ins converge on this model later (plan Phase 7); the graph
executor's signature work should check itself against the Resource/Report
split when it arrives.
