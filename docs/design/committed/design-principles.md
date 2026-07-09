# factor-q Design Principles

## Purpose

This document is the working list of design principles that guide decisions across the factor-q project. It is distinct from [VISION.md](../../../VISION.md) (*what* factor-q is and *why* it exists) and [ARCHITECTURE.md](../../../ARCHITECTURE.md) (*what* subsystems compose it and *how* they fit together). Principles are the *how we decide* layer — rules that should apply to a design choice regardless of which subsystem is in play.

Principles are testable against new design decisions: when presented with a choice, we should be able to ask "does this honour the principles?" and get a clear answer. When a proposal appears to violate a principle, the resolution is either to change the proposal or to consciously revise the principle — never to ignore it silently.

The list is expected to grow. Principles have emerged from specific design discussions and will continue to. A principle that is repeatedly invoked and never challenged graduates from "current working principle" to "settled bedrock"; one that is repeatedly inconvenient without clear benefit is a candidate for revision. This is a living document.

---

## Principles

### 1. LLMs are first-class users and a source of requirements

An LLM participating in a factor-q workflow is not merely an executor of tasks — it is a user of the system, with preferences, constraints, and feedback worth gathering. The ergonomics, clarity, and affordances of factor-q's tools are evaluated in part by how well they serve the models that call them, not only by how well they serve the humans who write the code.

This rests on a practical observation: LLMs have structural ergonomic needs that differ from humans'. They benefit from structured over prose responses, suffer from context bloat in ways humans don't, need explicit state introspection to avoid confabulating about their own execution, and encounter failure modes around ambiguity and tool-use that a human operator would notice immediately. Systems built only with human developers in mind routinely fail LLMs in ways that silently degrade the quality of agent work — and therefore of the whole system's output.

**What this rules out.** Designing on assumption alone. When a tool's shape or a subsystem's interface might affect how an LLM uses it, asking the LLM is not optional extra polish — it is primary requirements-gathering. Observing how agents actually use a surface, and treating that observation as a design signal, is a first-class activity.

**What this demands in practice.**
- Tool designs are evaluated against LLM-usability criteria (structured outputs, predictable failure modes, minimum ceremony, absence of context bloat), not only against human-readability.
- Feedback channels that an LLM can use productively exist — structured errors, introspection, explicit uncertainty, confidence signals.
- Documentation is as legible to an LLM reader as to a human reader. Code examples are complete and unambiguous; tables and structured formats are preferred over long prose where structure would help.

**Practice: co-design sessions.** The direct consequence of this principle is a specific development practice: design work that affects LLM-facing surfaces is conducted as a co-design session between a human collaborator and an LLM, with both participants' contributions treated as primary material — not as a spec drafted in isolation with LLM review requested after the fact. This is a first-class practice in how factor-q is built, documented in [CONTRIBUTING.md § Design sessions](../../../CONTRIBUTING.md#design-sessions). Collaborators joining the project are expected to continue it.

### 2. No confabulation where data exists

Any information the runtime tracks internally — budgets, context usage, tool versions, sandbox boundaries, handle state, spawn lineage, graph instance progress — that an agent (or any caller) might reasonably be asked about must be exposed through a tool, not left to inference.

The failure mode of withholding such data is not that the agent answers "I don't know." The agent will produce a plausible-sounding answer, inferred from visible conversation length and pattern-matched from training data, indistinguishable from knowledge to a casual reader. These answers pass sniff tests often enough that nobody notices they're invented. Inference fills the gap whether the harness permits it or not. The harness must not allow the gap to exist.

This principle is a direct corollary of treating LLMs as first-class users: an agent that confabulates about its own state is not being served by the system, it is being set up to produce subtly wrong answers. And the fix is near-free — the harness is already tracking every field worth exposing for its own purposes (budget enforcement, sandbox dispatch, event-bus correlation). Exposure is a read path over existing state, not new instrumentation.

