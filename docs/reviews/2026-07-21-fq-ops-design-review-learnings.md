# fq-ops design review — what thirteen revisions taught us

A point-in-time retrospective on the registry crate's design journey
(PR #346, 2026-07-20 → 2026-07-21): from a string grammar with runtime
validation to five value-type domain entities and a registry that holds
the declarations themselves. The final crate is smaller than the first
attempt and does more. This document extracts why, as candidate
principles — promotion into
[design-principles](../design/committed/design-principles.md) is a
separate, deliberate act.

## The journey, compressed

String vocabulary + `expected_kind` parser → enums-of-enums with a
central `spec()` match → traits with consts → single-site declarations
→ **value types behind an `Entry` enum** → explicit `Atom`/`View`/
`Synthetic` types → domain-scoped report permissions → model-native
`AtomRef`. Deleted along the way: the grammar, three id enums and
their match tables, three nature markers, `Creatable`, `OpMeta`,
`ResourceDocs`, both descriptor types, the `Nature` enum and field,
`reads`, the speculative wire envelopes, and every string that
anything parsed.

## Lessons

1. **When review comments correct ontology, stop coding and model.**
   Three full implementations preceded the domain-modelling session;
   every pre-model review round was an ontology correction disguised
   as a code comment (names aren't parseable text; identity is a
   tree; kind belongs to the definition). One unconstrained modelling
   conversation (resources split atom/view/synthetic, generic verbs +
   stream overlay, domain verbs, reports, machinery) turned all
   subsequent code decisions into derivations. The review cycle is an
   expensive way to discover a domain model.

2. **Structure must carry semantic weight — in both directions.**
   Delete structure nothing branches on (`OpMeta`, the per-op nature
   field, the descriptors, `reads`); add structure where behaviour
   genuinely differs (`Atom`/`View`/`Synthetic` as types once their
   verb sets and constructor shapes diverged; `summary` + `description`
   once listings and detail views wanted different text). The tests:
   "does any code branch on this distinction?" and "is any consumer
   generic over this abstraction?" — a no to both means delete it, a
   yes to either means make it explicit.

3. **Declarations and requests sit on opposite sides of a trust
   boundary, with opposite rules.** Declarations (one binary, compile
   time): invalid states unrepresentable — `cost.drop` is a compile
   error. Requests (wire, cross-version): invalid states representable
   and *refusable* — `Stream(view-domain)` deserialises fine and
   resolves to not-registered, because turning version skew into parse
   errors is strictly worse. Both halves are the same principle
   applied to different trust boundaries.

4. **If you're projecting traits into descriptor structs, the
   declaration probably wanted to be a value.** The trait/descriptor
   duality (declare on an impl, project into a serialisable struct,
   keep them aligned) dissolved entirely once declarations became
   constructor calls: the registered value *is* the definition, and
   generic constructors (`Command::new::<Input>(…)`) keep type capture
   at the single declaration site — the same generic slot types the
   handler later.

5. **One-site declaration, enforced ruthlessly.** Adding an entity
   touches its declaration and one `register()` call — nothing else.
   Every violation (id enums + match tables, docs passed at the
   register call, parallel vocabularies) was deleted on sight, and
   each deletion shrank the crate.

6. **Rendering is documentation; identity is structure.** Names derive
   (`Display`), nothing parses, the wire carries native types, and the
   only guarantee strings owe is collision-freedom at registration —
   checked where the collision would happen, not policed by grammar.

7. **Infrastructure vocabulary in model types is a permanent leak.**
   `subject`/`stream` strings in receipts and raw NATS subjects in
   filters would freeze bus topology into public surface the day a
   caller pins one (D8). Model-native references — `(Domain, seq)` —
   keep the mapping behind the edge. Corollary discovered in the same
   fix: cross-domain sequence comparisons were quietly meaningless;
   watermarks are per-domain.

8. **Defer shapes to their consumer; keep rules as types.** The
   invoke/next_batch envelopes had zero in-crate consumers and were
   deleted — they'll be designed against the real tarpc edge. But
   `Receipt` stayed: "commands return atom references, never state" is
   a model *rule*, and the type is the rule's shape. The distinction:
   domain fact vs consumer convenience.

9. **Ask what a grant also grants.** Two authz designs fell to this
   question: `control.trigger` would have made work-dispatch and
   daemon-shutdown the same grant (verbs attach to their subject
   matter's domain instead); input-derived report authority made
   `cost.summary` require reading the whole event log (reports scope
   to their own domain, making aggregates a privilege boundary).

10. **The oracles made the churn cheap.** Thirteen reworks with zero
    behavioural regressions, because every one ran inside the golden
    net and the `describe()` snapshot — contract drift was always a
    reviewable diff, never a surprise. Build the oracle before the
    thing it will judge (the plan's Phase 0 ordering, vindicated).

11. **Borrowed vocabularies resolve stuck questions fast.** Turn
    granularity resisted definition until tabletop language (a Round
    of Turns) mapped it; `EndTurn` and the ADR-0027 step boundary were
    already speaking it. When naming stalls, look for the domain that
    already solved it.
