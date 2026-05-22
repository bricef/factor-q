# The Reducer Harness

factor-q runs agents through one of two execution paths:

| Path | Selected by | Status | What it is |
|---|---|---|---|
| **Legacy executor** | default | shipped, used in production | Async Rust function that drives the LLM/tool loop directly. |
| **Reducer harness** | `fq trigger --reducer` | prototype, behaviourally equivalent | Pure synchronous reducer (`step` function) plus a host loop. Same canonical events, suspend/resume capable. |

Both paths produce **the same canonical event sequence** (`triggered → llm.request → llm.response → cost → tool.call → tool.result → … → completed`) so downstream consumers (the SQLite projection, `fq events tail`, dashboards) cannot tell which path produced an invocation.

This guide explains:
1. [When to use the reducer path](#when-to-use-the-reducer-path)
2. [The reducer model](#the-reducer-model) (one diagram)
3. [Using `--reducer` from the CLI](#using-reducer-from-the-cli)
4. [The Rust API](#the-rust-api) (`Reducer` trait, types, examples)
5. [Suspend and resume](#suspend-and-resume)
6. [What's not yet supported](#whats-not-yet-supported)
7. [Where the code and tests live](#where-the-code-and-tests-live)

For background on **why** factor-q has this shape, see [`docs/design/wasm-boundary-design.md`](../design/wasm-boundary-design.md). For an honest critique of the design as it stood before the prototype, see [`docs/design/2026-04-19-design-assessment.md`](../design/2026-04-19-design-assessment.md). For the prototype's verification report, see [`docs/plans/closed/2026-04-25-native-reducer-prototype.md`](../plans/closed/2026-04-25-native-reducer-prototype.md).

## When to use the reducer path

| Use the reducer path when… | Reasoning |
|---|---|
| You want to **suspend an invocation** and resume it later (in the same process or another). | The reducer's state is a single opaque blob you can serialise, persist, and feed back in. |
| You want to **debug a deterministic step trace** of an agent's decisions. | `step(StepInput) -> StepOutput` is pure: same inputs always yield the same output, no hidden state, no async interleaving. |
| You're working on the **harness boundary itself** (new state-machine features, alternative reducers, WASM packaging). | The boundary is the contract; this is where it's exercised. |

| Use the legacy executor when… | Reasoning |
|---|---|
| You just want to run an agent. | The legacy path is the default for a reason: it's the simpler shape and the one that the rest of the runtime is currently built around. |
| You're testing a feature that hasn't landed in the reducer yet. | See [What's not yet supported](#whats-not-yet-supported). |

## The reducer model

```
                      ┌─────────────────────────┐
                      │ host loop (ReducerRunner)│
                      └─────────┬───────────────┘
                                │
                  build StepInput {
                    config, trigger,
                    state, last_result,
                    now_ms, random_seed,
                    step_index
                  }
                                │
                                ▼
                  ┌─────────────────────────┐
                  │ Reducer::step (pure fn) │
                  └─────────┬───────────────┘
                            │ returns StepOutput {
                            │   next_action,
                            │   state, logs, events
                            │ }
                            ▼
                  match next_action:
                    CallModel(req)        → host runs LLM, emits events
                    CallTool(req)         → host runs tool, emits events
                    CallToolsParallel(rs) → host runs tools, emits events
                    Complete(text)        → host emits `completed`, returns
                    Failed(err)           → host emits `failed`, returns
                            │
                            └────────────────────► loop with last_result set
```

**The reducer holds no state between calls.** Everything it remembers must round-trip through the opaque `state: Vec<u8>` blob. Everything it does must come back as a `NextAction` for the host to execute. Time, randomness, and external effects all enter through `StepInput`.

This shape gives factor-q five properties for the price of one mechanism:

| Property | Why it falls out |
|---|---|
| **Suspension** | The host can stop calling `step` after any iteration; the reducer doesn't notice. |
| **Migration** | Persisted state can be resumed by a different host process or even a different machine. |
| **Replay / shadow mode** | Re-running with the same `(state, last_result, now_ms, random_seed)` reproduces the same `StepOutput` bit-for-bit. |
| **Audit logging** | Every step's input/output is structured data; logging is "write the pair to disk." |
| **Determinism** | Determinism is structural, not a contract the harness has to satisfy by discipline. |

The cost is one ergonomic constraint: the reducer is **synchronous** and writes its loop as an explicit state enum rather than a stack of `await`s. See [the closing report](../plans/closed/2026-04-25-native-reducer-prototype.md) for the assessment of how that worked out in practice.

## Using `--reducer` from the CLI

```sh
# Run an agent through the reducer path
fq trigger sample-agent "Say hello." --reducer

# Same agent, default path, for comparison
fq trigger sample-agent "Say hello."
```

Both forms emit the same canonical events to NATS, materialise into the same SQLite projection, and produce the same kind of console output. The only visible difference today is one log line — `Running agent... (path: reducer)` vs `(path: legacy)` — included so the operator can tell at a glance which path ran.

The `--reducer` flag is incompatible with `--via-nats`. The trigger dispatcher inside `fq run` always uses the legacy executor for now; if you publish a trigger over NATS, that's the path it will take regardless of CLI flags.

## The Rust API

The reducer module is `fq_runtime::reducer`. Three things to know about:

### The `Reducer` trait

```rust
pub trait Reducer {
    fn step(&self, input: StepInput) -> Result<StepOutput, HarnessError>;
}
```

That's the entire trait. One method. Pure. Synchronous. No `Send + Sync` bounds in the trait itself — the host will require them when it consumes a reducer in an async loop.

### The boundary types

Defined in [`fq-runtime/src/reducer/types.rs`](../../services/fq-runtime/crates/fq-runtime/src/reducer/types.rs).

| Type | Role |
|---|---|
| `StepInput` | What the host hands to `step` on each call. |
| `StepOutput` | What `step` hands back: `NextAction` plus updated state, logs, and semantic events. |
| `NextAction` | One of: `CallModel`, `CallTool`, `CallToolsParallel`, `Complete(String)`, `Failed(HarnessError)`. |
| `CapabilityResult` | What the host puts in `last_result` on the next call: `ModelResult`, `ToolResult`, `ParallelToolResults`, `Cancelled`, `HostError`. |
| `AgentConfig` | Static-for-the-invocation config (model, system prompt, tool schemas, allowed tool names, max iterations). |
| `TriggerPayload` | Static-for-the-invocation trigger (source, subject, payload JSON). |
| `LogEntry`, `EmittedEvent` | Fire-and-forget tracing/event emission. |
| `HarnessError` | Terminal failure surfaced from the reducer (`MaxIterations`, `InternalError`). |

All boundary types derive `Serialize`/`Deserialize`. The state blob is JSON in the native prototype; future WASM packaging will put the same shapes through the component-model ABI.

### The shipped `Harness` reducer

[`fq_runtime::Harness`](../../services/fq-runtime/crates/fq-runtime/src/reducer/harness.rs) is the production `Reducer` implementation that mirrors the legacy executor's behaviour. Its persistent state is small:

```rust
struct HarnessState {
    phase: Phase,        // Initial | AwaitingModel | DispatchingTools | Done
    messages: Vec<Message>,  // conversation history
    iteration: u32,      // bounded by AgentConfig::max_iterations
}
```

Four phases, three fields. If you write your own reducer (for retries, ReAct, plan-and-execute, skill composition), it goes in another file alongside `harness.rs` and implements the same trait.

### Driving a reducer from your own code

Most callers should use [`ReducerRunner`](../../services/fq-runtime/crates/fq-runtime/src/reducer/runner.rs), which is what `fq trigger --reducer` uses. It composes the existing `LlmClient`, `ToolRegistry`, `EventBus`, and `PricingTable`:

```rust
use fq_runtime::{Harness, ReducerRunner, EventBus, PricingTable, ToolRegistry};
use fq_runtime::events::TriggerSource;
use std::sync::Arc;

let bus     = EventBus::connect("nats://localhost:4222").await?;
let pricing = Arc::new(PricingTable::load(&cache_path).await);
let tools   = Arc::new(ToolRegistry::with_builtins());
let runner  = ReducerRunner::new(bus, pricing, tools);

let outcome = runner
    .run(
        &Harness::new(),       // your Reducer impl
        &agent,                // a validated fq_runtime::Agent
        &llm,                  // any LlmClient
        TriggerSource::Manual,
        None,                  // trigger subject
        serde_json::json!({"input": "go"}),
    )
    .await?;
```

The return type is `InvocationOutcome` — same enum the legacy executor returns. Anything that already handles a legacy outcome handles a reducer outcome.

### Driving the reducer manually (no host loop)

For tests, debuggers, or a custom host, call `step` directly:

```rust
use fq_runtime::Harness;
use fq_runtime::reducer::types::{StepInput, AgentConfig, TriggerPayload, TriggerSourceKind};
use fq_runtime::Reducer;

let harness = Harness::new();

let mut state: Vec<u8>   = Vec::new();
let mut last_result      = None;

for step_index in 0.. {
    let input = StepInput {
        config:       agent_config.clone(),
        trigger:      trigger_payload.clone(),
        state,
        last_result,
        now_ms:       /* wall clock */ 0,
        random_seed:  /* host-provided */ 0,
        step_index,
    };

    let output = harness.step(input)?;
    state = output.state;

    use fq_runtime::reducer::NextAction::*;
    match output.next_action {
        Complete(text)             => { return Ok(text); }
        Failed(err)                => { return Err(err.into()); }
        CallModel(_req)            => last_result = Some(/* run llm */),
        CallTool(_req)             => last_result = Some(/* run tool */),
        CallToolsParallel(_reqs)   => last_result = Some(/* run tools */),
    }
}
```

This is the loop `ReducerRunner::run` performs internally. Writing your own is appropriate when you need fine control over how actions are executed (e.g. replay against captured `last_result` values, dry-run dispatch, instrumented testing).

## Suspend and resume

Suspension is structural in the reducer model: persist the state blob between any two `step` calls and resume by feeding it back. The reducer instance can be dropped, recreated, moved between processes — none of that affects the outcome.

```rust
let harness = Harness::new();

// Run a few steps...
let s0 = harness.step(StepInput { state: vec![], last_result: None, /* ... */ })?;
let s1 = harness.step(StepInput { state: s0.state, last_result: Some(/* model response */), /* ... */ })?;

// Persist the suspended state. This bytes blob is the entire suspendable state.
std::fs::write("invocation-snapshot.json", &s1.state)?;

// ... process exits, time passes, reboot ...

// Resume in a fresh process / fresh harness.
let snapshot     = std::fs::read("invocation-snapshot.json")?;
let new_harness  = Harness::new();   // no shared state with the previous one

let s2 = new_harness.step(StepInput {
    state: snapshot,
    last_result: Some(/* whatever capability you re-supply */),
    /* ... */
})?;
// `s2` continues exactly where the previous run left off.
```

The runtime does not yet automate this for you — there's no on-disk durable state store wired up. The boundary supports it; that's a follow-up plan item, not a missing primitive.

For a working test of this exact pattern, see `state_round_trips_across_drop_and_resume` in [`harness.rs`](../../services/fq-runtime/crates/fq-runtime/src/reducer/harness.rs).

## Host-fulfilled tools

Most tools execute as ordinary `Tool` impls registered in the
[`ToolRegistry`](../../services/fq-runtime/crates/fq-runtime/src/tools.rs):
the host hands the tool a `ToolContext` carrying the sandbox,
the tool runs, the result comes back. The reducer never sees
the difference.

Some tools need data the `ToolContext` cannot expose because it
isn't sandbox-scoped — it's invocation-scoped. The `self_inspect`
tool is the canonical example: an agent calling `self_inspect`
wants the runtime's own bookkeeping (cost so far, iterations,
configured budget). That state lives on the runner, not on the
tool. For these the runtime uses a **host-fulfilled tool**
pattern:

1. The schema lives in [`fq-tools/src/builtin/self_inspect.rs`](../../services/fq-runtime/crates/fq-tools/src/builtin/self_inspect.rs)
   so the LLM-facing schema list and the registry entry are
   consistent.
2. Both `AgentExecutor::execute_tool_call` and
   `ReducerRunner::run_tool` intercept the tool name *before*
   the registry lookup. A registry-level `execute()` call would
   hit a tripwire error — that's the host telling itself
   it forgot to intercept.
3. The actual data is synthesised by
   [`crate::introspection::synthesize_self_inspect`](../../services/fq-runtime/crates/fq-runtime/src/introspection.rs),
   which both paths call. Sharing the synthesis keeps the event
   equivalence guarantee intact: same input state → same JSON
   output → same `tool.result` event.
4. The agent sees `self_inspect` exactly like any other tool —
   one entry in its `tools:` list, ordinary `tool.call` /
   `tool.result` events, JSON output.

For an agent that uses `self_inspect`, see
[`agents/examples/self-aware.md`](../../agents/examples/self-aware.md).

## What's not yet supported

Honest enumeration. The boundary supports each of these; the prototype hasn't wired them up yet.

| Gap | Status |
|---|---|
| Concurrent parallel tool dispatch | Reducer emits `CallToolsParallel(Vec<...>)`; the runner dispatches them sequentially. One-line refactor with `futures::join_all`. |
| Durable state persistence | State blobs round-trip in tests; `fq run` doesn't checkpoint them to disk. Active design: [`docs/design/data-architecture-requirements.md`](../design/data-architecture-requirements.md). |
| `fq trigger --via-nats` reducer dispatch | The trigger dispatcher inside `fq run` always uses the legacy executor. |
| Live-LLM end-to-end smoke test | Done as of 2026-04-25 — both paths exercised against a real Anthropic call (simple completion and tool-use loop). Not yet a permanent CI test. |
| Cost-equivalence assertion | Equivalence tests assert event order. Cost numbers were observed identical on the live runs but aren't asserted in tests. |

## Where the code and tests live

| What | Path |
|---|---|
| Module entry | [`fq-runtime/src/reducer/mod.rs`](../../services/fq-runtime/crates/fq-runtime/src/reducer/mod.rs) |
| Boundary types + `Reducer` trait | [`fq-runtime/src/reducer/types.rs`](../../services/fq-runtime/crates/fq-runtime/src/reducer/types.rs) |
| `Harness` (state-enum reducer) | [`fq-runtime/src/reducer/harness.rs`](../../services/fq-runtime/crates/fq-runtime/src/reducer/harness.rs) |
| `ReducerRunner` (host loop) | [`fq-runtime/src/reducer/runner.rs`](../../services/fq-runtime/crates/fq-runtime/src/reducer/runner.rs) |
| CLI flag wiring | [`fq-cli/src/main.rs`](../../services/fq-runtime/crates/fq-cli/src/main.rs) (search `reducer:`) |
| Pure unit tests | `harness.rs` `#[cfg(test)] mod tests` — runs without NATS. |
| Equivalence tests | `runner.rs` `#[cfg(test)] mod tests` — skips silently when `FQ_NATS_URL` is unset. |

To run only the reducer tests:

```sh
cargo test -p fq-runtime --lib reducer::
```

## Cross-references

- Boundary design: [`docs/design/wasm-boundary-design.md`](../design/wasm-boundary-design.md)
- Pre-prototype design assessment: [`docs/design/2026-04-19-design-assessment.md`](../design/2026-04-19-design-assessment.md)
- Native reducer prototype closing report: [`docs/plans/closed/2026-04-25-native-reducer-prototype.md`](../plans/closed/2026-04-25-native-reducer-prototype.md)
- WASM-packaging plan (deferred — superseded by tool-isolation-model): [`docs/plans/closed/2026-04-19-wasm-harness-prototype.md`](../plans/closed/2026-04-19-wasm-harness-prototype.md)
- Design principles: [`docs/design/design-principles.md`](../design/design-principles.md)
