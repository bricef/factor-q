# ADR-0021: Cost control for MCP services via `_meta`, and the memory implementation boundary

## Status

Accepted (2026-06-12). Addendum to
[ADR-0013](0013-memory-as-mcp-service.md) (memory as an MCP service);
builds on [ADR-0004](0004-cost-controls-from-day-one.md) (cost controls)
and [ADR-0018](0018-mcp-server-initiated-execution.md) (the runtime is
the sole spend arbiter; advisory-roots precedent).

Implementation: partial — §1 (memory stays an MCP service) stands and is
honoured; **the §2 `_meta` cost protocol is not built**. The string
`factorq.top` appears in no Rust source at all, so the runtime neither
stamps `factorq.top/budget` on outbound tool calls nor reads
`factorq.top/cost` off results. `CostMetadata` is real, but it rides LLM
responses and invocation summaries, never a server-reported tool cost, and
[the MCP guide](../../guide/mcp.md) has no `_meta` section to track. §3's
table is the honest part of the file: the tool-dispatch gate it marks
"(exists)" is the one row that does. §4's deferral has since been half
answered — see the note there.

## Context

[ADR-0013](0013-memory-as-mcp-service.md) decided memory is an MCP
service. Its **leading** rationale was a *forcing function* — "make
memory depend on MCP so the client gets built early." The MCP client is
now built and proven end-to-end against the reference everything server
over both stdio and Streamable HTTP, so that rationale is **spent**: the
decision should stand or fall on its remaining merits, not on
bootstrapping.

Re-evaluated on the merits, MCP still wins for memory — but for reasons
0013 under-weighted, and it surfaced one gap 0013 never addressed:

