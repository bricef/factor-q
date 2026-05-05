# Signatures as Primitive and the Two-Level Optimization Hierarchy

## Context

This document captures a reframing of factor-q's bedrock primitive and a corresponding architecture for the self-improvement loop. The reframing dissolves an ambiguity that has been quietly present in factor-q's design — whether the agent or the schema is primary — and the optimization hierarchy that follows from it provides a tractable path to the Q200 milestone (autonomous self-improvement).

These ideas should land before the agent definition format, the graph executor, and the optimization infrastructure are built. They affect what those things *are*, not just how they are implemented.

## Decisions

### 1. The bedrock primitive is the signature, not the agent

A **signature** is a typed input schema, a typed output schema, and a high-level intent statement. It is a language-agnostic, declarative specification of a unit of work. It says nothing about how the work is performed.

An **implementation strategy** is a concrete realization of a signature: an LLM call with a compiled prompt, a deterministic function, an MCP tool invocation, an ensemble with a verifier, or a composite of these. A given signature can have multiple implementations.

A **node** is a graph element that binds a signature to an implementation strategy at execution time.

What factor-q has been calling "agents" is two distinct things conflated: the signature (the contract) and the binding (which implementation to use). The split is worth making lexically explicit in the codebase.

The justification: the *durable, definitional* element of a unit of work is its contract — what goes in, what comes out, what it means. The implementation is a tunable. Multiple implementations of the same signature can coexist; a signature might be implemented today by Sonnet with a hand-authored prompt and tomorrow by a Haiku with a compiled prompt or a Rust function, with no graph changes required.

This is the same "define-once, derive-interfaces" pattern factor-q applies elsewhere, applied one level deeper. Inter-node typed contracts (decided previously) define the interfaces between nodes. Signatures define what a node *is*, with the implementation deriving from that definition.

### 2. The intent statement is part of the signature, separate from any prompt

A signature carries a brief natural-language intent — typically a few lines — that states what the signature means semantically. This is distinct from any prompt that implements the signature.

Intent statements should be high-level. "Summarize a customer support ticket into a 2-sentence escalation decision" is an intent. A 500-word system prompt with role-play, formatting rules, and few-shot examples is a prompt — and prompts are derived artifacts.

The discipline: if intent expands to fit a specific model's quirks, it has stopped being intent and become a prompt. The signature carries the contract and the meaning; compilation produces whatever model-specific text is required to make a particular implementation behave.

### 3. Prompts are derived artifacts, not authored definitions

A prompt is one form of compilation output: text produced for a specific (signature, model, dataset) tuple, used by an LLM-backed implementation strategy. Prompts may be hand-authored as a starting point — this is a perfectly valid implementation of a signature — but the architectural commitment is that they are *implementations*, not definitions, even when written by humans.

Consequences:

- Different models receive different compiled prompts for the same signature
- Prompt evolution under self-improvement has a well-defined target: the signature is fixed; the prompt is what changes
- Swapping implementation strategies (LLM ↔ deterministic function ↔ MCP tool) requires no graph changes and no signature changes

### 4. Optimization is structured as a two-level hierarchy

The self-improvement story is split into two complementary optimization loops operating at different levels of the architecture, with cleanly separated concerns and shared interfaces.

**Node-level optimization** takes a fixed signature and improves its implementation. Search space: prompt phrasings, few-shot demonstration sets, model selection, decoding parameters, possibly implementation strategy itself. Metric: per-signature verifier `Verdict` evaluated against a per-signature test set. Optimizer: DSPy-style algorithms (MIPROv2, GEPA) or equivalent, run offline against a stable signature contract.

**Graph-level optimization** takes a fixed set of signatures and improves the topology connecting them. Search space: which signatures exist in the graph, which connect to which, where verifiers sit, where fan-out/fan-in happens, what merge strategies fan-in nodes use. Metric: end-to-end workflow verifier output evaluated against a workflow test set. Optimizer: meta-agent proposes structural changes, shadow-mode evaluates both topologies, accept on improvement.

