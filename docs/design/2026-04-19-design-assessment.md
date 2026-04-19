# Design Assessment: 2026-04-19

## Purpose

A point-in-time reflection on the agent harness and tool-
isolation architecture as it stands on 2026-04-19. Not an
evergreen design document — a snapshot meant to orient
resumption of design work, especially by a future self or
collaborator picking up the thread cold.

The substance is a critical review: what the current design
does well, what it does poorly, what implicit tradeoffs are
being made, what to do next. Deliberately honest rather than
diplomatic.

## Where we are

### Current design artefacts

Written in sequence through April 2026:

- [`wasm-boundary-design.md`](./wasm-boundary-design.md) —
  the agent harness as a reducer: the guest exports a single
  pure `step(StepInput) -> StepOutput` function with zero
  imports; the host drives the loop.
- [`tool-isolation-model.md`](./tool-isolation-model.md) —
  trust boundary moved from agent-scoped to tool-scoped;
  per-tool isolation tiers (in-process, subprocess,
  container, WASM, microVM); workspace state introduced as
  the third leg alongside harness and external state.
- [`wasm-posix-sandbox.md`](./wasm-posix-sandbox.md) —
  exploratory direction for the WASM isolation tier
  specifically for shell and file-manipulation tools
  (BusyBox-in-WASI with overlay filesystem).
- [`agent-os-architecture.md`](./agent-os-architecture.md) —
  earlier framing doc positioning factor-q as an OS for
  agents; updated to cross-reference the newer design
  documents.

One prototype plan:

- [`2026-04-19-wasm-harness-prototype.md`](../plans/active/2026-04-19-wasm-harness-prototype.md)
  — prototype plan for validating the reducer boundary.

Backlog entries added for: agent tool catalogue design,
workspace snapshotting strategy, WASM POSIX investigation.

### What has been decided

- Reducer model for the harness. Pure function of inputs. No
  hidden state. Host owns the loop.
- Per-tool isolation declared on each tool. Five tiers
  available, tool picks based on threat profile.
- Workspace state is a first-class invocation concern.
  Pre-loaded base layers make the filesystem a universal data
  interface for context.
- External state is fundamentally out of scope; tools handle
  via idempotency discipline.
- Network proxy is load-bearing for every network-touching
  tool.

### What has not been decided

- Whether WASM is actually necessary (we concluded "probably
  yes for operational reasons like hot reload,
  self-improvement, multi-version comparison" but
  acknowledged the reducer model doesn't strictly require
  it).
- The workspace snapshot mechanism (OverlayFS, CRIU, git,
  stable paths). Starting position: stable paths for
  phase 2.
- The guest SDK ergonomics question: do we provide a
  wrapper for writing state machines, or does everyone roll
  their own?
- Tool catalogue contents and naming conventions.
- Pool sizing and lifecycle for containerised tools.

## What the design gets right

Architectural properties that hold up under scrutiny, not
rationalisations after the fact.

### Reducer convergence is structural

Five concerns — suspension, migration, fault recovery,
shadow replay, audit logging — collapse into one mechanism
(the step input/output log plus the opaque state blob)
because the reducer shape fundamentally gives you this. It
is not accidental. The shape *is* the design.

### Trust-boundary relocation is honest

Many agent architectures put the sandbox around "the agent"
as a ball of vaguely-untrusted stuff, because they are
afraid to name what they trust. We moved the boundary to
where risk actually lives (tool execution) because that is
where it actually is. Prompt injection, adversarial model
outputs, untrusted MCP servers — all manifest as tool calls
with bad arguments, not as a corrupted orchestrator.

### The pre-loaded-workspace insight

Not obvious, and practically important. Making the
filesystem a universal data interface (agents accessing
domain context as files rather than through dedicated tools)
collapses tool proliferation. It matches how models are
trained — they have seen vast amounts of "navigate the
project directory, read files, understand context". The
architecture supports this naturally.

### Per-tool isolation tiers match threat topology

Paying isolation overhead proportional to threat makes the
trade-offs explicit per tool. A jq tool and a third-party
MCP server have radically different risk profiles; treating
them identically would mean either over-protecting cheap
tools or under-protecting risky ones.

## What the design gets wrong or leaves unresolved

Problems — some structural, some just missing work.

### Substantial design with zero validation

Three design docs, a prototype plan, and no code written in
any of these directions. The reducer model in particular is
asserted as elegant and tractable but has not been encoded
for any realistic agent. The state enum might stay small and
clean — or it might balloon into something gnarly once retry
logic, multi-step reasoning, error handling, and skill
composition are folded in. We do not know.

Design cycles uncoupled from validation cycles drift. This
has drifted.

### Speculative benefits driving complexity cost

The reducer's key wins — suspension, migration, shadow
replay, self-improvement — are all things we anticipate
needing. None is required for the current phase-2 scope
(single-node, personal tool, first-party harness). We may be
paying ergonomic cost (explicit state machines instead of
natural async) for benefits that never concretise.

If the prototype reveals that the state machine is painful
and the speculative benefits remain speculative, this is a
real reckoning.

### Network proxy is doing too much without a design

The network proxy now carries per-tool allowlist enforcement,
aggregate per-agent policy, credential injection, rate
limiting, shadow-replay recording, audit logging, and
caching. Every other component depends on it. It has no
dedicated design document.

Concentrating this much responsibility in one as-yet-
undesigned component is risky. The proxy needs its own
design pass before any of the other architecture can depend
on it.

### External-state handling is hand-waved

