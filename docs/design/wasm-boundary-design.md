# WASM Host/Guest Boundary Design

## Status

Draft. Designed around the reducer model for the guest. Most
initial open questions are resolved; remaining open items are
listed at the end.

## Context

ADR-0010 accepted containers as the default isolation tier with
Kata+Firecracker as an upgrade path, and identified WASM as a
future tier worth investigating. Subsequent architectural work
(see [agent-os-architecture.md](./agent-os-architecture.md))
established that WASM isolation has the strongest theoretical
fit for factor-q's capability-based security model and enables
single-binary deployment of the entire runtime.

Given no timeline pressure and a preference for building the
right foundation once, WASM is now the **target** isolation
tier, not a future option. This document defines the boundary
between the WASM guest (the per-agent execution harness) and
the host (the factor-q runtime).

The boundary is an **ABI**. Once published it is expensive to
change, so it needs careful design up front.

## The guest is a reducer, not a long-running process

The central design choice: the guest is a **pure function**,
not a long-running async process. It exports a single `step`
function that takes the current state plus the latest
capability result and returns the next action plus updated
state. The host drives the loop, persists state between calls,
and executes the actions the guest requests.

This makes the guest stateless between calls. All invocation
state lives on the host — either in memory during active work
or on durable storage during suspension. The host can suspend
any invocation at any step boundary, move it to another node,
or resume it arbitrarily later, without the guest knowing or
caring.

Consequences:

- **Suspension is trivial.** The host persists the guest's
  opaque state blob; resume is just calling `step` with it.
- **No replay, no snapshots.** Neither deterministic replay
  nor wasmtime instance snapshots are needed.
- **No determinism requirement.** The guest is a pure function
  of its inputs by construction; determinism is a property of
  the boundary shape, not a contract the harness must satisfy.
- **Synchronous guest code.** The guest is written as
  synchronous Rust (or any WASM-targeting language). No tokio,
  no `async`/`await`, no futures runtime in the guest. The
  guest crate compiles cleanly as ordinary sequential code.
- **Host owns concurrency.** Concurrency between agents and
  between parallel tool calls within an agent is host-side.
  The guest describes intent; the host schedules execution.

This is the Elm/Redux pattern applied to agent execution:
`(state, event) -> (new-state, action)`.

## Scope: what runs where

**In the guest (WASM):**
- The `step` reducer function
- The state enum and transition logic
- Tool-use orchestration policy (iteration limits, error
  handling, parallel dispatch decisions, dynamic model
  switching)
- Skill composition logic (augmenting prompts)
- Between-step logic (context compression, ReAct,
  plan-and-execute patterns)

**In the host (native):**
- The step loop (call `step`, execute the returned action,
  call `step` again with the result, repeat)
- State persistence (between steps, across suspension)
- NATS event bus client
- SQLite projection consumer
- Trigger dispatcher
- Pricing table and cost calculation
- Tool implementations (builtin, MCP client, remote routing)
- LLM provider clients
- Agent definition parsing and registry
- The network proxy (built into the host)
- Sandbox enforcement for host-side resources
- Budget enforcement
- Observability and event synthesis
- Clock and random sources (passed into `step` as data)

The guest expresses *policy*; the host provides *mechanism*.
Everything that isn't the decision-making core of "what should
this agent do next" lives in the host.

## The boundary

### Guest export

A single function:

```
step(input: StepInput) -> Result<StepOutput, HarnessError>
```

That's it. No other exports; no imports beyond WASM allocator
primitives. The guest is fully pure: same inputs always yield
the same outputs, no hidden state, no side channels.

### StepInput

```
record StepInput {
    config: AgentConfig,                    // static for the invocation
    trigger: TriggerPayload,                // static for the invocation
    state: list<u8>,                        // opaque guest state; empty on step 0
    last-result: option<CapabilityResult>,  // outcome of previous step's action
    now-ms: u64,                            // wall-clock time at step start
    random-seed: u64,                       // fresh randomness for this step
    step-index: u32,                        // monotonic counter starting at 0
}
```

Notes:
- `config` and `trigger` are stable across an invocation.
  Passing them every step is cheap (WIT canonical ABI handles
  it) and keeps `step` pure.
