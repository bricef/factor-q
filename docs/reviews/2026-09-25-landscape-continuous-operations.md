# Landscape scan: continuous operations — 2026-09-25

Taken against `main` @ `919a8d4` (2026-09-25). A web survey of the
systems that share factor-q's focus on *running* ongoing agent processes
rather than on the build-and-develop part of the lifecycle. It is a
snapshot: product names, features and positioning in this space move
every few months, so treat it as dated evidence, not a current map. The
durable claim it supports lives in [`VISION.md`](../../VISION.md#core-thesis).

## The observation

Most agent harnesses and meta-harnesses — Claude Code, OpenCode, Crush
(see the analyses under [`research/`](../../research/)) and the
orchestrators built on top of them — are shaped around a *session*: a
human opens it, the agent does a piece of build or development work, the
session ends. Their lifecycle stops at "the change is made".

factor-q's focus is the other side of that line: processes that never
finish. Triggers arrive on their own schedule, work is suspended and
resumed across restarts, spend is bounded over weeks rather than per
prompt, and a human is one input among many rather than the loop driver.
The question the scan asks is: who else is building for that?

## What exists

No surveyed system combines all of factor-q's properties, but five
clusters overlap with parts of the thesis.

### 1. Ambient / event-triggered agent frameworks

Closest in spirit: agents triggered by events rather than prompts, with
humans pulled in at decision points.

- **Google ADK ambient agents** — an `EventSource` produces
  `TriggerEvent`s and the agent runs per event; triggers are HTTP routes
  on the ADK server, with Cloud Run as the recommended host. ADK also
  added pause/resume for long-running agents.
- **LangGraph / LangGraph Platform** — LangChain's "ambient agents"
  framing and the Agent Inbox pattern for human-in-the-loop, backed by a
  persistence layer that checkpoints full graph state.

Where they differ: libraries plus a managed runtime on someone else's
cloud. Cost control, self-hosting and operator tooling are not the
centre of gravity.

### 2. Durable-execution engines

The infrastructure layer for "survive crashes, pause for days, resume
exactly where you were".

- **Temporal** — the most mature engine; strong replay semantics, heavy
  programming model.
- **Restate** — explicitly positions itself as durable infrastructure for
  AI agents ("the agent stays ordinary code, reliability lives
  underneath").
- **Inngest**, **DBOS**, **Hatchet** — lighter, event-driven or
  serverless-friendly variants.
- Cloud primitives: **Cloudflare Workflows** (GA), **AWS Durable
  Functions**, **Vercel Workflow DevKit**.

Where they differ: general workflow infrastructure. The agent layer,
budgets, provider throttling and the operator surface are yours to
build. factor-q's reducer harness and event bus occupy this layer and the
agent layer above it.

### 3. Stateful agent servers

- **Letta** (formerly MemGPT) — an open-source server where each agent is
  a persistent identity with self-edited memory blocks, all state in a
  database, reachable over REST and inspectable in its ADE.

Where it differs: the unit is a long-lived *agent with memory*, not a
supervised *process* of many agents with triggers and budgets.

### 4. Managed agent runtimes

- **AWS Bedrock AgentCore**, **Google Vertex AI Agent Engine**, **Azure
  AI Foundry Agent Service**, **OpenAI Agents API** (durable sessions,
  hosted or external execution).

Where they differ: hosted, multi-tenant, and typically tied to one cloud
or model family. factor-q is single-tenant, self-hosted and
model-agnostic by design ([`VISION.md`](../../VISION.md#design-principles)).

### 5. Operations-domain products ("AI SRE")

Continuous monitoring, investigation and supervised remediation — the
market form of VISION's "automated systems operations" use case. Gartner
treated AI SRE as its own category in a January 2026 market guide.

- **Resolve AI**, **Datadog Bits AI SRE**, **Azure SRE Agent**,
  **PagerDuty SRE Agent**, **incident.io AI SRE**, **Cleric**,
  **Traversal**, **New Relic Agentic Platform**, **Dynatrace**.

Where they differ: finished vertical products for one domain, not a
general runtime. Their graduated-trust model (read-only, then
approval-gated, then autonomous remediation) is a useful reference for
factor-q's own human-in-the-loop design.

## Takeaways

- **The market is converging on the thesis.** A 2026 LangChain survey
  reports 71% of teams with agents in production run at least one around
  the clock, up from 28% in 2024. "We do always-on" is no longer a
  differentiator on its own.
- **The distinction is the combination.** factor-q pairs a self-hosted,
  model-agnostic runtime with a replayable event bus, cost as a
  first-order safety concern, and an operator surface. It sits between
  the durable-execution engines (infrastructure, no agent layer) and the
  ambient-agent frameworks (agent layer, someone else's cloud).
- **"Harnesses stop at build" holds for coding harnesses**, not for
  LangGraph or ADK. Positioning text should say which it means.

## Sources

- [Orca Security — Best AI Agent Runtime Tools & Platforms 2026](https://orca.security/resources/blog/best-ai-agent-runtime-tools-platforms/)
- [MoClaw — Always-On AI Agent: What 24x7 Actually Means in 2026](https://moclaw.ai/blog/always-on-ai-agent-2026)
- [Edge of Context — Long-Running AI Agent Runtime in 2026](https://slavadubrov.github.io/blog/2026/05/26/ai-agent-runtime/)
- [Addy Osmani — Long-running Agents](https://addyosmani.com/blog/long-running-agents/)
- [Inngest — Durable Execution: The Key to Harnessing AI Agents in Production](https://www.inngest.com/blog/durable-execution-key-to-harnessing-ai-agents)
- [Comuvia — Durable Execution for AI Agents in 2026](https://comuvia.ai/articles/durable-execution-for-ai-agents-temporal-vs-inngest-vs-restate-vs-prefect)
- [Reactify — Durable AI agents in 2026](https://www.reactify-solutions.com/articles/durable-ai-agents-2026)
- [CloudRPS — Durable Execution Beyond Temporal](https://cloudrps.com/blog/durable-execution-restate-dbos-hatchet-beyond-temporal/)
- [Google ADK — Ambient Agents](https://adk.dev/runtime/ambient-agents/)
- [Google Developers Blog — Long-running agents with ADK](https://developers.googleblog.com/build-long-running-ai-agents-that-pause-resume-and-never-lose-context-with-adk/)
- [Sequoia — Ambient Agents and the Agent Inbox (Harrison Chase)](https://sequoiacap.com/podcast/training-data-harrison-chase-2)
- [AWS — AgentWatch: proactive AWS monitoring with ambient agents](https://aws.amazon.com/blogs/machine-learning/agentwatch-proactive-aws-monitoring-with-ambient-agents/)
- [Letta docs — Stateful agents](https://docs.letta.com/guides/core-concepts/stateful-agents)
- [letta-ai/letta](https://github.com/letta-ai/letta)
- [Vibranium Labs — Top AI SRE Agents 2026](https://vibraniumlabs.ai/blog/top-ai-sre-agents)
- [Augment Code — AI SRE: the 2026 guide](https://www.augmentcode.com/guides/ai-sre-ai-powered-site-reliability-engineering)
- [IT Brief — New Relic unveils agentic AI platform for SRE](https://itbrief.com.au/story/new-relic-unveils-agentic-ai-platform-for-sre-automation)