"Idempotency is your problem" is a correct statement of the
boundary but not a solution. Real agent systems have
concrete infrastructure for idempotency-key generation and
persistence, retry-safe tool-call marking, effect auditing,
and manual-intervention affordances for failed irreversible
actions. We have named the gap without filling it.

### Several critical follow-up docs do not exist

Specifically:
- Workspace snapshot design (mechanism, base-layer format,
  migration protocol)
- Network proxy design (architecture, performance, security)
- Agent tool catalogue design (what tools exist, how they
  are scoped, how they compose)
- Guest SDK design (or the decision that no SDK is needed)

Each is load-bearing. Each is in the backlog. Each should
probably precede or accompany implementation of the reducer
prototype.

## Implicit tradeoffs not named explicitly

Choices made by implication rather than deliberation. Some
are fine; some deserve revisiting.

### Ergonomic code → explicit state machine

We chose the reducer shape for its suspension/migration
properties, implicitly trading natural Rust async code for
state-enum match dispatch. We claim this is manageable. We
have not written one. The prototype will settle this.

### Platform vs product tension

The design is increasingly shaped like a platform for
running agents. But factor-q is being built as a personal
tool. These aren't strictly incompatible but they pull
differently — a platform wants generality and extensibility;
a personal tool wants opinionated simplicity.

We should be honest about which we are building. Right now
we have been designing the platform without the product
pressure to keep it simple.

### Single-node reality, multi-node design

Migration, distributed capability transparency, multi-node
fault recovery are designed in. Phase 2 is entirely
single-node. We may be designing for a shape we never
encounter.

### Tool-configuration explosion

Every tool declaration includes tier, image, resource
limits, network policy, filesystem policy, env policy,
credentials, pricing. For a handful of tools this is
manageable. For realistic agents with dozens of tools
across multiple MCP servers, this is a lot of configuration
to maintain correctly. Good defaults help, but only if the
defaults are right.

### Supply-chain and trust for extensions

User-authored tools and third-party MCP servers are
mentioned but not designed for: distribution, versioning,
signing, trust establishment, installation workflow,
revocation. These will matter the moment anyone installs a
tool they did not write. We have deferred these questions
implicitly.

### Pool management deferred as "operational"

Warm-pool sizing for container-tier tools is flagged as
operational, implying a late-stage concern. It is not —
without pooling, every container-tier tool call is ~100ms
startup, which can dominate tool-heavy agent runtime. If
the prototype runs any tool-heavy workload, pool behaviour
is a first-order design question, not an afterthought.

### Debuggability asserted, not demonstrated

The claim "reducer-mode debugging is easier because `step`
is reproducible" is true in theory. Actual debugging
involves incomplete information, flaky models, emergent
tool-interaction bugs, and production conditions. None of
that has met the architecture yet. The prototype plan's
debugging evaluation criteria are the right check.

## What to do next

A clear recommendation, not a list of options.

### Build the reducer prototype before more design

Specifically: port `AgentExecutor::run()` to a state-enum
reducer, behind a Rust trait, in a native crate. **No WASM
yet.** The reducer claim is architectural; WASM is
packaging. Validate the architecture first.

What the prototype should demonstrate or falsify:

- Whether the state enum stays small and tractable for
  realistic agents
- Whether suspension and resumption actually work end-to-end
- Whether parallel tool dispatch composes cleanly
- Whether the resulting code is maintainable (subjective
  but assessable)

If the prototype survives contact with reality, the boundary
design is validated. WASM packaging becomes the next
question. If it does not, the design needs revisiting before
any more complexity is layered on.

### Treat the design docs as hypotheses

They describe *how we currently think* the architecture
should work. They are not binding until something is built
against them. Expect revisions after the prototype.

### Hold off on further design docs

Specifically: do not write more design docs on adjacent
topics (network proxy, workspace snapshotting, tool
catalogue) until the reducer prototype has run. The act of
building will reveal which questions matter and which do
not. Designing in the absence of that feedback is expensive
guesswork.

## Meta-questions worth raising when we resume

Not the open questions the design docs already capture —
questions about how we are working.

### Are we building a platform or a product?

If a personal tool: simpler. Fewer isolation tiers, fewer
abstractions, no multi-node, no self-improvement loops.

If a platform: keep going as designed, but be honest that
the personal-tool use is a small test case for a larger
system.

Worth a deliberate answer rather than continuing to drift.

### Are we anchored on benefits we have validated?

Suspension, migration, shadow mode, self-improvement. Which
of these will actually be exercised in the next three to
six months? If none, are they the right drivers for current
complexity?

### Is the design debt growing faster than the implementation?

Right now: yes. Three design docs, zero new code in this
direction. This ratio should invert for the next work
cycle.

### What would cause us to pivot?

Specifically: if the reducer prototype reveals the state
machine is untenable, what is plan B? Probably: drop the
suspension goal, let the harness be a long-running async
function, accept that suspension is out of scope. Worth
knowing the pivot now so it is not a crisis if the
prototype forces it.

## Summary

The design is internally coherent and has real architectural
wins (reducer convergence, trust-boundary relocation,
pre-loaded workspaces, proportional isolation). It is also
substantially unvalidated, driven partly by speculative
future benefits, and accompanied by several load-bearing
concerns (network proxy, workspace snapshotting, tool
catalogue) that have not been designed yet.

The right move on resumption is not more design. It is a
prototype that validates (or refutes) the reducer claim —
which is the load-bearing one — before any further layers
are committed to.