- `state: list<u8>` is opaque — the guest serializes whatever
  it wants to remember. The host only stores and returns it.
- `last-result` is `None` on step 0; otherwise it carries the
  outcome of the action requested in the previous step.
- `now-ms` and `random-seed` are provided explicitly so the
  guest has no side-channel access to time or randomness.
  They're recorded in the step log, which gives us time
  provenance for audit and bit-exact reproducibility on
  replay.
- `step-index` is for invariant checking and debug output;
  the guest and host can assert it matches expectations.

### StepOutput

```
record StepOutput {
    next-action: NextAction,
    state: list<u8>,                    // updated opaque state
    logs: list<LogEntry>,               // fire-and-forget
    events: list<EmittedEvent>,         // fire-and-forget semantic events
}

variant NextAction {
    call-model(ModelRequest),
    call-tool(ToolCallRequest),
    call-tools-parallel(list<ToolCallRequest>),
    complete(string),                   // final output
    failed(HarnessError),               // terminal failure
}

variant CapabilityResult {
    model-result(ModelResponse),
    tool-result(ToolResult),
    parallel-tool-results(list<ToolResult>),
    cancelled,                          // host cancelled the action
    host-error(string),                 // host-side failure the guest should handle
}
```

Logs and events are batched per step — the guest returns a
list of each. They're fire-and-forget; the host writes them to
the tracing subscriber and event bus respectively. Keeping
them in `StepOutput` (not as imports) preserves the "guest has
zero imports" invariant.

### The loop

The host runs this loop (roughly):

```
let mut state = vec![];
let mut last_result = None;
for step_index in 0.. {
    let input = StepInput {
        config, trigger, state, last_result,
        now_ms: now(),
        random_seed: rand(),
        step_index,
    };
    let output = guest.step(input)?;
    write_logs(output.logs);
    emit_events(output.events);
    state = output.state;
    match output.next_action {
        NextAction::Complete(out) => return Ok(out),
        NextAction::Failed(err)   => return Err(err),
        action => {
            last_result = Some(execute_action(action).await);
        }
    }
}
```

At any iteration, the host may persist `state` and
`last_result` to disk, release the guest instance, and resume
later with a fresh instance and the same state. The guest
cannot tell.

## Division of responsibilities

### Model calls

**Host owns:** provider routing, API keys and auth, endpoint
resolution, transport, retry on transient errors, failover
between providers, rate limiting, pricing lookup, cost
accounting, budget enforcement, shadow-mode recording and
replay, request caching, execution (actually calling the
model and awaiting the response).

**Guest owns:** message construction, model selection (from
declared aliases), tuning parameters (temperature, top-p,
max-tokens, stop sequences), tool advertising for the call,
output format shaping, response interpretation, error-handling
strategy.

The guest never sees provider URLs, API keys, or provider
model IDs. It references models by alias. The host resolves
aliases to endpoints and credentials.

### Tool calls

**Host owns:** tool-name resolution (to builtin, MCP server,
remote host, shadow replay cache), validation that the
requested tool is in the agent's declared tool list, sandbox
enforcement for paths and exec context, structured result
serialization, event synthesis (`tool.call` / `tool.result`
events), execution (including concurrent dispatch for
`call-tools-parallel`).

**Guest owns:** deciding when to request tool calls, deciding
which tools to advertise in a given model call, interpreting
and chaining results, deciding whether to retry on error,
choosing between sequential (`call-tool`) and parallel
(`call-tools-parallel`) dispatch.

### Observability

**Host owns:** automatic synthesis of structural events
(`triggered`, `model.call`, `model.result`, `tool.call`,
`tool.result`, `completed`, `failed`). Cost and token
accounting. Duration measurement. Error classification. Every
`step` call is also a structural boundary the host logs.

**Guest owns:** emitting *semantic* events via the `events`
list in `StepOutput` — events describing what the agent is
doing logically. E.g., a `phase.start` event for "refinement".
These are opaque to the host schema; the host persists them.

The guest cannot emit structural events directly. They're
synthesized by the host from step transitions and action
execution. This prevents a compromised guest from forging
tool-call events it never made.

## Model-call design

### Naming

