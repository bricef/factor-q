# Operator dashboard — a standalone read-only visibility surface

**Status:** executed through layer 3 (2026-07-12) — `views`, the CLI-as-formatter refactor, the in-daemon tarpc read service, and the `fq-dashboard` binary are all landed on #105; the ADR-0006 read-half note is recorded. Remaining: deploy-managing the dashboard process. A web dashboard for *operator* visibility — process state and health metrics — over the running `fq` daemon. Deliberately not a business/product dashboard: there is **no Q, cost-as-leverage, or work-out reporting here** (that belongs with the [M0 close-the-loop](2026-07-05-m0-close-the-loop.md) metric work, not an ops tool). Grounded in the [2026-07-05 project assessment](../../reviews/2026-07-05-project-assessment.md) (the API layer is "the long pole [with] no active plan"; the observability floor is unbuilt) and lands a first, narrow read-half of [ADR-0006](../../adrs/draft/0006-api-design.md).

## Why this, why now

Today the only way to see what the runtime is doing is the CLI (`fq status / costs / events / workers / invocation show`). Every one of those reads is inline SQL plus text formatting living *inside the CLI binary*, reaching directly into the projection and control-plane stores. There is no programmatic read surface and no at-a-glance operator view: "is the daemon healthy, is the JetStream consumer keeping up, what invocations are in flight, what is each one doing right now, what am I spending per agent?" are all answerable only by running several commands and reading prose.

An operator dashboard is the smallest honest forcing function for the read-half of the runtime API. It exercises the typed read surface end to end against a real consumer, without committing to the streaming/write questions ADR-0006 still holds open, and it surfaces a first slice of the observability floor the assessment flags as missing.

## Non-goals (explicit)

- **No Q / leverage / work-out metrics.** This is an ops tool. The Q numerator does not exist anywhere yet and defining it is M0's job, not this dashboard's. A dashboard that fabricated a Q number would be worse than none.
- **No writes, no control.** Read-only. No start/stop/pause of agents or the runtime. Administrative controls are a later, authenticated surface.
- **No live streaming (v0).** Polling only (see below). SSE/WebSocket event tailing is deferred — it is the part of ADR-0006 this plan deliberately does not answer.
- **No multi-agent trace/causality graph (v0).** The projection does not store `parent_event_id`/`trace_id` today; the parent/child chain is not queryable without new projection columns. Single-invocation timelines are in scope; cross-invocation graphs are not.
- **No auth (v0).** Localhost-only, consistent with the current NATS and `fq-cas serve` posture. See Security posture.

## Architecture: three layers, three crash domains

The design assumes the runtime is **not reachable from outside the host**. Only the dashboard's HTTP port is ever exposed (via SSH tunnel or similar); the runtime's own surface stays strictly localhost.

```
browser ──HTTP/HTML──▶ fq-dashboard ──tarpc/bincode(localhost)──▶ fq run (daemon)
   (meta-refresh)      (standalone bin)     (typed read RPC)      (tarpc read service)
                                                                         │
                                                                views (Serialize DTOs)
                                                                         │
                                                          one SQLite handle, ?mode=ro
                                                          + lifted report_stream probe
```

### Layer 1 — `fq-runtime::views` (the backbone)

A new library module: `Serialize` DTOs plus query functions over the single read-only SQLite handle (projection + control-plane + worker WAL are all one file, opened `?mode=ro`), plus the JetStream health probe (`report_stream`: stream depth, consumer lag, ack/num-pending) **lifted out of the CLI into a reusable helper**. The name reflects what they are: typed read-only *views* over the runtime's stores (`state` is avoided — it collides with `invocation_state`/`state_blob`).

This is the load-bearing refactor. Today the CLI *is* the read layer, which is the "clients touch the store directly" anti-pattern the assessment flags, wearing a CLI hat. After extraction: the CLI becomes a formatter over `views`, the tarpc service becomes a thin wire-mirror of it, and there is one source of read logic. `DoctorReport` (already `#[derive(Serialize)]` with a `--json` path) is the proof this factoring is already wanted and the template to follow. The CLI gets `--json` on its read commands for free as a side effect.

### Layer 2 — tarpc read service in the daemon

The runtime exposes its reads as a `#[tarpc::service]` trait, mirroring the `views` DTOs over bincode-on-localhost — the **same discipline `fq-store` already uses** for `CasService` (`services/fq-store/src/service.rs`): a wire trait, a `WireError` for serializable errors, a handler forwarding to the backing library. This is the first tarpc surface in `fq-runtime` (tarpc lives only in `fq-store` today, behind a `service` feature); the pattern is proven next door and copyable.

**Why tarpc internally rather than the daemon speaking HTTP directly:**

