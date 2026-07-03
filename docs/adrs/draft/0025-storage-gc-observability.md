# ADR-0025: Storage GC observability (Prometheus metrics)

## Status

Draft (2026-07-03). Builds on
[ADR-0023](../accepted/0023-storage-and-vector-foundation.md) (the storage
foundation and its M5 service) and the
[storage GC design](../../design/storage-garbage-collection.md) (M1c, built);
sets the metrics convention later runtime instrumentation should follow.

## Context

M1c shipped garbage collection with strong correctness guarantees but no
machine-readable telemetry from a running deployment. Today:

- GC runs **only** as the one-shot `fq-cas gc` CLI (operator- or
  cron-triggered). Its `AuditReport` — reclaimed objects/blocks, orphan
  files reaped, refcounts reconciled, **alarms** — is printed and discarded;
  only the exit code survives.
- `fq-cas serve` exposes the raw CAS over tarpc only. It holds no
  `Repository` (never opens the storage index), so it *cannot* run GC or
  report GC state, and it has no HTTP surface to scrape.
- No crate in the project depends on a metrics library; there is no
  `/metrics` endpoint anywhere. This ADR is the project's first metrics
  decision and sets the pattern.

The tension: Prometheus is pull-based, but until
[M5 — Service wiring + SDK](../../plans/active/2026-06-27-storage-vector-foundation.md)
there is no long-running process that owns the GC state to scrape. M5's
single-binary service composes the layers, opens the databases, and **runs
the GC worker** — the natural scrape target. The alarm signal (the
forbidden state: a live object missing a block) is the one metric that
must page someone, and it should not wait for M5 to become visible.

## Decision

Two-stage, converging on M5:

1. **Target (M5): the composed service exposes `/metrics`.** The M5
   single-binary service serves a Prometheus endpoint over its HTTP
   listener; the in-process GC worker updates metrics on every pass.
   Pull-model, live gauges, real histograms.
2. **Interim (now): `fq-cas gc` writes a textfile-collector snippet.**
   A `--prom-textfile <path>` flag makes the one-shot CLI write its report
   in Prometheus text exposition format (atomic rename), for
   node_exporter's textfile collector to pick up. Cron stays the
   scheduler; no new daemon, no Pushgateway to operate.
3. **Instrumentation goes through the `metrics` facade crate**, with the
   exporter bound only in the M5 binary (`metrics-exporter-prometheus`).
   Library crates (`fq-store`, later `fq-runtime`) record via the facade
   and stay exporter-agnostic.

### Metric set

Namespace `fq_store_`. Interim (one-shot) exposes last-run gauges; the M5
worker adds true counters and histograms.

| Metric | Type | Meaning |
|---|---|---|
| `fq_store_gc_alarms` | gauge | Invariant violations from the last audit. **Must be 0**; alert on `> 0`. |
| `fq_store_gc_last_run_timestamp_seconds` | gauge | When the last audit finished; alert on staleness. |
| `fq_store_gc_last_run_duration_seconds` | gauge (M5: histogram) | Audit pass duration. |
| `fq_store_gc_reclaimed_objects` / `_blocks` | gauge (M5: counter `_total`) | Reclaimed by the last pass (M5: cumulative). |
| `fq_store_gc_orphan_files_reaped` | gauge (M5: counter `_total`) | Crash-orphaned files removed. |
| `fq_store_gc_refcounts_reconciled` | gauge (M5: counter `_total`) | Leaked reservations corrected. |
| `fq_store_objects` / `_blocks` | gauge | Store size (from `Stats`). |
| `fq_store_logical_bytes` / `_physical_bytes` | gauge | Dedup picture (ratio derivable). |
| `fq_store_unreferenced_objects` / `_blocks` | gauge | GC pressure: garbage awaiting reclaim. |

Suggested alerts: `fq_store_gc_alarms > 0` (critical — treat as data
loss, see the [operator manual](../../guide/operating-fq-cas.md));
`time() - fq_store_gc_last_run_timestamp_seconds > 2×` the cron interval
(GC has stopped running).

## Rationale

- **The scrape-surface question is really a topology question.** Only a
  process holding the `Repository` can produce GC metrics; M5 is where
  that process is already chartered ("run the GC worker"). Deciding
  anything else (a bespoke daemon now, GC inside today's CAS-only
  `serve`) would pre-empt M5's design for little gain.
- **Textfile collector over Pushgateway for the interim.** The store is
  node-local (files + SQLite), so a node-local collector matches the
  deployment shape; it degrades gracefully (no extra service, no
  Pushgateway staleness semantics), and it is one small CLI flag.
- **Last-run gauges, not fake counters, in the interim.** One-shot runs
  cannot accumulate; counters that reset every run would make `rate()`
  lie. Gauges + a timestamp are honest and still alertable. True
  counters arrive with the long-lived M5 worker.
- **A facade now prevents a lock-in later.** The `metrics` crate keeps
  `fq-store` free of exporter dependencies and gives `fq-runtime` a
  convention to adopt when it is instrumented.

## Consequences

- `fq-cas gc` gains `--prom-textfile <path>` (small, testable addition);
  the operator manual gains a monitoring section with the alert rules.
- The M5 service definition inherits two requirements: an HTTP `/metrics`
  endpoint and metric registration for the GC worker — recorded here so
  they land in M5's scope rather than being rediscovered.
- Metric names above become a compatibility surface once shipped;
  renames after that need a deprecation window.
- Project-wide: new instrumentation uses the `metrics` facade; exporters
  bind only in binaries, never in library crates.

## References

- [ADR-0023 — storage and vector foundation](../accepted/0023-storage-and-vector-foundation.md)
- [Storage + vector foundation plan — M5](../../plans/active/2026-06-27-storage-vector-foundation.md)
- [Storage garbage collection (design)](../../design/storage-garbage-collection.md)
- [Operating fq-cas (operator manual)](../../guide/operating-fq-cas.md)