The interface uses `model-call` / `ModelRequest` /
`ModelResponse` rather than the `chat-*` terminology common in
assistant SDKs. Agents don't always have conversational
context, and the "chat" framing narrows the mental model
unnecessarily. A `ModelRequest` describes running a language
model over some input; the fact that the input is a sequence
of messages is incidental to the framing.

The `messages` field inside the request retains its name —
it's conventional and describes the data accurately.

### Model aliasing

Agent definitions declare named models:

```yaml
models:
  primary:
    id: claude-opus-4-7
    temperature: 0.7
    max_tokens: 4096
  fast:
    id: claude-haiku-4-5
    temperature: 0.3
```

The guest references models by alias. The host resolves alias
→ provider + endpoint + credentials + pricing. This means:
multiple models per agent are native, provider migration is
host-side, and credential scope is explicit (only declared
aliases are reachable).

### Request shape

```
record ModelRequest {
    model: string,                     // alias from agent config
    messages: list<Message>,           // including any system message

    // Tuning — all optional, override agent-def defaults
    temperature: option<f32>,
    top-p: option<f32>,
    max-tokens: option<u32>,
    stop-sequences: list<string>,

    // Tool advertising for this call
    tools: option<list<string>>,       // subset of agent's declared tools;
                                       // host rejects anything not declared
    tool-choice: ToolChoice,           // auto | any | none | specific(name)

    // Output shaping
    response-format: ResponseFormat,   // text | json | json-schema(schema)

    // Escape hatch for provider-specific knobs
    provider-options: list<tuple<string, string>>,
}
```

### Response shape

```
record ModelResponse {
    model-used: string,                // actual model the host called
                                       // (may differ if substituted)
    content: string,
    tool-calls: list<ToolCall>,
    stop-reason: StopReason,           // end-of-turn | tool-use |
                                       // max-tokens | stop-sequence
    usage: Usage,                      // input/output tokens + cost-usd
    provider-metadata: list<tuple<string, string>>,
}
```

### Parameter inheritance

Three tiers, clear precedence:

1. **Per-call override** — fields set in the `ModelRequest`.
2. **Per-alias default** — fields declared under the alias in
   the agent definition.
3. **Host-wide default** — built into the runtime.

Unset fields walk down the tiers. No merging of nested
objects.

### Model substitution

The host may transparently substitute a different model
(shadow-mode replay, cost caps, provider outage failover).
The response's `model-used` reports what actually ran. The
guest sees substitution on the next step and can react if it
cares; it cannot veto.

### Tool subsetting

`tools: option<list<string>>` lets the guest narrow which
tools it advertises per call (useful for skill-based scoping
or context-budget management). The host validates that every
name is in the agent's declared tool list; advertising
undeclared tools fails at the boundary.

This is distinct from the agent definition's static tool
list. The definition grants the *capability*; the per-call
`tools` list decides *advertising*.

## Tool-call design

### Flow

The guest is the loop *policy*, the host is the loop *driver*.
A typical invocation looks like:

```
step 0:  state=<empty>, last-result=None
         -> CallModel(request_0), state_1
step 1:  state=state_1, last-result=ModelResult(response_0)
         (response_0 contains tool calls)
         -> CallToolsParallel([t1, t2, t3]), state_2
step 2:  state=state_2, last-result=ParallelToolResults([r1, r2, r3])
         -> CallModel(request_1), state_3
step 3:  state=state_3, last-result=ModelResult(response_1)
         (stop-reason = end-of-turn)
         -> Complete("final answer")
```

Each step's inputs and outputs are fully described by
`StepInput` and `StepOutput`. No hidden state on either side.

### Why guest-orchestrated

Why doesn't the host just run a model-call-then-dispatch-tools
loop itself, skipping the guest for this pattern?

