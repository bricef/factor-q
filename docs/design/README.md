# Design documents

Two kinds of design doc live here, and the folder says which is which:

- **[`committed/`](committed/)** — describes the system as built, or a
  design decision in force (typically backed by an
  [accepted ADR](../adrs/accepted/) or shipped code). If a committed doc
  contradicts the code, one of them is wrong — fix whichever it is.
- **[`aspirational/`](aspirational/)** — design-ahead: explorations,
  wishlists, and proposals for work that is not yet scheduled or accepted.
  These are thinking tools, not commitments; they may be adopted, revised,
  or abandoned without ceremony.

Dated files at this level (`YYYY-MM-DD-*-assessment.md`) are point-in-time
assessments — snapshots that are never updated, only superseded.

**Movement between folders:** when an aspirational design is adopted, record
the decision as an ADR and move the doc to `committed/` (updating inbound
links); if a committed doc stops matching reality and isn't worth fixing,
demote or delete it. Do the reclassification in the same change that alters
the doc's status, so the folders stay trustworthy.
