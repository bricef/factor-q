# Design documents

Two kinds of design doc live here, and the folder says which is which:

- **[`committed/`](committed/)** — describes the system as built, or a
  design decision in force (typically backed by an
  [accepted ADR](../adrs/accepted/) or shipped code). If a committed doc
  contradicts the code, one of them is wrong — fix whichever it is.
- **[`aspirational/`](aspirational/)** — design-ahead: explorations,
  wishlists, and proposals for work that is not yet built.
  These are thinking tools, not commitments; they may be adopted, revised,
  or abandoned without ceremony.

**The folders track construction, not agreement.** A decision can be
accepted and still unbuilt, and that doc belongs in `aspirational/` — the
ADR records that we agreed, the folder records whether it exists. Getting
this backwards is what a 2026-08-26 review found across the design set:
several documents described mechanisms nobody had written, in the present
indicative, from `committed/`. A reader has no way to tell an account of
the system from a plan for one unless the folder does that work.

So: **a doc moves to `committed/` when the thing it describes runs**, not
when its ADR is accepted. If a document is mostly built with a named
unbuilt part, it may stay in `committed/` provided that part is called out
explicitly — but the default for "accepted, not yet built" is
`aspirational/`, with a Status section at the top saying what actually
ships today.

Dated files at this level (`YYYY-MM-DD-*-assessment.md`) are point-in-time
assessments — snapshots that are never updated, only superseded.

**Movement between folders:** when an aspirational design is adopted, record
the decision as an ADR and move the doc to `committed/` (updating inbound
links); if a committed doc stops matching reality and isn't worth fixing,
demote or delete it. Do the reclassification in the same change that alters
the doc's status, so the folders stay trustworthy.