- **Why MCP still fits.** Memory carries heavy dependencies — an
  embedding model and a vector engine — that we do **not** want compiled
  into the orchestrator binary. Isolating them in a separate process
  (any language; the mature embedding/RAG tooling is Python's) is the
  load-bearing reason now, together with independent backend evolution,
  reuse by any MCP client, and the Phase-2 "shared vector *service*
  consumed by MCP services" shape. Memory is also a *tool-only* MCP shape
  (agent → server), so it rides the ordinary tool-call path — none of the
  server-initiated grant machinery (ADR-0017/0018) applies to it.
- **The gap: cost.** A memory server's semantic search needs embeddings,
  which can be metered spend. [ADR-0004](0004-cost-controls-from-day-one.md)
  requires all spend to be bounded by the invocation budget, attributed,
  and evented. Work done *inside* an MCP server is across a process
  boundary the runtime cannot see into. The agent-facing interface is
  identical whether memory is MCP or native (typed `memory.*` tools,
  ADR-0016), so this — not the interface — is the real question: **is
  there a data path to account for, and bound, cost incurred inside a
  cooperating MCP server?**

MCP's answer is the `_meta` field (SEP-1319): a free-form
`{ }`-shaped object on every request, result, and notification, reserved
for implementation metadata out of band from the tool's semantic payload
(`CallToolResult.structured_content`). The client already writes request
`_meta` today (a progress token on every outbound call).

## Decision

### 1. Memory stays an MCP service (ADR-0013 reaffirmed, rationale refreshed)

Memory remains one or more MCP servers, **for dependency isolation,
polyglot backends, reuse, and the shared-vector-service design** — not
the (retired) forcing-function reason. Working-memory / context-window
management stays in the runtime, unchanged from ADR-0013.

### 2. A bidirectional `_meta` cost protocol (first-party convention)

> **Decided, not built.** Everything in this section is written in the
> present tense and none of it exists — `factorq.top` appears in no Rust
> source. Treat it as the specification to build against, not as a
> convention you can already speak.

The runtime and a *cooperating* (first-party) MCP server exchange cost
information through `_meta`, namespaced under `factorq.top/` — a domain
the project controls. (MCP reserves bare and `modelcontextprotocol.io/`
keys and asks implementations to prefix theirs with a dot-labelled
namespace they own, then `/`.)

- **Outbound — budget hint** on `CallToolRequestParams._meta`:

  ```jsonc
  "_meta": { "factorq.top/budget": { "remaining_usd": 0.042, "model": "<embed-model>" } }
  ```

  The runtime stamps every outbound tool call with the invocation's
  remaining budget. A well-behaved server self-limits under the ceiling.

- **Inbound — cost report** on `CallToolResult._meta`:

  ```jsonc
  "_meta": { "factorq.top/cost": { "usd": 0.0008, "tokens": 1200, "model": "<embed-model>" } }
  ```

  The runtime reads it and folds it into the invocation's cost the same
  way sampling does — a `CostMetadata` on the `tool.result` event,
  analogous to the one on `llm.response` — so it counts against `budget`
  and shows up in `fq costs`.

Together: *hint → server self-limits → report → runtime deducts → updated
hint on the next call.*

### 3. Control model: advisory hint, hard backstops

The budget hint is **advisory**, exactly like roots (ADR-0018): it tells
a cooperating server its intended ceiling; it is not an enforcement wall.
The runtime keeps these **hard** controls regardless of cooperation:

| Control | Strength | Mechanism |
|---|---|---|
| Refuse the call when over budget | **hard, pre-spend** | the runtime's tool-dispatch gate (exists) — at tool-call granularity, not sub-operation |
| Cap spend *within* a call | soft (cooperative) | `factorq.top/budget` request `_meta` |
| Account for what was spent | hard *if the server reports* | `factorq.top/cost` result `_meta` |
| Abort a running call | hard | `call_tool_cancellable` → `notifications/cancelled` |
| True sub-call enforcement | hard | only when the runtime **owns** the spend (see §4) |

MCP has **no primitive for a server to delegate a costly op back to the
client** (sampling is text-completion only — there is no "ask the client
to embed"). So this `_meta` hint+report loop *is* the MCP-native way to
budget across the process boundary; the only stronger option is the
runtime owning the spend.

### 4. Embedding boundary: deferred to the storage / memory design

The embedding boundary — local model vs metered API, embed at write vs
query time, where vectors live — is **not decided here.** Memory is
expected to layer on a more primitive **content-addressed storage**
service that is yet to be defined, and the embedding / indexing strategy
belongs to *that* design; picking it now would over-commit ahead of the
storage architecture.

> **Half answered since.** The content-addressed storage service was
> defined 15 days later and then built:
> [ADR-0023](0023-storage-and-vector-foundation.md) and
> [ADR-0024](0024-separate-databases-storage-foundation.md) settle the
> storage foundation, and `fq-store` ships the CAS. What ADR-0023 resolves
> of the three questions above is only *where vectors live* (F9,
> sqlite-vec) and the plugin protocol (F7). **Local-vs-metered and
> embed-at-write-vs-query-time remain open** — and they are exactly the two
> that decide whether the §2 protocol is a no-op or load-bearing. The
> embedding half is also still unbuilt: ADR-0023's layer 3 does not exist.

What this ADR guarantees is that whichever boundary that design lands on
is **cost-accountable and budgetable** through §2/§3:

- **No metered spend** (e.g. a local embedding model) → the cost protocol
  is a no-op and isolation is maximal.
- **Metered embedding API** → either the runtime owns the embed
  (cost-gated, handing the server a pre-computed vector) or the server
  reports via `factorq.top/cost` and is bounded post-hoc plus by the
  don't-dispatch gate.

The storage / memory plan settles this; the `_meta` mechanism here keeps
the choice cost-safe either way.

### 5. Scope: first-party only

This is a **first-party convention**. Third-party MCP servers are black
boxes: the runtime accounts only for spend **it** owns (LLM calls,
runtime-owned embeds) or that a cooperating server **reports**. We make
no claim of metering arbitrary servers, and an uncooperative server's
internal spend is simply invisible — by the nature of the protocol, not a
factor-q limitation.

## Consequences

- A small, documented `_meta` vocabulary — `factorq.top/budget` (out),
  `factorq.top/cost` (in) — that the runtime writes on outbound tool calls
  and reads on results, plus a `CostMetadata` on `tool.result` when a
  cost is reported. (Reuses the `Meta`/`CostMetadata` types already in
  the codebase; the outbound `_meta` writer already exists for progress
  tokens.) **Not built** — the vocabulary was never written and neither
  reader nor writer exists. The reuse premise still holds, which is why
  this remains cheap; but nothing in the codebase speaks it today
- Memory — and any first-party cost-bearing MCP service — is **budget-
  aware and accounted without being native**, which removes cost as a
  reason to pull memory into the runtime.
- The memory plan, when written, builds on this: a content-addressed
  storage primitive with a vector index over it, a memory MCP server
  speaking the `factorq.top/*` `_meta` convention, and `fq`-side wiring
  that stamps the budget hint and folds the cost report into the event
  bus + budget.
- Guides would track the live `_meta` convention, leaving this ADR as the
  point-in-time rationale. They do not, because there is no convention to
  track: [the MCP guide](../../guide/mcp.md) has no `_meta` section. Until
  §2 is built, this ADR is the only description of it.