Because the tool-use loop is *policy*. Iteration limits, error
handling, between-step context compression, ReAct vs
plan-and-execute, dynamic model switching ("escalate to the
strong model if the fast one is thrashing"), skill
composition, budget-aware routing — all harness-level policy.
Pushing it into the host collapses the interesting design
space.

Self-improvement also depends on this split. A new harness
version may have a smarter loop. If the host owned the loop,
harness upgrades would require kernel changes.

### Two-layer validation

- **Guest**: validates that each `ToolCall` returned by the
  model maps to a tool the harness knows about. If the model
  hallucinates, the guest synthesizes an error tool-result,
  feeds it back on the next step, and the model corrects.
- **Host**: validates that every `ToolCallRequest` in a
  `NextAction` is in the agent's declared list. A compromised
  guest attempting an undeclared call is refused.

Both layers guard different failure modes (model
hallucination vs. compromised harness).

### Parallel tool calls

The guest expresses parallelism by returning
`CallToolsParallel(list)`. The host dispatches all requests
concurrently, collects results, returns them as
`ParallelToolResults(list)` in the next step's `last-result`.
Order in the result list matches order in the request.

No race conditions, no `select!` inside the guest, no
scheduling logic in the guest. The host decides execution
order; the guest sees a deterministic result batch.

### Tool results are uniform

The host can't tell whether a `CallTool` in a `NextAction`
was prompted by a model or issued directly by the harness for
internal orchestration. All tool calls are equivalent from the
host's perspective. This is a feature — the harness can use
tools for its own reasoning (e.g., a `context-summarize`
utility tool) without the host needing a separate capability.

## Suspension, migration, and persistence

Decided by design for harness state. Workspace state
(filesystem) is a separate concern — see
[`tool-isolation-model.md`](./tool-isolation-model.md) for
the workspace-state architecture.

The guest's `state` (opaque bytes) plus the invocation's
static inputs (`config`, `trigger`) and its step-input/output
log fully describe the **harness-level** state of an
in-flight invocation. For full invocation suspension, this
must be combined with workspace-state snapshotting (overlay
filesystems, container checkpoints, or equivalent).

With the harness-state machinery defined here, the host can:

- **Suspend** — persist `state` + `last-result`, release the
  guest instance. Resume by instantiating a fresh guest and
  calling `step` with the persisted data.
- **Migrate** — transmit `state` + context to another node;
  resume there. The guest can't tell the difference.
- **Recover** — on host crash, the step log is durable (it's
  on the event bus). Resume an orphaned invocation from any
  host.
- **Limit concurrency** — suspend running invocations at any
  step boundary to free instance slots for pending work.

This mechanism converges with several other concerns:

- **Audit logging** — the sequence of `(StepInput, StepOutput)`
  pairs for an invocation is its audit trail.
- **Shadow mode** — replay the sequence of `StepInput`s to a
  different guest binary (a new harness version) for
  comparison. No determinism-of-harness is required; each
  step is a pure function, so given the same input the new
  guest produces its own next-action and the host compares
  them.
- **Reproducible debugging** — any failing invocation can be
  reproduced offline from its step inputs. Stateful-timing
  bugs don't exist in this model.

One mechanism, six concerns:
1. Suspension & resumption
2. Node migration
3. Fault-tolerant recovery
4. Concurrency-pool control
5. Audit logging
6. Shadow-mode replay

This convergence is a strong signal the design is right.

## Budget enforcement

Enforced host-side. When the host executes a
`NextAction::CallModel`, it consults the pricing table,
charges the invocation's running total, and — if the ceiling
is hit — returns `HostError::BudgetExceeded` as the `last-
result` on the next step. The guest handles it in the next
step's state transition (typically by returning
`NextAction::Failed`).

The guest cannot escape budget enforcement: `CallModel` is
the only path to a model, and every `CallModel` is
intercepted.

MCP tool calls that wrap paid services can also be priced via
the same pricing table; cost is deducted when the host
executes the `CallTool`.

## Memory and data transfer

All structured data crosses the boundary via WIT records,
marshalled by the component-model canonical ABI. The guest
does not manage linear memory manually. For typical agent
workloads (state measured in KB, not MB), copy overhead is
negligible.

The `state` field is `list<u8>` specifically to let the guest
use its own serialization format (serde+bincode is the likely
default for a Rust harness). The host treats it as an opaque
blob — it only needs to store and return it.

## Versioning

The boundary is versioned as a WIT package:
`factorq:agent-harness@MAJOR.MINOR.PATCH`.

- **Patch**: doc changes only. No ABI impact.
- **Minor**: additive changes (new optional fields, new action
  variants). Old guests continue to work.
- **Major**: breaking changes. Host and guest must agree on
  version.

Minor versions should be rare. The goal is to land a stable
interface before widespread harness deployment and evolve it
slowly afterward.

