# Project Assessment: 2026-07-05

## Purpose

A point-in-time critical review of the project as a whole, taken at the M2
merge boundary (access control shipped, `627a5ec`). Where the
[April assessment](./2026-04-19-design-assessment.md) reviewed the harness and
tool-isolation *design*, this one reviews the *project*: roadmap position,
strategic sequencing, and where effort is and isn't going. Not an evergreen
document — a snapshot to orient the next planning conversation. Deliberately
honest rather than diplomatic.

## Where we are

**Phase 1 (closed)** delivered the walking skeleton and it holds: the
NATS/JetStream event spine, the suspend/resume reducer harness as the single
execution path, the LLM abstraction, sandboxed tools, the SQLite projection,
trigger dispatch, budgets/pricing, and the `fq` CLI. It also picked up things
it didn't originally promise: a full MCP client, an acceptance harness, a
mock-LLM test harness, and binary distribution (ADR-0022, `install.sh`).

**Phase 2 (active since 2026-04-11)** is
["MCP, Memory, and Skills"](../plans/active/2026-04-11-phase-2-mcp-and-memory.md),
roughly at midpoint:

| # | Scope item | Status |
|---|---|---|
| 1 | MCP client support | Done |
| 2 | Vector DB + embedding infra | In progress — expanded into the [storage + vector foundation plan](../plans/active/2026-06-27-storage-vector-foundation.md); M1 + M2 done, M3–M5 remain |
| 3 | Memory MCP service | Not started (consumes pillar 2) |
| 4 | Skill registry MCP service | Not started (consumes pillar 2) |
| 5 | Context window management | Not started — no compaction code in the runtime |
| 6 | Agent definition extensions | Partial — `mcp:` blocks landed; `skills:` access pending |

Of the phase's nine success criteria, the two MCP ones (#3, #4) are met;
the other seven — memory persistence and search, compaction, skill
discovery/activation/ACL, shared vector infrastructure — all wait on work
not yet started.

