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

**Amended 2026-08-05**, on four points. `invocation.resume` becomes a
declared verb mirroring `invocation.drop` (there is no reason two
actions on the same resource should be reachable different ways). The
model states plainly that **NATS is not an external control surface**,
recorded as
[ADR-0006 Appendix C](../../adrs/accepted/0006-registry-first-api.md).
Registry state, load errors included, moves onto the machinery scope
as `control.status`, leaving `agent.list` homogeneous — which in turn
forced the sharper correction of **2026-08-06**: a synthetic has no
Get at all, and the machinery reads (`control.status`,
`control.doctor`) are reports, recorded as
[ADR-0006 Appendix D](../../adrs/accepted/0006-registry-first-api.md).
And two entries that
described the world inaccurately — `traversal.run`, which does not
exist, and `deadletter.requeue`, which is not how the codebase spells
it — are reconciled before cohort 4.3 mints them into code. Unblocks
verb 19 of the
[Phase-4 call-point inventory](../../plans/active/2026-07-28-phase-4-call-point-inventory.md).

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
  streamable resources. An atom's List answers with the atoms themselves
  by default; it may instead answer from an index, under the rule in
  [Generic verbs](#an-atoms-list-may-answer-from-a-different-store-than-its-get).
- **Views** (projections) have stable identity and changing state — but a
  view's state is a fold of atoms, so views change *only because new atoms
  exist*. A view is read as "the fold as of watermark W"; it is never
  streamed directly — you stream its atoms. Its List answers with the
  view's **index** — one row per fold, cheap to enumerate, declared as
  its own schema — not N full folds: Get returns the state, List
  returns index rows, and the two shapes genuinely differ. ("Follow this invocation" is
  Stream(TranscriptEntry, invocation=I), not "stream the invocation." The
  dashboard's snapshot-then-cursor transcript already works exactly this
  way.)
- **Synthetic** resources stand for live machinery rather than recorded
  truth. There are no atoms behind them, no key, no filter, no state
  schema — and **no generic read at all: a synthetic derives nothing.**
  It exists to give the machinery's bespoke verbs a home and a
  permission scope, and that is the whole of its job. Authority on a
  synthetic resource's verbs is always declared manually, given its
  nature. Machinery *state* is read through reports scoped to it
  (`control.status`, `control.doctor`) — see [the meta
  surface](#the-meta-surface).

In code the natures are **explicit value types** (`Atom` / `View` /
`Synthetic`), constructed with exactly the type parameters their
nature has and registered directly: the value handed to the registry
*is* the definition, and the verb set derives from the type. A
synthetic's type parameters are therefore **none** — it declares a
domain, a summary and a stability, and nothing else, because it
derives nothing. `fq_ops::Synthetic` still carries a `state_schema`
and still derives a Get, from the earlier reading; [the meta
surface](#the-meta-surface) records what that costs to correct.

The initial catalogue:

| Resource | Nature | Notes |
|---|---|---|
| Event | atom | the substrate; every other resource derives from it |
| Turn | atom | one action (an assistant output or a tool result), filtered by invocation; a **Round** is the bundle of Turns in one agent-loop iteration (the ADR-0027 step boundary is a Round boundary), recoverable via the `round` grouping key |
| DeadLetter | atom | born of trigger exhaustion |
| Trigger | atom | minted by `trigger.publish` (a domain verb) and by **co-located** first-party adapters via the wire-contract SPI — ingress, not control (see the control-surface principle) |
| Invocation | view | fold: phase, totals, archive status |
| Worker | view | fold: registration + heartbeats + ownership |
| Agent | view | the daemon's registry snapshot (reload swaps it); its index rows are agent definitions and nothing else — a file that failed to parse never became an agent, so it belongs to the machinery |
| Control | synthetic | the daemon machinery itself — a permission scope with **no generic reads**, carrying the lifecycle verbs (down, reload; room for future ones such as peer join) and scoping the machinery reports (`control.status`, which answers with machinery state **including the registry's own, load errors and all**; `control.doctor`) |
| Operation | view | the surface describing itself: the catalogue of promises |

Domains need not all carry catalogue resources: `Cost` exists purely as
a permission scope for reports, as `Control` exists for the machinery.

That last row is deliberate self-similarity: "describe the registry" is
just List(Operation) — the catalogue is a resource like any other, read
through the same generic verbs it describes.

## Generic verbs and the stream overlay

Resources take **generic verbs** — defined once, derived for every
resource in the catalogue:

- **Get** — one resource by identity: atoms and views only. Views
  answer as of a watermark. Synthetics have no Get — nothing on the
  generic surface reads them.
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

### An atom's List may answer from a different store than its Get

A view's List has always returned index rows rather than folds. An
**atom's** List returns the atoms themselves *by default* — listing facts
hands back facts, which is what `turn.list` does and must keep doing. But
the default is a default, not a law, and an atom may declare a distinct
index (`Atom::with_index`) whose List is served from a cheaper store.

The rule, in full, because the next atom that wants a cheap listing should
have a rule to follow rather than a precedent to reverse-engineer:

> **An atom's List may answer from a different store than its Get, and
> may therefore answer with something narrower than the atom — provided
> every list row carries the identity Get takes, and the declaration says
> so.**

Both halves are load-bearing.

- **The row names what Get needs.** A listing that a caller cannot walk
  from is a dead end: they would have to reconstruct a key from fields
  that were never promised to reconstruct one, and the first schema change
  breaks them silently. Carrying the key makes the narrower row a
  *summary of a reachable fact* rather than a lossy substitute for it.
- **The declaration says so, in its `description`.** Not in prose a reader
  has to go and find — in the declared text, which is what
  `List(Operation)` publishes and what lands in `operator_surface.json`.
  The surface describes its own contract or the contract does not exist.

The shape of the answer, then, is a property of the *question*, not of
the storage that happens to be convenient: Get asks "this fact, whole",
List asks "what happened, narrowed and capped", and Stream asks "tell me
as they happen". **A reader who needs payloads in bulk streams; it does
not list.** Stream therefore always answers with the atom's state, never
its index — a stream is creation-notification, and half a fact is not a
notification of one.

`Event` is the first atom to take the opt-in. Its Get and Stream read the
event log, where the payload lives; its List reads the projection's
index — timestamp-ordered, indexed on the columns the filter narrows by,
and holding extracted fields rather than payloads. Serving that List from
the log would have made the operator's most-reached-for read cost a scan
in direct proportion to how much history the system had accumulated,
which is the wrong way round. Every index row carries its `seq`, so
`event.get` is one call away from any row.

## Domain verbs

Where the surface is genuinely bespoke, it stays bespoke — a short,
curated list of commands whose *semantics are the contract* (receipts,
idempotency, caveats), never hidden behind a generic verb:

| Verb | Authority | The contract that makes it bespoke |
|---|---|---|
| invocation.drop | Write invocation | archives as failed; workers observe at the next step boundary. Refused on an invocation the daemon is actively driving unless `--live`, which halts it at its next boundary first (in-flight tools finish) before the drop |
| invocation.resume | Write invocation | the counterpart to drop — reconciles unknown execution instead of abandoning it: durably completes every stuck tool dispatch with an honest interrupted result, then re-drives the invocation through ordinary SafeReplay recovery. Refused on anything not **Ambiguous**, and each refusal is distinct because the operator must be able to tell them apart: terminal (including operator-dropped), live on this daemon, stuck in an *LLM* dispatch (injection reconciles tool calls only), or already resumed |
| trigger.publish | Write trigger | dispatch work: at-least-once with a bounded budget; the receipt references the appended trigger atom |
| dead_letter.requeue | Write trigger | selects the newest dead letter; **not idempotent**; fresh delivery budget |
| worker.prune | Delete worker | evicts stale registrations; co-emits its events (no silent mutation) |
| control.down | Write control (manual) | drain-to-step-boundary then exit; confirmation is the shutdown event |
| control.reload | Write control (manual) | registry swap affects next trigger only |

(Verbs that mint atoms — `trigger.publish`, `dead_letter.requeue` — are
still verbs, not generic creation: their semantics (delivery budget,
non-idempotency) are the contract, and `trigger.publish`'s authority
(Write trigger) stays separately grantable from the machinery's
lifecycle authority.)

**Names are rendered, never chosen.** A verb's name is its domain's
segment plus its declared word (P8), and the domain segment is the
snake_case rendering of the `Domain` variant — `Domain::DeadLetter`
gives `dead_letter`. So `dead_letter.requeue` is the spelling, here and
everywhere. The `deadletter.requeue` this table carried until
2026-08-05 was not a second convention needing reconciliation; it was
prose disagreeing with a name structure had already decided, which is
the only way the two can ever differ.

Commands return **receipts** — model-native references to the atoms
they appended, never state: `AtomRef { domain, seq }`, the same
sequence that cursors streams and watermarks reads (P5). Bus
coordinates (subjects, stream names) are internal infrastructure (D8),
mapped by the edge, never exposed in a receipt. A receipt's watermark
is **per-domain** (sequences from different domains are not
comparable) and feeds the next Get/List of that domain for
read-your-writes.

### The two-authority hazard

`invocation.drop` and `invocation.resume` share a shape no other verb
has: each evaluates a precondition against records that are **older
than the decision it is about to make**, and each applies a durable
side effect **before** it has finished answering. That pair of
properties is what produced #445 and #383, so the invariants are
recorded here rather than rediscovered a third time.

**Drop's invariant (PR #445): it must never report `NotFound` for work
the daemon is actively running.** The liveness authority is the
in-memory runner; the identity authority is the projection — and they
do not share a clock, because the runner marks an invocation active on
its first line, seconds before anything durable names it. A `--live`
drop could therefore arm a halt that stops real work and *then* answer
that the invocation did not exist. The fix was structural rather than
defensive: the liveness authority now answers identity too, so
resolution is infallible exactly when the halt was armed, and the
partial application is unrepresentable instead of handled.

**Resume's invariant: it must never refuse an invocation whose state it
has already changed, and never act on a terminal decision it cannot yet
see.** Resume has both of drop's properties, in its own arrangement:

- Its side effect is the interrupted-result injection — one committed
  transaction that rewrites every stuck tool dispatch as completed.
  Two later steps still report failure after it (stored-identity
  validation, agent lookup), the audit publish is best-effort, and the
  re-drive is a detached task whose failure reaches only the log. And
  the injection is precisely what makes the invocation *stop* being
  Ambiguous, so a resume that injects and then fails leaves work no
  second resume will accept — stranded until a daemon restart's
  recovery sweep happens to pick it up as SafeReplay. Order every
  fallible step **before** the injection, so that past it the command
  is infallible and always answers with a receipt naming the
  `invocation.operator_resumed` atom. That receipt is not decoration:
  it is the only thing that tells an operator the WAL moved, and today
  the refusal and the after-the-fact failure are the same shape on the
  wire.
- Its precondition reads terminality from folds an asynchronous
  consumer writes. `invocation.drop` publishes and returns; until the
  coordination consumer applies that event, resume's guards both read
  "not terminal" and it will re-drive the invocation the operator just
  dropped (#383). Sequence is the domain's clock and the edge already
  gates reads at a watermark, so the terminal authority must be read
  at or after the coordinate the drop's receipt named — not sampled
  from whatever the fold currently says.

Neither is a caveat about one verb. The rule the next command of this
shape inherits: **a command that decides on a fold and acts on the
world owes the model an ordering in which its decision cannot be
overtaken and its effect cannot outrun its answer.**

## Reports

The kind the earlier taxonomy was missing. A report is a **named, typed
computation over resources**: `cost.summary`, `cost.by_agent`,
`control.doctor`, `control.status`. Reports are not Gets on a
pretend-resource and not a query language — each is an individually
named promise with typed parameters and a typed result, few by design,
and watermarked like any read.

**The machinery reports stretch that definition, and the stretch is
declared rather than hidden.** `control.status` answers largely with
state the daemon is holding — the registry's agents and its stored load
errors — which is not obviously a "computation over resources", and
`control.doctor` has always had the same character. Two things make it
acceptable. First, the caution the definition exists to enforce is
against reports that are *Gets on a pretend-resource*; since a
synthetic has no Get, there is no read for a machinery report to
duplicate or disguise, and the failure mode the caution names cannot
occur. Second, machinery state genuinely has no atoms behind it — it is
not a fold of anything — so the alternative is not a purer Get but a
second read mechanism carved out for one nature. A slightly stretched
definition of "computation" is the smaller cost.

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
first drafted — no separate meta category, no per-probe operations, the
same access-control semantics as everything else (Read control), on the
same domain whose lifecycle verbs write it. Bring taxonomy back only if
the machinery surface stops being small.

*Amended 2026-08-06.* Probe collapses into **reports on the `Control`
scope** — `control.status` (version, liveness, registry state) and
`control.doctor` — not into a generic read. The first drafting said
`control.get`, on the reading that a synthetic answers by Get alone;
that reading is withdrawn above. A synthetic is a permission scope that
hosts bespoke verbs, and nothing more.

### `control.status` — the accumulation point for machinery state

*Amended 2026-08-05, resolved 2026-08-06.* **The registry's current
state — load errors included — rides `control.status`, and `agent.list`
returns homogeneous agent rows.** `control.status` is also where
further machinery information accumulates, by growing one schema rather
than by growing the op roster.

The forcing case was `agent.list`, whose index row is a sum today
(`AgentEntryView::Agent(..) | ::LoadError { message }`) because a
view's List answers `Vec<Index>` with no envelope for
collection-level data, and a file that failed to parse has no agent id
to be listed under. The tell that the union is transport rather than
modelling is what every consumer does with it first: partition it
straight back into two lists. An agent entry is semantically an agent
definition, and a definition that failed to load never became one.

The model already had the right home and did not use it. Synthetics
stand for live machinery rather than recorded truth, and the daemon's
agent registry is live machinery by construction — `control.reload`
rebuilds it, which is why the Agent row is a view over a registry
snapshot rather than a fold of atoms. Registry state is machinery
state; it belongs on the machinery scope, where a load error is not an
anomalous row but an ordinary field.

This generalises. The question "where does *this* piece of machinery
state go?" now has one answer, which is what stops the next such value
being smuggled into whichever listing happens to be nearby.

#### What it renders as, and why the Get had to go

Naming the op `control.status` collided with a rendering rule: a
synthetic was said to answer by Get alone, and `OpId::Get(Domain::Control)`
renders `control.get`. The obvious repairs were all bad. Keeping
`control.get` ignores that the op was named deliberately and that
"get" names an identity lookup a synthetic has no key for. Letting a
synthetic carry a *named read verb* would put a second kind of read on
a surface whose entire read side is generic and derives Read and
nothing else. Adding a rendering exception for one nature buys a name
with a special case.

Declaring `control.status` a **report** on scope `Control` was the
remaining option, and it initially looked worse rather than better,
for a reason this document states plainly: *reports are not Gets on a
pretend-resource*. `Control` is not a pretend-resource, so a report
that duplicated its Get would be exactly what that caution is about.

**But that objection only holds while the Get is doing work.** The
model said two things about synthetics — that Get alone derives, and
that they exist to give the machinery's bespoke verbs a home and a
permission scope. The second is the load-bearing one. Once machinery
state is a report, the first is not a rule with an awkward exception;
it is simply false, and the honest response is to delete it rather
than build a naming rule on top of it.

So the resolution is not "status as a report *despite* the Get". It is:

- **A synthetic has no Get** — no key, no filter, no state schema, no
  derived read of any kind. It is a permission scope that hosts
  bespoke verbs. That is the amendment made in *Resources* above.
- **`control.status` and `control.doctor` are both reports** on that
  scope. The read side stays uniform, no rendering rule gains an
  exception, and the pretend-resource caution stops applying because
  there is no Get left to duplicate.
- **`Control` keeps doing what synthetics are for**: hosting
  `control.down` / `control.reload` and scoping authority (Read
  `Control` for the reports, manually declared Write for the verbs).

The lesson worth keeping is that the naming collision was a symptom.
Two rounds were spent looking for the right *name* for the machinery
read, when the defect was the *existence* of a generic read on a
nature that has nothing to read generically.

**What this costs, honestly.** `fq_ops::Synthetic` still carries a
`state_schema` and the registry still derives `Get` for it, so code
and model now disagree until cohort 4.4. That is deliberate: the
correction is not free, and a documentation amendment is not the place
to force it. Removing it touches `Synthetic::new`'s type
parameter, the registry's `derived_ops` and its synthetic-Get resolve
arm, three assertions in `fq-ops/tests/registry.rs`, the `ControlState`
fixture, `opid.rs`'s module doc, and — decisively — the committed
schema-snapshot oracle `tests/snapshots/exemplar_registry.json`, whose
`synthetic` entry serialises a `state_schema` today. It also needs the
`Control` report identities (`control.status`, `control.doctor`) that
do not exist yet. Do it with cohort 4.4, where the declaration lands
anyway.

## Access control, uniformly

One vocabulary across the whole surface — verb × scope, where scope is
a domain (which may exist purely as a scope, like `Cost`):

- Get / List / Stream ⇒ Read on the resource's domain — derived, and
  the *only* derived authority: the generic surface is read-only.
  Atoms and views only; a synthetic derives no read, so it derives no
  authority either
- Domain verbs ⇒ declare their verb (see table); verbs on the synthetic
  Control resource always declare manually
- Reports ⇒ Read on their own domain (never their inputs — aggregates
  are a privilege boundary)

## NATS is not an external control surface

The bus is the system's internal event log and coordination substrate.
It is not a control plane. **Nothing outside the daemon commands the
system by publishing to a subject** — every operator action is a
declared op on the authenticated edge, and a proposed verb that would
need a subject of its own is a verb whose declaration is missing.

ADR-0006's D8 ("NATS is internal infrastructure, not public API") and
ADR-0031's rejected alternative ("`fq` keeps its own NATS connection
for commands") each said where the bus must not be *exposed*. This is
the same commitment stated as a property of the architecture, so it
settles the next case rather than the last one:

- **Control flows one way in: client → edge → daemon → bus.** The
  daemon publishes because it owns the log; a client never does.
  `fq.control.*` is therefore not an entry point. The subjects that
  remain (`reload`, `down`, `invocation.resume`) are legacy, and
  retire with their verbs' flips.
- **A control subject is not a cheaper edge.** It skips authority
  (D7's verb × scope is enforced at the edge and nowhere else), skips
  the receipt/watermark contract (a subject can carry a reply, but not
  an `AtomRef` a caller can gate the next read on — P4/D3), and skips
  audit identity. A verb reached that way is not a lighter version of
  the declared verb; it is a different verb with none of the model's
  guarantees.
- **It also fails differently, and worse.** Core NATS answers "no
  responders" the instant nobody owns a subject — at the client,
  indistinguishable from a considered reply. `invocation.drop`'s
  liveness guard was a request/reply subject and read that answer as
  *"nothing is running, drop directly"*, so any window in which the
  subject was unowned silently bypassed the guard, a restart racing
  startup recovery being exactly when an operator reaches for drop
  (PR #441). Over the edge an unreachable daemon is a connection
  error, never a licence to proceed: the guard fails closed by
  construction, and the ordering constraint that used to defend it is
  gone rather than merely satisfied.
- **Ingress is not control.** `fq.trigger.<agent>` remains a
  documented SPI for co-located, first-party adapters (D8's one
  carve-out); the Trigger row above says so. It *submits work the
  daemon then decides about* — it does not command the daemon. Remote
  ingress is `trigger.publish` on the edge like everything else. The
  question to ask of the next proposal is not "is it NATS", it is
  **does the caller decide, or does the daemon**: anything expecting
  the system to act on the caller's authority is a declared op.

The practical test for a new verb: if it cannot be expressed as a
declared op with an authority, a typed input, and a receipt, that is a
finding about the verb — not a reason to reach for the bus.

## Deltas against ADR-0006 (recorded as its Appendix B)

- **D2's kinds refine.** Command / Query / Stream / Probe becomes:
  generic resource reads (Get, List) + Stream overlay + domain verbs
  (the Command survivors, including the atom-minting ones) +
  **Reports** (new). Probe dissolves entirely into reports on the
  `Control` scope (`control.status`, `control.doctor`) — *not* into a
  generic read; a synthetic has no Get (amended 2026-08-06). Create
  does not exist — the generic surface is read-only.
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
  exactly what is semantically bespoke: the catalogue itself, seven
  domain verbs, four reports.
- **D6's generic envelopes are edge artifacts**, designed with the
  Phase-2 tarpc service rather than in the contract crate.
- Everything else stands: receipts (D3), watermarks (D4), sequence
  cursors (D5), derived surfaces (D6), the authority vocabulary (D7),
  NATS interior (D8) — D8 now restated positively, as the
  control-surface principle above.

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
| `traversal.status` / `.tail` — **planned** | Get(Traversal) / Stream(TraversalEvent) |
| `trigger.publish` · `invocation.drop` · `invocation.resume` · `dead_letter.requeue` · `worker.prune` · `control.down` · `control.reload` | domain verbs |
| `traversal.run` — **planned** | a domain verb, when there is a graph executor to run |
| `cost.summary` · `cost.by_agent` (scope `Cost`) · `control.doctor` · `control.status` (scope `Control`) | reports |
| `runtime.health` · `runtime.status` · `runtime.version` | `control.status` — one machinery report |

**Planned rows are not surface.** Nothing named `traversal` exists in
the codebase: the graph executor is deliberately held (#414), so those
three rows say where the ops *will* land, not what the registry serves.
They are kept because the mapping is itself the finding below. Every
other row names a real domain concept — which op is *registered* yet is
the [Phase-4 inventory](../../plans/active/2026-07-28-phase-4-call-point-inventory.md)'s
business, not this table's — and that is exactly why the traversal rows
had to be marked: unmarked, they read as description, and cohort 4.3
would mint a verb the model only imagined.

Findings worth keeping:

- **Traversal is the proof of "born derived":** the whole trio costs one
  catalogue row, not three op definitions — the original ADR-0006
  motivation, applied. Still a prediction rather than a result, since
  the executor is held; the model's claim is about what the trio *will*
  cost when it arrives.
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
  The 2026-08-06 amendment sharpens this rather than unsettling it:
  once the synthetic's Get is gone, hosting verbs and scoping authority
  is *all* it does, which is what the finding said it was for.
- **Phase-7 preview:** CAS blobs/objects are atoms par excellence;
  object-version history is atoms under a named-view fold — the model
  extends to the fq-store registry instance without strain.