- **Typed, compiler-checked contract.** The `views` DTOs flow straight over bincode. No JSON schema to keep in sync; a trait change fails the dashboard's compile rather than drifting silently.
- **Clean decoupling.** The runtime exposes *what it knows* (typed domain reads). The dashboard owns *what the browser needs* (HTTP shape, pagination, view-models, refresh). Browser-facing shape can churn in the dashboard without touching the daemon.
- **Smaller in-daemon surface = better isolation.** The read service runs in the daemon (so it shares the daemon's one NATS connection for the health probe and its `?mode=ro` handle), but as a small read-only bincode handler on its own tokio task — not a full HTTP router. All the churny, higher-risk, browser-shaped code lives in the *separate* dashboard process. Being in-daemon shares the process; read-only `?mode=ro` rules out corruption, and an isolated task + a timeout on the JetStream probe + a feature flag bound the blast radius. The store already proves a localhost tarpc server co-resident with its owner is fine.

### Layer 3 — `fq-dashboard` (standalone binary)

A new crate (`services/fq-dashboard`). Its own crash domain: it holds only a tarpc client and an HTTP server. It **cannot touch runtime state and cannot take the runtime down** — if it panics, the daemon never notices; if the daemon is unreachable, the dashboard renders "runtime unreachable, last seen Ns ago" rather than breaking.

v0 is deliberately naive: on each browser request the binary calls the tarpc read service, renders a static HTML page, and the browser auto-refreshes via `<meta http-equiv="refresh" content="N">`. Zero client-side JS, zero CORS (browser talks only to the dashboard), zero framework. This BFF shape (rather than the browser fetching the runtime API directly) is what keeps the runtime surface strictly localhost: only the co-located dashboard calls it, and the operator tunnels to one port.

The dashboard needs **no NATS connection** — health (including JetStream lag/backlog) arrives through the tarpc service, which runs the probe internally. Giving the dashboard its own NATS client would duplicate the probe and add a failure coupling; explicit non-choice for v0.

## v0 surface

All of these map onto data that exists today — no new projection columns, no new instrumentation. Each tarpc method returns a `views` DTO; the HTTP routes are the dashboard's own shape over them.

| Concern | tarpc method (illustrative) | Backed by |
|---|---|---|
| **Health** | `health() -> HealthReport` | `DoctorReport` + lifted `report_stream`: daemon up, per-stream depth, consumer lag, ack/num-pending, ambiguous-invocation and stale-worker counts |
| **Workers** | `workers() -> Vec<WorkerRow>` | `coordination_worker` roster + liveness/staleness |
| **Invocations (list)** | `invocations(filter) -> Vec<InvocationSummary>` | `coordination_invocation_owner` + `invocation_archive` |
| **Invocation (detail)** | `invocation(id) -> InvocationDetail` | worker WAL: `invocation_state` phase/step_index + `tool_dispatch` + `llm_dispatch` — the "what is it doing right now" view, the standout and free |
| **Events (recent)** | `events(filter) -> Vec<EventRow>` | projection `events` table (agent / type / since / limit) |
| **Costs (per-agent)** | `costs(filter) -> Vec<CostSummary>` | projection `cost_summary` — per-agent cost/token aggregate, for operator spend-watch (not Q) |

Note the limits, stated so they are recorded decisions rather than omissions: costs are **per-agent only** (per-invocation/per-model breakdowns need new projection columns — deferred); event tail is **backfill-from-projection**, not live (the current `events tail` is live-only core NATS with no replay — v0 polls the projection instead).

## Security posture (v0)

Localhost bind, no auth, on both the tarpc service and the dashboard's HTTP port — consistent with the current runtime posture (NATS on localhost unauthenticated; `fq-cas serve` localhost-only until M5). The assumption is a single operator on the host, reaching the dashboard via SSH tunnel. Stated here so it is a decision, not a gap. Remote exposure and auth are out of scope and gated on the same broader work as NATS auth and store token-gating.

## Sequencing

1. **Extract `fq-runtime::views`** — DTOs + queries over the `?mode=ro` handle, lift `report_stream` into a helper. Refactor the CLI read commands onto it (they get `--json`). This is the backbone and the largest single piece; it has standalone value even before any dashboard exists.
2. **tarpc read service in the daemon** — wire trait mirroring the DTOs, `WireError`, handler, feature-flagged, localhost bind, probe timeout, own task.
3. **`fq-dashboard` binary** — tarpc client + static HTML render + `<meta refresh>`; health page first, then invocations list + detail, then events + costs.
4. **Note on ADR-0006** — record that the read-half is answered by "typed tarpc internally + purpose-built HTTP adapters at each edge", and that this is proposed as the shape for the API generally (it dissolves 0006's one-protocol-for-all-clients dilemma and matches headless-first). The streaming and write halves remain open in the ADR.

## Decisions taken

- **tarpc internal, HTTP at the edge** — over a direct HTTP endpoint on the daemon, for the typed contract, the decoupling, and the smaller in-daemon surface. (Discussed 2026-07-10.)
- **Read service in-daemon** — over a separate reader process, because tarpc shrinks the in-daemon surface enough that the shared-process risk is bounded and acceptable, and it avoids duplicating the DB handle and NATS probe outside the runtime.
- **Dashboard is a standalone binary and a BFF** — separate crash domain (hard requirement: it must be able to crash without affecting the runtime), and BFF rather than browser-direct so the runtime surface stays strictly localhost.
- **Polling, not streaming (v0)** — a naive `<meta refresh>` page is sufficient for an operator tool and defers the streaming half of ADR-0006 entirely. **Superseded for the transcript page (2026-07-13):** the live transcript streams over SSE — datastar (v1.0.0, vendored, MIT, served from the dashboard binary) patches new turns in place, fed by cursor-indexed `transcript_since` reads over tarpc at 1s; the run's `Outcome` entry closes the stream, and no-JS browsers fall back to a `<noscript>` meta-refresh. Every other page keeps plain meta-refresh polling.
- **No Q, no writes, no auth, no multi-agent trace (v0)** — see Non-goals; each is a deliberate cut, not an oversight.

## Open questions

- Refresh interval and whether it should be per-page (health faster than costs) — trivial, decide during build.
- Whether the tarpc service reuses `fq-store`'s framing/setup conventions verbatim or the runtime grows its own thin `service` feature mirroring the store's — prefer verbatim reuse of the discipline.
- Exact `HealthReport` shape vs. the existing `DoctorReport` — likely `HealthReport` is `DoctorReport` plus the stream/consumer probe fields; reconcile during layer 1.