## Decisions

Initial open questions, now resolved.

### Streaming: deferred

The harness operates on complete messages. Streaming benefits
external observers (CLI token displays, web UIs), not the
harness itself. External consumers subscribe to
`agent.model.delta` events on the bus; the boundary stays
non-streaming. Revisit only if a harness pattern emerges that
actually consumes partial content.

### Cancellation: handled by the loop, not a capability

The host drives the loop. To cancel an invocation, the host
either stops calling `step` and terminates the invocation, or
returns `CapabilityResult::Cancelled` as the next
`last-result` so the guest can clean up gracefully. No
`check-cancelled` primitive is needed; the guest has no
long-running call to interrupt.

### External events: via actions, not a recv primitive

An agent invocation is atomic from trigger to completion.
External signals required mid-run (human-in-the-loop
approval, inbound messages) are expressed as tool actions
(e.g., a `wait-for-approval` tool). The host's tool
implementation blocks until the external event arrives, then
returns its result via `last-result`. From the guest's
perspective, it's an ordinary tool call that happened to take
a long time.

The host is free to suspend the guest during this wait —
since the guest holds no in-memory state between steps,
waiting 3 seconds and waiting 3 weeks look identical at the
boundary.

### Tool argument format: JSON string, optional schema validation

Arguments and results cross as JSON strings. The model
generates JSON anyway; alternatives add marshalling without
gain.

The host *may* optionally validate arguments and results
against JSON schemas declared alongside each tool. This is a
future enhancement — additive to the wire format. Tools
declare schemas; the host validates at the boundary and
returns a structured error if invalid. Not required for v1.

### Clock and random: passed in StepInput

The host provides `now-ms` and `random-seed` explicitly as
fields in `StepInput`. The guest has no other source of time
or randomness — WASI clock and random imports are not
exposed.

Three consequences:
- The guest is provably pure (no side channels).
- Time and randomness are captured in the step log for audit
  and bit-exact reproducibility.
- Debugging against a historical invocation is deterministic:
  feed the same step inputs, get the same outputs.

### Spawn/join: tools, not first-class variants

Sub-agent orchestration is implemented as tools (`spawn`,
`join`, `fan-out`), exposed through `CallTool` /
`CallToolsParallel`. This keeps the action variants small,
makes sub-agent access explicit in the agent definition (must
declare `spawn` to use it), and lets the host track
parent/child lineage through tool implementation (where
budget inheritance and supervision policy live).

### Toolchain: wasmtime

Bytecode Alliance stewardship, full component-model and
preview2 support, Rust-native (trivial to embed in
`fq-runtime`), stable governance. Alternatives (wasmer,
stitch) offer nothing we need that wasmtime lacks.

Reconsider only if the prototype reveals a specific blocker.

## Open questions

### Debugging tractability

Empirical — covered by the prototype plan
([`docs/plans/active/2026-04-19-wasm-harness-prototype.md`](../plans/active/2026-04-19-wasm-harness-prototype.md))
with concrete evaluation criteria. Can't be settled on paper.

Worth noting: the reducer model makes debugging significantly
easier than async-in-guest would have. Because `step` is pure,
any failing invocation can be reproduced offline by feeding
its step inputs back through the same guest binary. No
stateful-instance recreation, no race-condition timing. Bug
reports reduce to "here is the state and the inputs that
failed."

### Guest SDK ergonomics

The harness is written as an explicit state machine (enum +
match dispatch on `step`). This is less ergonomic than natural
async code, though not by much for the scale of state our
harness has (~5 variants). Open question: do we provide a
guest-side library that wraps the state machine in a more
ergonomic builder/combinator API? Decide once the prototype
shows how painful (or not) raw state-machine code feels.

## Next steps

1. Write the WIT package formally:
   `factorq:agent-harness@0.1.0`.
2. Begin the prototype per the prototype plan.
3. Build host-side step-loop driver and action executors,
   wiring to the existing `LlmClient`, `ToolRegistry`,
   `EventBus`.
4. Shadow-run the WASM harness alongside the native executor
   for a release cycle to validate behavioural equivalence.
5. Switch the default over.

The boundary is now committed in shape; remaining work is
implementation.
