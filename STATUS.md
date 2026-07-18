# Status

One screen: what runs today, where we are, what's next. Updated at
milestone boundaries — **last: 2026-07-05** (M2 access control merged,
`627a5ec`). If this contradicts `git log`, trust the log and fix this file.

## What runs today

- **Runtime (`fq`)** — a persistent daemon (`fq run`: event projection +
  trigger dispatcher over NATS/JetStream). Agents are Markdown definitions
  executed through the suspend/resume [reducer harness](docs/guide/reducer-harness.md);
  per-agent model selection, budget enforcement after every LLM call,
  sandboxed built-in tools (`file_read`, `file_write`, `exec`,
  `self_inspect`). Full [MCP client](docs/guide/mcp.md) (spec 2025-11-25):
  stdio + Streamable HTTP transports; tools, resources, prompts, and the
  server-initiated capabilities (sampling, elicitation, roots). Operator
  surface: `fq init / run / trigger / reload / down / agent / invocation`
  (including `transcript`) `/ events / costs / status / workers /
  dead-letters / doctor` (read commands take `--json`), plus a read-only
  web dashboard (`fq-dashboard` over the daemon's localhost tarpc read
  service — the
  [operator-dashboard plan](docs/plans/active/2026-07-10-operator-dashboard.md)).
- **Store (`fq-cas`)** — [content-addressed storage](services/fq-store/README.md)
  (BLAKE3, FastCDC dedup) + named objects with version history + verified
  online GC + [access control](docs/guide/access-control.md) (event-sourced
  grants with delegation/revocation, biscuit capability tokens, default-deny
  gate). Library + CLI (`put/get/object/gc/grant/token`). `fq-cas serve` is
  localhost-only and unauthenticated until M5.
- **Infra** — NATS via `infrastructure/docker-compose.yml` (localhost;
  **no auth — don't expose the port beyond the host**). Build from source
  (`just up`, see [Quickstart](QUICKSTART.md)); `install.sh` awaits the
  first release.

## Where we are

Phase 1 (the walking skeleton) is
[closed](docs/plans/closed/2026-04-02-phase-1-foundation.md).
[Phase 2](docs/plans/active/2026-04-11-phase-2-mcp-and-memory.md) — MCP,
memory, and skills — is at its midpoint:

| Phase 2 pillar | State |
|---|---|
| 1. MCP client | **Done** |
| 2. Storage + vector foundation | **In progress** — M1 (CAS/index/GC) and M2 (access control) done; M3 (extraction) → M4 (embedding + retrieval) → M5 (service wiring + SDK) remain |
| 3. Memory service | Not started (consumes M4/M5) |
| 4. Skill registry | Not started (consumes M4/M5) |
| 5. Context window management | Not started |
| 6. Agent-definition extensions | `mcp:` done; `skills:` pending |

## What's next

M3, then M4, then M5, per the
[storage + vector foundation plan](docs/plans/active/2026-06-27-storage-vector-foundation.md);
Memory and Skills MVPs build on the result. On the runtime side the
[reducer verification plan](docs/plans/closed/2026-07-05-reducer-verification.md)
is **complete** (claims R1–R7 all oracle-backed in the hermetic CI
path: trace oracle, state validation, sim world, resume equivalence,
crash DST, budget properties, soak — seven real bugs found and fixed
by it; `just soak` scales the lifecycle driver for deep local runs).
The v0 dogfood loop is **live**: a `doc-drift` agent reviews this repo's
commits daily on the local runtime (project at `~/fq-dogfood`, outside
the repo) — its findings feed the
[backlog](docs/plans/backlog.md). That loop is read-and-report; the
loop-first [M0 "close the loop" plan](docs/plans/active/2026-07-05-m0-close-the-loop.md)
drives the next step — an agent that lands validated PRs against this
repo — with a crude proxy-metric set (read relative to an expert+frontier
baseline) to keep the Q ladder honest. Open strategic questions
(security sequencing, the API layer) are in the
[2026-07-05 project assessment](docs/reviews/2026-07-05-project-assessment.md).

## Not built yet

API layer (ADR-0006, draft) · multi-agent orchestration (ADR-0007, draft) ·
memory + skills services · context compaction · container isolation
(ADR-0010, accepted but unbuilt) · scheduled triggers · observability floor
(JSON logs, metrics, alerting) · NATS auth · tagged binary releases (the
rolling `main-latest` deploy channel is built — see
[ops/dogfood](ops/dogfood/README.md) — but no `v*` release has shipped).

## Pointers

[Quickstart](QUICKSTART.md) · [Architecture](ARCHITECTURE.md) ·
[Vision](VISION.md) · [Active plans](docs/plans/active/) ·
[Backlog](docs/plans/backlog.md) · [ADRs](docs/adrs/) ·
[Guides](docs/guide/)
