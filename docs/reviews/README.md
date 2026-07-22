# Reviews and assessments

Point-in-time examinations of the project, named `YYYY-MM-DD-slug.md`
and pinned to the state of the repository on that date (most cite the
exact commit in their header). They come in a few flavours:

- **Self-assessments** — critical reflections on the design or the
  project as a whole, taken at a milestone boundary to orient resumption
  of work.
- **Cold code reviews** — in-depth reviews of the codebase produced
  without the author's context, to surface what a fresh reader finds.
- **Inventories** — exhaustive snapshots of some surface (e.g. every
  read/write endpoint), captured as input to a specific decision.
- **Retrospective learnings** — what a completed piece of work taught
  us, extracted as candidate principles.

A review is never updated — it is a snapshot, only ever superseded by a
later one. If its findings led to changes, those show up in code, plans,
and ADRs, not as edits here. Dated subfolders (e.g. `2026-07-16/`) hold
snapshot artifacts — diagrams and other captures — belonging to a review
of that date.

Consult this folder when you want an honest picture of the project at
some past moment (strengths, risks, open questions as seen then), when
resuming work after a gap, or when looking for the lessons behind the
[design principles](../design/committed/design-principles.md). Reviews
*inform* decisions; the decisions themselves are recorded in
[`../adrs/`](../adrs/), and present-day behaviour in
[`../guide/`](../guide/).