**Distance to a hosted deployment** splits in two. Running the current shape
on a server is days of ops work, not code — the daemon, compose file,
Dockerfile, and installer all exist (with one hard caveat: NATS listens on
`0.0.0.0:4222` with no auth and the compose file publishes the port). A
*properly* hosted deployment — remote clients, secured wire, observable — is
blocked on: the API layer (ADR-0006, still draft; clients today talk to NATS
and the projection SQLite directly), token-gated remote exposure of the store
(M5's charter), container isolation (ADR-0010 accepted, unbuilt), the
observability floor (deferred in the [backlog](../plans/backlog.md)), and
scheduled triggers. The API layer is the long pole and has no active plan.

## What the project gets right

- **The verification culture is the standout.** Oracle-gated slices, property
  tests, DST with fault injection and soaks, TLA⁺ where it earned its keep
  (GC), conformance suites reused across backends *and* across the wire, and
  pre-merge multi-dimension reviews with every finding independently
  re-verified. Almost no solo project works this way. The related habit —
  plans as durable records with "decisions taken while building" — preserves
  the *why*, which outlasts the code.
- **The thesis is coherent and the day-one choices match it.** Cost controls
  before the first incident (ADR-0004), the event spine actually being the
  spine, headless-first actually headless.
- **Velocity without quality collapse.** M1c took ~3 days, M2 ~2 days
  including a security review. When a plan has an oracle, the work converges
  fast and correctly.

## Critique

### 1. Nothing measures Q, and M0 has no plan

The north star is a ratio the system never observes. Costs are tracked (half
the denominator); human-time-in and work-value-out aren't, even crudely. More
concretely: M0 — "factor-q can work on factor-q" — is named the bootstrapping
milestone in [VISION.md](../../VISION.md), yet no active plan drives toward
it. The roadmap is *platform-first* (storage → memory → skills → someday
orchestration) rather than *loop-first*. The risk: a beautiful runtime never
used in anger, integration pain discovered late, and priorities set by
architectural completeness rather than by what a running workload needs.

**Suggestion:** define the smallest real dogfood loop — an agent that watches
this repo's CI and triages failures, or a docs-drift checker — and run it
*now*. The pieces exist (MCP, shell/file tools, triggers, budgets). Let its
gaps reorder the milestones. The
[reference workloads](../design/committed/reference-workloads.md) are good touchstones, but
touchstones aren't running loads.

### 2. Pillar 2 grew from a scope item into a platform

The phase 2 plan listed "vector database and embedding infrastructure" with
the engine choice deferred (Qdrant / LanceDB / sqlite-vec). What's being
built is a five-milestone content platform: CDC-chunked CAS,
formally-verified lock-free GC, name index, event-sourced grants, biscuit
tokens, plugin protocols, retrieval stack. Each piece is well-motivated by
the single-binary self-hosted stance, and the execution is excellent — but
three months into the phase, the two services it is named for haven't
started. The engineering isn't the critique; the opportunity cost is.

**Suggestion:** hold M4 to reference-implementations-only (the plan already
gestures this way); resist retrieval sophistication (hybrid, rerankers —
already deferred, keep them there); consider whether Memory's MVP can ship
against M4's search without waiting for M5 polish.

### 3. Rigor concentrates where verification is easiest, not where risk is

Storage got the oracle treatment. The reducer harness — the path every
invocation runs through — has flagged boundary-invariant gaps sitting in the
[backlog](../plans/backlog.md) since mid-May (§ "Reducer boundary
invariants"), and context-window management — the thing that actually kills
long-running autonomous agents — is unstarted with no design. It also
couples to Memory ("store important context before compaction"), so its
absence will be felt exactly when Memory ships.

**Suggestion:** before Memory and compaction are layered onto the harness,
give the reducer-boundary invariants the M1c/M2 treatment: claims, an
oracle, a plan.

### 4. Security effort is inverted relative to today's exposure

M2 built a rigorous capability system for a store with no remote callers —
while agents execute shell behind in-process path checks (ADR-0010's
container isolation is accepted but unbuilt), NATS runs unauthenticated on a
published port, and MCP already gives agents network-reaching tools. Fine
for a single-operator laptop; the point is sequencing, since Memory/Skills
make agents more capable and longer-lived.

**Suggestion:** cheap wins first — bind NATS to localhost in the default
compose and add token auth; then decide when ADR-0010 lands relative to
Memory/Skills.

### 5. Two source-of-truth patterns now coexist

The runtime says "NATS is the source of truth; the projection is rebuildable
by replay" — but `fq-events` has 30-day retention, so rebuildability quietly
expires and the projection becomes the de facto long-term record (the
[archive hand-off](../plans/closed/2026-05-16-archive-hand-off.md) covers
invocation payloads, not the event trail). fq-store's M2 chose the opposite
and sounder pattern: a relational event log as the log of record, NATS as
fan-out behind a durable outbox.

**Suggestion:** make an explicit decision — converge the runtime on the M2
pattern, or document that past 30 days the projection is authoritative —
before more consumers assume replay-from-NATS.

### 6. Design surface is well ahead of implementation, and drift is visible

Twenty-odd design docs, some very ambitious (agent-os-architecture,
signatures-and-optimization-hierarchy, shadow-mode), while
[ARCHITECTURE.md](../../ARCHITECTURE.md)'s status section is still titled
"Implementation status (phase 1)" despite MCP and fq-store having shipped —
and the phase 2 plan has no status section at all. Reconstructing "where are
we?" takes five documents. For a project developed largely through agent
sessions, that cost is paid every session.

**Suggestion:** a one-screen status roll-up (what runs today, links to
active plans, what's next), kept current at milestone boundaries; refresh
the ARCHITECTURE status section; label aspirational design docs as such so
future readers — human or agent — don't mistake them for commitments.

### 7. The draft ADRs are the real frontier

[ADR-0006 (API)](../adrs/accepted/0006-registry-first-api.md),
[ADR-0007 (inter-agent communication)](../adrs/accepted/0007-inter-agent-communication.md),
and [ADR-0008 (extension model)](../adrs/draft/0008-extension-model.md) have
been drafts since the April design push — and the project's one-line
description is "multi-agent systems," which lives or dies on 0007.
Sequencing infrastructure first is defensible, but multi-agent is where the
event schema, budgets, and access control get stress-tested *together*.

**Suggestion:** a thin vertical — two agents, one handoff, over the existing
trigger mechanism — to de-risk 0007 with running code before phase-3 designs
harden on paper.

## What to do next

In priority order, as recommendations (not commitments — sequencing is a
planning conversation):

1. **Stand up the smallest dogfood loop** on this repo and let it pull the
   roadmap (critique 1). Cheapest possible version first.
2. **Finish pillar 2 lean** — M3/M4 reference implementations, Memory MVP as
   early as the search path allows (critique 2).
3. **NATS localhost + token auth** in the default compose (critique 4) — an
   afternoon, closes the one live footgun.
4. **A status roll-up doc** + ARCHITECTURE status refresh (critique 6) —
   cheap, pays rent every agent session.
5. **Reducer-boundary invariants get a plan** before Memory/compaction land
   on the harness (critique 3).
6. **Source-of-truth decision** for the runtime event trail (critique 5).
7. **Two-agent handoff vertical** to de-risk ADR-0007 (critique 7).

## Summary

Engineering quality is unusually high and the discipline is real; the
project's best asset is that when a plan has an oracle, work converges fast
and correctly. The main risk is not technical but strategic: everything so
far optimizes the *platform*, while the north star is a *loop* — and the
loop has no plan, no measurement, and no running instance. There is a usable
irony here: M0 is "factor-q works on factor-q," and today factor-q is built
entirely by other agent harnesses. That gap is the Q-measurement baseline,
sitting right there. Point the oracle habit at the loop itself: state "the
system does X units of useful work on this repo per week unattended" as a
claim, build the smallest thing that makes it checkable, and let that pull
the roadmap.