Each level treats the other as fixed during its own optimization. Node-level treats topology as given; graph-level treats node implementations as given. The discipline is enforced naturally by signatures: signatures form the stable interface between the two layers, and signatures are what the graph-level loop manipulates and what the node-level loop optimizes for.

### 5. The two-level hierarchy is the specific structure that makes Q200 tractable

A single-loop self-improvement system tries to optimize topology, bindings, prompts, and models simultaneously. The search space is enormous, credit assignment is murky, and end-to-end evaluation is expensive. Spurious correlations are easy to find — a topology change can appear beneficial when it is actually masking a bad prompt that the new topology happens to route around.

The two-level structure addresses each of these:

- **Tractable search spaces.** Each level has a smaller, better-bounded search space than a single combined loop.
- **Clean credit assignment.** When end-to-end performance regresses, the structure indicates whether the cause is node-level (some signature's local verifier score dropped) or graph-level (all node verifiers are fine, but workflow output regressed).
- **Stable building blocks.** When the graph-level loop reasons about a topology change, it can assume the signatures it composes are already well-characterized against their local metrics. This is the specific property that turns graph-level optimization from a noisy global search into a structural search over stable primitives.
- **Mitigated dogfooding lock-in.** Node-level optimizations generalize across workflows. Even if graph-level optimization narrows toward factor-q's own development workloads, well-optimized signatures remain useful broadly. This addresses the local-optima concern that single-loop self-improvement is most vulnerable to.

### 6. Optimization sequencing: node before graph

Node-level optimization must precede graph-level optimization in the project's build order. Doing graph-level first means optimizing topology over noisy, unstable primitives — which is the failure mode that makes self-improvement loops drift rather than converge.

The implied roadmap:

- **Q20 (parity)** — runtime engineering only, no optimization loops. The runtime executes graphs of hand-authored signature implementations.
- **Q100 (consistent local advantage)** — node-level optimization active. Compiled signatures accumulate against local verifiers and outperform hand-tuned prompts on the workloads they target.
- **Q200 (autonomous self-improvement)** — graph-level optimization active, composing the stable, high-performing signatures produced by the node level.

Graph-level optimization without a stable node-level foundation is the wrong order and would produce drift rather than improvement.

### 7. Compiled signatures are durable, reusable assets

A signature optimized once produces a compiled artifact: model selection, prompt text, demonstration set, decoding parameters, version, and the verifier history that established its quality. This artifact is reusable across any graph that uses the signature.

Concretely: the node-level loop is not redoing work per workflow. It is building a library of well-tuned signatures that workflows compose from. The library effect compounds — value accumulates with usage, independent of any specific workflow.

This has a practical consequence: factor-q can demonstrate progress in months rather than years. Even before Q100, the project produces measurable, reusable improvements in the form of compiled signatures with verifier-attested quality. These artifacts are concrete, comparable, and usable in isolation — a much stronger foundation than per-workflow promises.

### 8. Robust node-level optimization to preserve graph-level optionality

A signature optimized too tightly to its current upstream distribution can degrade silently when graph topology changes. The local verifier score remains high (the local test set is unchanged), but production behavior degrades because the upstream input distribution shifted.

Mitigation: per-signature test sets are deliberately broader than the current production distribution. Signatures are optimized for robustness across plausible upstream variations, not maximum performance on the current upstream. This costs some local performance and preserves graph-level optionality.

A consequence: compiled signatures should not be frozen permanently. Periodic re-optimization triggered by upstream changes is part of normal operation, not an exceptional event. The system records both the verifier history and the upstream distribution at compilation time, so that drift can be detected and re-optimization scheduled when warranted.

## Implementation notes

### Mapping onto existing architectural decisions

The signature reframing composes cleanly with prior decisions:

- The typed input/output schemas (per inter-node contracts decision) **are** the structural component of a signature. What is missing is only the explicit intent statement.
- The annotations layer is the right home for compilation metadata: model used, compiled prompt version, optimizer's confidence, training data identifier. None of this affects downstream behavior; all of it is visible to the learning loop.
- The verifier-`Verdict` shape is the metric that node-level optimizers consume. The substrate is already in place.
- The event-sourced bus is the substrate that graph-level optimization needs to evaluate topology changes via shadow-mode replay.

### What changes in the agent/node definition format

The "agent definition" splits into two artifacts:

- **Signature definition**: input schema, output schema, intent. Stable, versioned, the interface the graph composes against.
- **Binding**: a deployment-time choice of implementation strategy for a signature, including the compiled artifact (prompt text, model, demonstrations) when relevant.

The graph definition operates on signatures. Bindings are resolved at execution time, possibly based on cost/latency/accuracy constraints, and possibly with multiple bindings active simultaneously for A/B evaluation by verifiers.

### DSPy as offline optimizer, not runtime component

DSPy is the most concrete prior art for node-level optimization (MIPROv2, GEPA). The integration is *offline*: DSPy runs as a separate Python tool that consumes signatures, training data, and the verifier-`Verdict` substrate, and produces compiled-prompt artifacts that the Rust runtime loads.

This preserves the runtime's coherence (it stays Rust-native, event-sourced, sandbox-disciplined) while accessing DSPy's optimizer machinery. The boundary is clean: DSPy operates on signatures and produces artifacts; the runtime consumes artifacts and executes them. Neither system needs to know about the other's internals.

### Some signatures are not (input → output) shaped

Not every unit of work fits a clean (input → output) signature. Open-ended research, multi-turn reasoning, dynamic tool use — these have signatures shaped like `(question) → report`, where most of the actual work is hidden behind a complex internal loop.

This is fine. The signature defines the interface; the implementation can be arbitrarily complex inside, including a full agent loop with its own state machine. Signature-as-primitive does not eliminate agent-shaped complexity — it relocates it behind a well-defined contract.

### Naming and documentation

The codebase will benefit from explicit lexical separation:

- `Signature` — input schema + output schema + intent
- `Binding` — signature + implementation strategy + compiled artifact
- `Node` — graph element bound to a signature; resolves to a binding at execution time
- `Agent` — *deprecated as a primary term*; retained where useful as a synonym for "LLM-backed binding," but no longer the primitive of the architecture

The vision and architecture documents should be revisited to use this vocabulary. The shift is small in code volume but large in conceptual clarity.

## Open questions

- **Signature versioning and evolution.** Signatures will change. The mechanics for backward-compatible signature evolution (analogous to schema versioning in the inter-node contracts decision) need their own pass. Compiled artifacts are tied to specific signature versions; what happens when a signature version is bumped is a real design question.
- **Cross-signature bindings.** A binding of the form "use the same compiled prompt for these three similar signatures" is a useful optimization but complicates the model. Worth deferring until evidence demands it.
- **The compilation pipeline as infrastructure.** Going from signature to compiled artifact is real engineering, not a script. DSPy provides the algorithms but not the operational story (when compilations run, how artifacts are stored and selected, how training data is curated, how verifier histories accumulate). This is a Phase 3 build but worth scoping early.
- **Test set diversity discipline.** The robustness mitigation in §8 depends on per-signature test sets being broader than current production. Who curates these, how they evolve, and how the system detects when they have become too narrow are operational questions without firm answers yet.

## Why this matters

The reframing from agent-as-primitive to signature-as-primitive is small in terms of architectural surface — most of factor-q's existing decisions compose cleanly with it. But it is large in terms of what becomes possible:

- The optimization story moves from "we will figure out self-improvement" to a concrete two-level hierarchy with tractable subproblems.
- The path to Q200 becomes visible: node-level first, accumulating durable compiled artifacts; graph-level later, composing stable primitives.
- The product positioning sharpens: factor-q is an event-sourced runtime for compiled compute graphs over typed signatures, with durable execution and self-improvement as native properties. This is a clearer story than "an agent orchestrator" and composes better with the rest of the architecture.

The framing to internalize: factor-q is not an agent framework. It is a typed dataflow runtime where signatures are the unit of meaning, implementations are tunable, and optimization is structured to factor cleanly across the levels at which decisions are made. Agents persist as one implementation strategy among several, no longer as the architecture's center of gravity.
