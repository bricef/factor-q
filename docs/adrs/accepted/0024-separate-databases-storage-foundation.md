# ADR-0024: Separate databases for the storage foundation's three stores

## Status

Accepted (2026-06-27). Refines
[ADR-0023](0023-storage-and-vector-foundation.md) (F9), which parked this
question; builds on
[ADR-0011](0011-event-bus-and-persistence.md) (event-sourced projections).

## Context

ADR-0023 establishes the storage + vector foundation and leaves open
whether its metadata stores share one database or are separated — even
though all are SQLite (sqlite-vec for vectors). The object/block *bytes*
live as files; the metadata splits into three stores in two durability
classes:

| Store | Holds | Class |
|---|---|---|
| Storage index | names → CIDs + version history, object/block metadata, refcounts | **authoritative** (lose it = data loss) |
| Grant projection | current permissions, projected from grant events | **derived** (rebuild from the event log) |
| Vector index | chunks + embeddings (sqlite-vec) | **derived** (rebuild by re-embedding) |

## Decision

**Three separate databases**, one per store.

The usual reason to share a database — cross-store ACID transactions — does
not apply here, because ADR-0023's own choices remove the need: vectors
update via the **async embed-on-store pipeline** (eventually consistent),
grants are **event-sourced** (the projection is rebuilt from the log), and
the only atomicity actually required (a name update + its refcount delta)
is **within the storage index**. So nothing needs a transaction spanning
the three stores.

## Rationale

- **Independent replacement/upgrade** — each store can be swapped or
  rebuilt on its own. This is the primary motivation: maximum flexibility
  as each function evolves.
- **Write-lock isolation** — SQLite serializes writers per file (even in
  WAL). A long vector **backfill** (re-embedding millions of chunks) would
  stall storage-index writes if shared; separate files give independent
  writers.
- **Durability-class separation** — derived stores (vectors, grants) can be
  dropped and rebuilt without endangering the authoritative storage index,
  and backed up at different criticality.
- **Physical backend-swap boundary** — F9 makes the vector engine swappable
  (sqlite-vec → Qdrant / LanceDB); a separate database makes that boundary
  physical, rather than one table inside a shared file.
- **Pre-stages service extraction** and keeps schema migrations isolated.

## Consequences

- Minor operational complexity: three SQLite files, each with its own
  connection and migration lifecycle. The v1 single binary opening three
  files is trivial — no distribution required.
- No cross-store transactions (not needed); `ATTACH DATABASE` recovers
  cross-database *queries* if they are ever wanted (it does not cleanly
  recover cross-database transactions — also not needed).
- The vector index is the store most likely to move off SQLite later; its
  separation keeps that change contained.