**What this rules out.** Any design where "the agent can't know about X" is the default. If the runtime knows X, the agent should be able to ask. Exceptions require a specific, stated reason (usually security — the agent must not know another agent's credentials, say).

**What this demands in practice.** Every piece of runtime-tracked state that an agent might plausibly be asked about has a corresponding read path. `SelfInspect`, `AgentList`, `AgentPeek`, `ToolVersions`, `LoadCheckpoint`, and the introspection shape of handles are all instances of this principle.

### 3. Safe by construction, not by restriction

Every agent execution runs in a context where nothing is available by default. Tools, filesystem scope, environment variables, network endpoints, and resource budgets are explicitly granted in the agent definition, not implicitly assumed from a shared environment. An allow-list defaulting to nothing fails *safe* when the system or the author missteps — an omitted grant is a clean "denied", not a silent success with wider-than-intended reach; a deny-list defaulting to everything fails *open*.

The deeper commitment is in *how* the boundary holds. Restriction hands the agent broad, ambient authority — a real shell, a real filesystem, raw sockets — and adds checks to contain it; safety then equals the completeness of the checks, and every gap is a leak. (A phase-1 shell with a path allow-list still let an agent read `~/.config` via `cat`, because the allow-list guarded the *file tool*, not the subprocess it could spawn.) Construction gives the agent no ambient authority at all: its world is assembled from a small set of granted capabilities, each structurally incapable of the unsafe act. The unsafe thing is not *denied* — it is *unreachable, because it does not exist in the agent's world*. A path allow-list over a real filesystem is a restriction; a virtual filesystem whose only mount **is** the workspace is construction — there is no host path to escape to.

Construction is stronger than even a fail-safe restriction: a restriction has a surface that can be specified incompletely, while a constructed capability has nothing to leave a gap in. It shrinks what must be trusted (a few primitives, not the correctness of every check) and what must be audited — "what can this agent reach" is a finite list; "what can it *not* escape to" is unbounded. It is the through-line under decisions that are not obviously about sandboxing: the [harness is a pure function](../../adrs/accepted/0014-agent-harness-as-reducer.md) (it cannot execute, so it is not an attack surface); the agent's authority *is* its declared-tools list (there is no shell to restrict); tools are [typed narrow operations, not free-form APIs](../../adrs/accepted/0016-typed-operations-no-free-form-apis.md) (a Git tool that can only fetch-known / push-predetermined cannot do arbitrary git). [ADR-0028](../../adrs/accepted/0028-tool-scoped-isolation-and-workspace.md) is the fullest application — a harness-owned virtual filesystem and per-tool capability tiers.

**What this rules out.** Any design where capability is granted globally and removed selectively — global defaults, ambient credentials, an agent-visible general shell, "just make it work everywhere" tooling. And enforcement that leans on a check being complete where the reality could instead be *constructed so the check is unnecessary*.

**What this demands in practice.** Agent definitions declare exact scopes; the executor provisions exactly what is declared and nothing more. Prefer constructing the agent's world — a scoped virtual filesystem, a narrow typed tool, a WASM guest with only the capabilities granted — over fencing a broad one. The one residual is a real host process (a native build binary) carrying OS-level ambient authority no in-process construction removes; there, construction is supplemented by an OS boundary, and the standing move is to shrink that residual (port it into a constructed tier) rather than grow the fences around it. Failures that hit a boundary are reported as structured errors the agent can adapt to, never silent.

### 4. Cost is a first-order safety concern

Autonomous agents spending money without human oversight is a risk category co-equal with destroying data, leaking credentials, or corrupting shared state. Budget tracking, per-agent limits, aggregate ceilings, and cost-aware scheduling are runtime-level primitives, not observability features layered on after an incident.

**What this rules out.** Designs where cost is an afterthought — something to be measured and reported rather than enforced. It also rules out "eventually consistent" cost accounting: an agent that has just exceeded its budget must not be permitted another LLM call on the assumption that budget accounting will catch up in a moment.

**What this demands in practice.** Cost appears in event schemas, agent definitions, composition primitives, and CLI surfaces. Budget limits are enforceable — when hit, they halt execution. Aggregate budgets inherit down spawn trees, so a recursive fan-out cannot invisibly explode cost by staying under each child's individual ceiling while blowing the total.

### 5. The graph is the substrate for composition

Multi-agent work is modelled as a directed graph (with cycles) executed by a single engine. Higher-level compositional primitives — fan-out, review loops, map-reduce — are canonical graph shapes shipped as library fragments, not special cases in the runtime.

This keeps the number of things the runtime must be good at small. It also means that workflows are statically analysable, replayable, visualisable, and amenable to the kinds of transformation a self-improvement loop requires. A system that encodes its workflow logic in natural-language prompts is one that cannot reason about its own behaviour; a system that encodes it in graphs can.

**What this rules out.** Adding a bespoke executor for a new compositional pattern. New patterns are published as fragments in the library — if the existing graph primitive cannot express a pattern, that is a signal to extend the graph primitive, not to add a parallel execution mechanism. Natural-language prompts that smuggle workflow control flow ("first do X, then if Y, do Z") are a sign the graph layer is being used wrong.

**What this demands in practice.** Sugar tools (`AgentSpawn`, `AgentMap`, `AgentLoop`) compile to graph instances and the runtime handles them through the same executor as any other graph. The fragment library is the extension mechanism for new canonical patterns. Workflows that need dynamic branching, cycles, or multi-round convergence drop down to `AgentGraph` directly rather than encoding control flow in prompts.

### 6. The simplest thing that works, behind a verified, swappable seam

Every core capability is the simplest implementation that works, behind a well-defined interface, under a verification net that pins the interface's contract. Sophistication is deferred — it arrives later as a swap behind the same seam, not as complexity baked into the first version. Build the end-to-end experience on reference implementations first; deepen a component only once the whole loop runs.

**What this rules out.** Building the sophisticated version before the end-to-end path exists — a fast vector engine before `search` returns anything, a reranker before retrieval works, a clever compactor before the null policy is replaced. Coupling that cannot be swapped: a consumer reaching past the seam into a specific implementation, or a capability with no interface at all. And a seam without a contract — an interface not pinned by claims and a test net is not swappable in practice, because a replacement has nothing to be checked against.

**What this demands in practice.** Core capabilities sit behind traits: the event sink and injected clock, the [context policy and strategy](../aspirational/context-management.md), the extractor plugin protocol and the vector-engine boundary in the [storage foundation](../../plans/active/2026-06-27-storage-vector-foundation.md), the graph executor (Principle 5). Each ships a reference implementation first — a UTF-8 passthrough extractor, sqlite-vec, the two-stage default compactor, an in-memory event sink — and each seam carries the claims and verification the [reducer verification](../../plans/closed/2026-07-05-reducer-verification.md) work established, so a later, better implementation is validated against the same contract. Sophistication (hybrid search, rerankers, a faster engine, smarter compaction) is scheduled as a swap, never as a reason to delay the end-to-end path.

**Why this compounds — it is what makes fq-work-on-fq tractable.** A module behind a stable, tested seam is exactly what an autonomous agent can safely improve: the interface bounds the blast radius, and the verification net makes the change checkable against a contract the agent cannot silently break. This principle is therefore a precondition for the [M0 self-improvement loop](../../plans/active/2026-07-05-m0-close-the-loop.md), not merely hygiene — the discipline that keeps a human able to swap a component is the same one that lets an agent do it.

### 7. Respect known constraints proactively

Where the runtime knows a limit its environment imposes — a transport's maximum payload, a rate ceiling, a schema, a quota — it enforces against that limit at the boundary *before* acting, not by discovering it through failure. A constraint the system can read is a constraint the system is responsible for honouring; learning it from a runtime error is a defect, not bad luck.

**What this rules out.** Firing data at a transport that will reject it when the size was knowable; issuing a call that will be throttled when the budget was in hand; emitting output that violates a schema the system holds. Discovering a ceiling by crashing into it — especially when the crash is worse than the pre-empted error would have been, as when an oversized publish trips a NATS "maximum payload" violation and poisons a retry loop instead of returning a clean, attributable rejection at the publish seam.

**What this demands in practice.** Read the constraint where the environment advertises it (NATS reports `max_payload` in the server INFO at connect; providers advertise context windows and rate limits; stores declare their bounds), carry it in the runtime, and check against it at the single seam every caller passes through — so one guard protects every path and turns a silent cliff into a diagnosable error. This is the transport-and-environment cousin of Principle 3: both enforce against what is known at the boundary rather than finding out by crashing.

### 8. Tunable parameters are configuration, not code

A value tuned to adjust behaviour — an iteration cap, a cost floor, a timeout, a retry interval, a budget — belongs in configuration, changeable without editing, rebuilding, and redeploying code. A parameter you would ever want to *try a different value for* is a knob; a knob hardcoded as a constant is a defect, because every adjustment becomes a code change, a build, and a deploy when it should be an edit and a reload.

**What this rules out.** Hardcoding a tunable as a `const` and changing it by editing source — as `DEFAULT_MAX_ITERATIONS` was, bumped `20 → 100` through a code change, build, and restart. Burying a threshold, timeout, or floor where only a rebuild can reach it. And conflating a *tuning knob* (no single correct value; you set it to the workload) with a *structural invariant* (a subject scheme, a schema version, a wire format — correct-or-wrong, not tunable): the first is configuration, the second is code.

**What this demands in practice.** Tunables live in configuration with sensible defaults, overridable at the right scope — a daemon default (e.g. in `fq.toml`), a per-agent override in the [agent definition](../../adrs/accepted/0005-agent-definition-format.md) (so `fq reload` applies it with no restart), a per-invocation value where that fits. New tunables — the graph's ε cost floor and hop-ceiling, any budget floors — are configuration *from the start*, not retrofitted. The test is simple: if you would ever run the system with a different value to see what happens, it is configuration.

---

## How this doc evolves

Principles are added when they have been invoked or proposed in at least one concrete design discussion and appear generalisable beyond that single case. They are not added speculatively.

Principles are revised when they repeatedly produce friction without corresponding benefit, or when a clearly better formulation emerges. Revision is explicit — a principle that no longer represents current practice should be rewritten or removed, not silently ignored.

Where a principle first emerged from a specific design document, that document should reference the principle rather than restating it in full. This file is the canonical home.
