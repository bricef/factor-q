# Reference Workloads

Concrete agent workloads we use as touchstones when reasoning
about factor-q's architectural surface. Each is a real system
(internal or external) with a real operational profile, not a
strawman. Together they probe what factor-q-at-Q200 will
actually have to host.

These workloads are *not* test fixtures or benchmarks. They're
reference shapes — when a design decision is on the table
("does this support multi-MB tool outputs?", "does this break
under 50 concurrent invocations?"), the question to ask is
"what does this look like for Canopy / TradingAgents?".

For factor-q's high-level use-case categories see
[`VISION.md` § Target Use Cases](../../VISION.md). The
workloads below are concrete instances within those categories.

## Canopy

Regulatory intelligence platform monitoring US federal, US
state, UK, and EU education / children's-data law. EdTech
vendors and school districts subscribe; daily digests arrive
filtered by jurisdiction, role, and product profile.

External brief: [`../../Canopy/docs/product/canopy_brief.md`](../../Canopy/docs/product/canopy_brief.md)
(separate repo, internal product planning).

### Profile

| Dimension | Value |
|---|---|
| **Purpose** | Regulatory monitoring + interpretation + tailored digest delivery |
| **Topology** | Pipeline: scrape → change-detect → classify → two-stage LLM analysis (cheap → frontier) → customer profile match → digest |
| **Runtime model** | Continuous + scheduled. Source scrapers run on cron; per-customer digests fire on a delivery cadence. Long-lived workloads, not request-response. |
| **I/O** | Reads: federal/state legislative trackers, regulatory body publications. Writes: digest emails (transactional service), database for customer profiles + history. |
| **Invocation profile** | Per source: scrape → classify → LLM analyse. Burst-fanout when many sources change simultaneously (legislative session, regulatory rule cycle). At target scale (25-120 customers, 50+ jurisdictions): 100-200 active invocations during fanout windows; dozens steady-state. Working set on disk: scraped content + change diffs + drafts, 10-50 GB sustained. |
| **Cost shape** | $5-12 per customer per month variable cost (per the brief). Two-tier LLM model: GPT-4o-mini-class for extraction, frontier for synthesis. |

### What it stresses

- **Multi-project-parallel** at scale. Canopy is one of several
  workloads a single factor-q instance will host. The 100-200
  active-invocation profile during fanout informs sizing.
- **Long-running scheduled workloads** with state that
  persists across runs (source change-detection diffs).
- **Two-tier model routing per agent** (cheap and frontier).
  Validates per-agent model configuration.
- **Workspace-as-critical-state** at multi-GB sizes.
  Validates per-invocation workspace dir as a separate
  storage layer (data-architecture.md §3.3).
- **Cost accounting per customer** rolled up from
  per-invocation cost. Validates the existing cost-event
  + projection rollup model.

### What it doesn't probe

- Sub-second latency requirements.
- Cross-customer workspace sharing.
- Real-time human-in-the-loop interaction (the QA review is
  asynchronous, not blocking).

## TradingAgents

Multi-agent LangGraph DAG that simulates a trading firm.
Per-ticker per-date `propagate(ticker, date)` runs end-to-end
and emits a buy/sell/hold decision against a simulated
exchange. Research artefact, not live trading.

External: [github.com/TauricResearch/TradingAgents](https://github.com/TauricResearch/TradingAgents).

### Profile

| Dimension | Value |
|---|---|
| **Purpose** | Per-ticker trading-decision pipeline (research/backtest, not live trading) |
| **Topology** | Static DAG with debate sub-loop: 4-wide analyst fanout (Fundamentals / Sentiment / News / Technical) → 2-agent adversarial debate (Bull vs Bear, bounded rounds) → trader synthesis → risk team → portfolio manager approval |
| **Runtime model** | On-demand batch. One invocation = `propagate(ticker, date)`. No streaming, no continuous loop. Backtesting is repeated invocations across dates. |
| **I/O** | Reads: market data (Alpha Vantage, technical indicators), news feeds, social sentiment. Writes: append-only `~/.tradingagents/memory/trading_memory.md` (decision log, fed back into PM context); per-ticker SQLite checkpoint at `~/.tradingagents/cache/checkpoints/<TICKER>.db`. Multi-provider LLMs (10+: OpenAI, Anthropic, Google, xAI, DeepSeek, Qwen, GLM, Ollama, Azure, OpenRouter). Two tiers: `deep_think_llm` for debate/synthesis, `quick_think_llm` for analyst sub-tasks. |
| **Invocation profile** | Long. **Minutes per invocation**, dominated by sequential LLM calls plus debate rounds (each round = 2 LLM calls × N rounds). Analyst stage is 4-wide parallel; debate, trader, risk, PM are serial. Per-run token cost is large (multi-step reasoning over multi-page reports). |
| **Concurrency model** | Within an invocation: bounded fanout. Across tickers: caller's responsibility (run multiple `propagate`s in parallel). |

### What it stresses

- **Long-running invocations with mid-flight crash recovery
  value.** A 5-minute invocation that crashes during the
  debate has real recovery cost. The WAL contract
  (data-architecture.md §3.1) earns its weight here.
- **Parallel fanout within an invocation**, mapped to
  `NextAction::CallToolsParallel` in the reducer model.
  4-wide analyst dispatch is the simplest case.
- **Bounded debate sub-loops**, mapped to a state-enum
  inner loop. Validates that the reducer's state machine
  composes — a debate round is just a new phase variant
  with an iteration counter.
- **Cross-run shared memory.** The `trading_memory.md`
  pattern (append-only journal, fed back as context in
  future runs) is the load-bearing case for memory
  MCP services per [ADR-0013](../adrs/accepted/0013-memory-as-mcp-services.md).
  When designing memory MCP servers, "append-only journal
  that prior runs read" is a primary pattern.
- **Heavy multi-provider LLM use.** 10+ providers, two
  tiers per invocation. Validates per-agent model + provider
  configuration, separate API key budgeting per project
  (data-architecture.md §3.3).
- **Multi-project parallelism** at the caller level.
  Running 50 tickers concurrently = 50 active factor-q
  invocations, exactly the Q200 multi-project profile.

### What it doesn't probe

- Continuous reactive workloads (TradingAgents is batch).
- Sub-second latencies.
- Tool outputs in the multi-MB range (analyst reports are
  KB to low-MB).

## What's not yet exercised by either workload

Both Canopy and TradingAgents are **batch / scheduled
multi-agent pipelines with minutes-scale invocations and
KB-to-MB workspace state**. Surfaces neither workload
stresses:

- **Continuous reactive agents** — event-driven workers
  with no fixed end (operations alerting, real-time
  monitoring). The `automated systems operations` use case
  in `VISION.md` falls here. No reference workload yet
  captures this shape; testing it will probably require a
  third reference, e.g. an internal ops-bot.
- **Low-latency sub-second invocations** — agents called
  inline as part of a synchronous user request. factor-q is
  not currently sized for this profile and the architecture
  (NATS publish per event, sync WAL writes) carries
  overhead that breaks sub-100ms budgets.
- **Multi-GB tool output per call** — agents that read
  whole repositories, generate large datasets, etc. The
  current `STATE_BLOB_WARN_THRESHOLD_BYTES = 10 MB`
  threshold suggests 10s of MB is the comfort zone; beyond
  that, the inline-blob assumption needs revisiting (see
  data-architecture.md §6).
- **Agents-spawning-agents** at runtime — the reducer has
  no native concept of recursive sub-invocations. ADRs and
  the agent OS architecture flag this as future work but
  no concrete reference workload requires it yet.

When a third reference workload is added (or the operator
tries to run something that doesn't fit the existing two
shapes), update the gaps section above. The point of these
workloads is to make the design space tangible; if the
gap section grows, that's a signal to widen the
architectural discussion before the surprise arrives in
production.

## Cross-references

- [`VISION.md`](../../VISION.md) — high-level use case categories
- [`data-architecture.md`](./data-architecture.md) — the storage / persistence design these workloads stress-test
- [`tool-isolation-model.md`](./tool-isolation-model.md) — workspace and sandbox model
- [`agent-os-architecture.md`](./agent-os-architecture.md) — the broader runtime architecture
- [`ADR-0013`](../adrs/accepted/0013-memory-as-mcp-services.md) — memory-as-MCP-services (TradingAgents' `trading_memory.md` is a primary use case)
